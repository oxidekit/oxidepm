//! `fetch_verified`: download a file over https, check its size and sha256 while it is written.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{FetchError, Result};

/// Redirects followed at most (each one must be https as well).
pub const MAX_REDIRECTS: usize = 3;

/// What the caller already knows about the file: its hash and the most bytes it may have.
#[derive(Debug, Clone, Copy)]
pub struct Expect {
    pub sha256: [u8; 32],
    pub max_size: u64,
}

/// Timeouts and identity of a download.
#[derive(Debug, Clone)]
pub struct FetchOpts {
    /// Time allowed to connect (10 s).
    pub connect_timeout: Duration,
    /// Time allowed for the whole request, body included (10 minutes).
    pub total_timeout: Duration,
    pub user_agent: String,
    /// Tests only: also accept `http` for a host on the loopback interface (and for the
    /// redirects that follow it). The field does not exist in a normal build.
    #[cfg(any(test, feature = "test-support"))]
    pub allow_loopback_http: bool,
}

impl Default for FetchOpts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(600),
            user_agent: format!("oxidepm-fetch/{}", env!("CARGO_PKG_VERSION")),
            #[cfg(any(test, feature = "test-support"))]
            allow_loopback_http: false,
        }
    }
}

/// A file that arrived whole and matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub sha256: [u8; 32],
    pub size: u64,
}

/// Why a redirect was refused, written by the redirect policy and read back for the error.
#[derive(Debug)]
enum RedirectIssue {
    NotHttps(String),
    TooMany,
}

fn loopback_http_allowed(opts: &FetchOpts) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    {
        opts.allow_loopback_http
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        let _ = opts;
        false
    }
}

/// True when `url` may be requested: `https`, or (tests only) `http` to the loopback interface.
fn url_allowed(url: &reqwest::Url, loopback_http: bool) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => {
            loopback_http
                && matches!(
                    url.host_str(),
                    Some("127.0.0.1") | Some("[::1]") | Some("localhost")
                )
        }
        _ => false,
    }
}

fn client(opts: &FetchOpts) -> Result<(reqwest::Client, Arc<Mutex<Option<RedirectIssue>>>)> {
    let issue: Arc<Mutex<Option<RedirectIssue>>> = Arc::new(Mutex::new(None));
    let loopback = loopback_http_allowed(opts);
    let seen = Arc::clone(&issue);
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        // `previous` holds the URLs requested so far: the hop being decided is number len()
        if attempt.previous().len() > MAX_REDIRECTS {
            *seen.lock().unwrap() = Some(RedirectIssue::TooMany);
            return attempt.stop();
        }
        if !url_allowed(attempt.url(), loopback) {
            *seen.lock().unwrap() = Some(RedirectIssue::NotHttps(attempt.url().to_string()));
            return attempt.stop();
        }
        attempt.follow()
    });
    let client = reqwest::Client::builder()
        .user_agent(opts.user_agent.clone())
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.total_timeout)
        .redirect(policy)
        .build()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    Ok((client, issue))
}

/// Send the request and return the response whose headers passed the checks (status, length).
async fn open(url: &str, max_size: u64, opts: &FetchOpts) -> Result<reqwest::Response> {
    let parsed = reqwest::Url::parse(url).map_err(|_| FetchError::BadUrl(url.to_string()))?;
    if !url_allowed(&parsed, loopback_http_allowed(opts)) {
        return Err(FetchError::NotHttps(url.to_string()));
    }
    let (client, issue) = client(opts)?;
    let response = match client.get(parsed).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(match issue.lock().unwrap().take() {
                Some(RedirectIssue::NotHttps(to)) => FetchError::RedirectNotHttps(to),
                Some(RedirectIssue::TooMany) => FetchError::TooManyRedirects(MAX_REDIRECTS),
                None => FetchError::Network(e.to_string()),
            })
        }
    };
    // The policy stops at the refused hop and hands the 3xx back: that is a refusal, not an answer.
    if let Some(found) = issue.lock().unwrap().take() {
        return Err(match found {
            RedirectIssue::NotHttps(to) => FetchError::RedirectNotHttps(to),
            RedirectIssue::TooMany => FetchError::TooManyRedirects(MAX_REDIRECTS),
        });
    }
    if !response.status().is_success() {
        return Err(FetchError::Status(response.status().as_u16()));
    }
    if let Some(announced) = response.content_length() {
        if announced > max_size {
            return Err(FetchError::TooLargeAnnounced {
                announced,
                max: max_size,
            });
        }
    }
    Ok(response)
}

/// Make sure `dir` exists and nobody but the owner can use it. A directory that is missing is
/// created with mode 0700 (with its missing parents); one that is there must already be private.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(FetchError::Unsafe {
                    path: dir.into(),
                    reason: "is not a directory".into(),
                });
            }
            if meta.mode() & 0o077 != 0 {
                return Err(FetchError::Unsafe {
                    path: dir.into(),
                    reason: format!("must be private (mode 0700), is {:o}", meta.mode() & 0o7777),
                });
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| FetchError::io(format!("creating {}", dir.display()), e)),
        Err(e) => Err(FetchError::io(format!("looking at {}", dir.display()), e)),
    }
}

/// Download `url` into `dest`, which must not exist yet. Only an `https` URL is fetched, and so
/// is every redirect (at most [`MAX_REDIRECTS`]). A `Content-Length` above `expect.max_size`
/// is refused before the body is read, and the body is cut off the moment it exceeds the limit
/// (a lying or missing length). The file is created with mode 0600 in a private directory
/// (created 0700 when missing), hashed while it is written, compared in constant time, and
/// deleted on any failure.
pub async fn fetch_verified(
    url: &str,
    expect: &Expect,
    dest: &Path,
    opts: &FetchOpts,
) -> Result<Verified> {
    let dir = dest
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_private_dir(dir)?;
    // a refused URL or a refused length leaves nothing behind: ask before creating the file
    let response = open(url, expect.max_size, opts).await?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| FetchError::io(format!("creating {}", dest.display()), e))?;
    match receive(response, &mut file, expect).await {
        Ok(done) => Ok(done),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(dest);
            Err(e)
        }
    }
}

async fn receive(
    mut response: reqwest::Response,
    file: &mut std::fs::File,
    expect: &Expect,
) -> Result<Verified> {
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| FetchError::Network(e.to_string()))?;
        let Some(chunk) = chunk else { break };
        size += chunk.len() as u64;
        if size > expect.max_size {
            return Err(FetchError::TooLarge {
                max: expect.max_size,
            });
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| FetchError::io("writing the download", e))?;
    }
    file.sync_all()
        .map_err(|e| FetchError::io("syncing the download", e))?;
    let actual: [u8; 32] = hasher.finalize().into();
    if !bool::from(actual.ct_eq(&expect.sha256)) {
        return Err(FetchError::HashMismatch {
            expected: hex(&expect.sha256),
            actual: hex(&actual),
        });
    }
    Ok(Verified {
        sha256: actual,
        size,
    })
}

/// Download a small document into memory under the same rules (https only, redirects checked,
/// a size cap, timeouts). There is no hash to compare: the caller is reading something whose
/// integrity it checks another way (a release listing; a checksum file that names the hash of
/// the real download).
pub async fn fetch_bytes(url: &str, max_size: u64, opts: &FetchOpts) -> Result<Vec<u8>> {
    let mut response = open(url, max_size, opts).await?;
    let mut body = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| FetchError::Network(e.to_string()))?;
        let Some(chunk) = chunk else { break };
        if body.len() as u64 + chunk.len() as u64 > max_size {
            return Err(FetchError::TooLarge { max: max_size });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
