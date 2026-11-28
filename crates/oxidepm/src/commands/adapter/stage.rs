//! Download or read the adapter archive, verify it, and extract it into a private staging
//! directory. The CLI validates the manifest and hashes the executable before asking the
//! daemon to install it.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oxidepm_adapter_proto::{
    host_target, Manifest, Rule, SignedBy, ValidateCtx, MANIFEST_MAX_BYTES,
};
use oxidepm_core::release_bundle;
use oxidepm_fetch::{
    extract_tar_gz, fetch_bytes, fetch_verified, hex, Expect, ExtractLimits, FetchError,
};
use sha2::{Digest, Sha256};

use super::coded;
use super::index_src::{self, Fetched};

/// The largest archive read for a pinned URL (an adapter is a few megabytes).
const MAX_PINNED_ARCHIVE: u64 = 64 * 1024 * 1024;
/// The largest archive named by an index (also the cap `Index` enforces).
const MAX_INDEXED_ARCHIVE: u64 = 256 * 1024 * 1024;

/// Where the adapter comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A name in the signed index.
    Index(String),
    /// A name in the catalog that a release carries.
    Bundled(String),
    /// An archive in a file, pinned by its sha256.
    PinnedFile(PathBuf),
    /// An archive at a URL, pinned by its sha256.
    PinnedUrl(String),
}

/// How much the adapter is trusted and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// The archive is covered by the index signature of this key.
    Signed { key_id: String },
    /// The archive matches the installed bundle's checksum list.
    Bundled { signature: Result<SignedBy, String> },
    /// The user pinned the sha256 of the archive.
    Pinned,
}

/// An adapter that has been fetched, verified and extracted.
pub struct Staged {
    /// The extracted tree.
    pub dir: PathBuf,
    /// Deletes the staging directory when dropped (unless the daemon moved it already).
    pub _guard: Option<tempfile::TempDir>,
    pub manifest: Manifest,
    pub exe_sha256: String,
    pub archive_sha256: String,
    pub trust: Trust,
    /// The index the adapter was found in, as received.
    pub index: Option<Fetched>,
    /// The line shown as `Source`.
    pub source_line: String,
    /// The `source` column for the daemon: `index`, `bundled` or `pinned:<origin>`.
    pub source_tag: String,
}

/// What `stage` needs besides the source.
pub struct StageOpts<'a> {
    /// `--sha256` for a pinned archive.
    pub sha256: Option<&'a str>,
    pub allow_reserved_name: bool,
    /// Where to make the staging directory: `adapters/staging` for a real import, the system's
    /// temporary directory for a rehearsal (no adapter directory is touched).
    pub staging_parent: Option<&'a Path>,
    /// The names and verbs the core reserves, and the verbs of the CLI.
    pub reserved: &'a super::Reserved,
    /// Record the index for a real install. A rehearsal checks it without recording it.
    pub record_index: bool,
    /// Install this version of an indexed adapter instead of the newest one.
    pub version: Option<&'a str>,
    /// An update: stop with [`UpToDate`] if the newest release is not newer than this version.
    pub only_newer_than: Option<&'a str>,
}

/// The catalog offers nothing newer than what is installed.
#[derive(Debug)]
pub struct UpToDate {
    pub name: String,
    pub installed: String,
    pub newest: Option<String>,
}

impl std::fmt::Display for UpToDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is up to date: {} is installed",
            self.name, self.installed
        )?;
        match &self.newest {
            Some(version) => write!(f, " and the newest release is {version}"),
            None => write!(f, " and the bundle offers no compatible release"),
        }
    }
}

impl std::error::Error for UpToDate {}

pub(super) fn is_newer(offered: &str, installed: &str) -> bool {
    match (
        semver::Version::parse(offered),
        semver::Version::parse(installed),
    ) {
        (Ok(new), Ok(old)) => new > old,
        _ => offered != installed,
    }
}

fn daemon_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn fetch_error(what: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::HashMismatch { expected, actual } => coded(
            "E_CHECKSUM_MISMATCH",
            format!(
                "{what} does not match its published sha256: expected {expected}, got {actual}"
            ),
        ),
        other => anyhow::Error::new(other).context(format!("Could not download {what}")),
    }
}

/// A private (0700) temporary directory, deleted when dropped (`tempfile` leaves the mode to the
/// umask).
fn private_tempdir(parent: Option<&Path>) -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let mut builder = tempfile::Builder::new();
    builder.prefix("oxidepm-import-");
    let dir = match parent {
        Some(dir) => builder.tempdir_in(dir),
        None => builder.tempdir(),
    }
    .context("Cannot make a staging directory")?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .context("Cannot make the staging directory private")?;
    Ok(dir)
}

fn make_staging(parent: Option<&Path>) -> Result<tempfile::TempDir> {
    private_tempdir(parent)
}

fn read_manifest_bytes(dir: &Path) -> Result<Vec<u8>> {
    let path = dir.join("manifest.json");
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| coded("E_ADAPTER_INVALID", "the archive has no manifest.json"))?;
    if !meta.is_file() {
        return Err(coded(
            "E_ADAPTER_INVALID",
            "manifest.json is not a regular file",
        ));
    }
    if meta.len() > MANIFEST_MAX_BYTES as u64 {
        return Err(coded(
            "E_ADAPTER_INVALID",
            format!("manifest.json is larger than {MANIFEST_MAX_BYTES} bytes"),
        ));
    }
    Ok(std::fs::read(path)?)
}

/// Extract `archive` into a fresh staging directory and validate what is in it.
fn open_archive(
    archive: &Path,
    opts: &StageOpts<'_>,
    trust: &Trust,
) -> Result<(tempfile::TempDir, Manifest, String)> {
    let staging = make_staging(opts.staging_parent)?;
    extract_tar_gz(archive, staging.path(), &ExtractLimits::default()).map_err(|e| {
        coded(
            "E_ADAPTER_INVALID",
            format!("the archive is not acceptable: {e}"),
        )
    })?;
    let bytes = read_manifest_bytes(staging.path())?;
    let manifest = Manifest::parse(&bytes).map_err(|e| coded(e.rule.error_code(), e.detail))?;
    let mut ctx = ValidateCtx::new(daemon_version());
    ctx.signed_by_embedded_key = matches!(
        trust,
        Trust::Signed { .. } | Trust::Bundled { signature: Ok(_) }
    );
    ctx.allow_reserved_name = opts.allow_reserved_name;
    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    let (adapters, runtimes, verbs, builtin) = (
        strs(&opts.reserved.adapters),
        strs(&opts.reserved.runtimes),
        strs(&opts.reserved.verbs),
        strs(&opts.reserved.builtin_verbs),
    );
    ctx.reserved_adapters = &adapters;
    ctx.reserved_runtimes = &runtimes;
    ctx.reserved_verbs = &verbs;
    ctx.builtin_verbs = &builtin;
    manifest
        .validate(&ctx)
        .map_err(|mut e| {
            if let (Rule::ReservedName, Trust::Bundled { signature: Err(reason) }) =
                (e.rule, trust)
            {
                e.detail.push_str(&format!(
                    ". Could not verify the bundle signature: {reason}. Use --allow-reserved-name to import explicitly"
                ));
            }
            coded(e.rule.error_code(), e.detail)
        })?;
    manifest
        .check_exec_file(staging.path())
        .map_err(|e| coded(e.rule.error_code(), e.detail))?;
    let exe_sha256 = sha256_file(&staging.path().join(&manifest.exec))?;
    Ok((staging, manifest, exe_sha256))
}

/// Stage an adapter that the index names.
async fn stage_indexed(name: &str, bundled: bool, opts: &StageOpts<'_>) -> Result<Staged> {
    let local = if bundled {
        Some(index_src::bundled_index()?)
    } else {
        None
    };
    let fetched = if bundled {
        None
    } else {
        let fetched = index_src::fetch_index().await?;
        super::ops::index_update(&fetched, opts.record_index).await?;
        Some(fetched)
    };
    let index = match &local {
        Some((_, index)) => index,
        None => {
            &fetched
                .as_ref()
                .expect("fetched a remote index")
                .verified
                .index
        }
    };
    if index.adapter(name).is_none() && (!bundled || opts.only_newer_than.is_none()) {
        return Err(coded("E_ADAPTER_MISSING", format!("the index has no adapter named '{name}'. `oxidepm adapter search` lists what it has")));
    }
    let target = host_target();
    let offers = index.offered(
        name,
        &target,
        &daemon_version(),
        oxidepm_adapter_proto::SUPPORTED_PROTOCOLS,
    );
    let chosen = match opts.version {
        Some(wanted) => {
            let found = offers.iter().copied().find(|o| o.release.version == wanted);
            if found.is_none() {
                let have: Vec<&str> = offers.iter().map(|o| o.release.version.as_str()).collect();
                return Err(coded("E_ADAPTER_MISSING", format!("the index offers no version {wanted} of '{name}' for this machine; it offers: {}", if have.is_empty() { "none".to_string() } else { have.join(", ") })));
            }
            found
        }
        None => offers.first().copied(),
    };
    if let (Some(installed), None) = (opts.only_newer_than, opts.version) {
        let newest = chosen.map(|offer| offer.release.version.as_str());
        if newest.map_or(bundled, |version| !is_newer(version, installed)) {
            return Err(anyhow::Error::new(UpToDate {
                name: name.to_string(),
                installed: installed.to_string(),
                newest: newest.map(str::to_string),
            }));
        }
    }
    let Some(offer) = chosen else {
        return Err(coded(
            "E_ADAPTER_MISSING",
            format!("no release of '{name}' can be installed here (target {target}, daemon {}): all are yanked, revoked, need a newer daemon or have no archive for this target", daemon_version()),
        ));
    };
    let artifact = offer.artifact.clone();
    let version = offer.release.version.clone();

    let work = private_tempdir(None)?;
    let archive = work.path().join("adapter.tar.gz");
    let expect_sha = parse_hex32(&artifact.sha256).ok_or_else(|| {
        coded(
            "E_ADAPTER_INVALID",
            "the index holds no valid sha256 for the archive",
        )
    })?;
    let expect = Expect {
        sha256: expect_sha,
        max_size: artifact.size.min(MAX_INDEXED_ARCHIVE),
    };
    let (trust, source_line, source_tag) = if let Some((bundle, _)) = &local {
        let name = release_bundle::archive_name(&artifact.url)?;
        let actual = bundle
            .copy_verified(name, expect.max_size, std::fs::File::create(&archive)?)
            .map_err(|e| coded("E_CHECKSUM_MISMATCH", e.to_string()))?;
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(coded(
                "E_CHECKSUM_MISMATCH",
                format!("the bundle catalog has a different sha256 for {name}"),
            ));
        }
        (
            Trust::Bundled {
                signature: index_src::verify_bundle_signature(bundle, &target),
            },
            format!("bundle {}", bundle.path().display()),
            "bundled",
        )
    } else {
        fetch_verified(&artifact.url, &expect, &archive, &index_src::fetch_opts())
            .await
            .map_err(|e| {
                fetch_error(
                    &format!(
                        "the archive for '{name}' {version} ({target}) at {}",
                        artifact.url
                    ),
                    e,
                )
            })?;
        let fetched = fetched.as_ref().expect("fetched a remote index");
        (
            Trust::Signed {
                key_id: fetched.verified.signed_by.key_id.clone(),
            },
            format!("index {} (sequence {})", fetched.origin, index.sequence),
            "index",
        )
    };

    let (staging, manifest, exe_sha256) = open_archive(&archive, opts, &trust)?;
    if manifest.name != name || manifest.version != version {
        return Err(coded(
            "E_ADAPTER_INVALID",
            format!(
                "the archive holds {} {}, not {} {} that the index lists",
                manifest.name, manifest.version, name, version
            ),
        ));
    }
    if exe_sha256 != artifact.exe_sha256 {
        return Err(coded(
            "E_CHECKSUM_MISMATCH",
            format!(
                "the executable has sha256 {exe_sha256}; the catalog says {}",
                artifact.exe_sha256
            ),
        ));
    }
    Ok(Staged {
        dir: staging.path().to_path_buf(),
        _guard: Some(staging),
        manifest,
        exe_sha256,
        archive_sha256: artifact.sha256.clone(),
        trust,
        index: fetched,
        source_line,
        source_tag: source_tag.to_string(),
    })
}

/// Ask the human to type the first 12 digits of the hash (on a terminal only).
fn confirm_hash_typed(actual: &str) -> Result<bool> {
    if !atty::is(atty::Stream::Stdin) || !atty::is(atty::Stream::Stdout) {
        return Ok(false);
    }
    let typed: String = dialoguer::Input::new()
        .with_prompt("To install this archive, type the first 12 hex digits of its sha256")
        .allow_empty(true)
        .interact_text()?;
    Ok(typed.trim().eq_ignore_ascii_case(&actual[..12]))
}

/// Stage an archive that the user pinned by hash.
async fn stage_pinned(source: &Source, opts: &StageOpts<'_>) -> Result<Staged> {
    let work = private_tempdir(None)?;
    let archive = work.path().join("adapter.tar.gz");
    let origin = match source {
        Source::PinnedFile(path) => {
            let meta = std::fs::metadata(path)
                .with_context(|| format!("Cannot read {}", path.display()))?;
            if !meta.is_file() || meta.len() > MAX_PINNED_ARCHIVE {
                anyhow::bail!(
                    "{} is not a file of at most {MAX_PINNED_ARCHIVE} bytes",
                    path.display()
                );
            }
            std::fs::copy(path, &archive)
                .with_context(|| format!("Cannot read {}", path.display()))?;
            path.display().to_string()
        }
        Source::PinnedUrl(url) => {
            let bytes = fetch_bytes(url, MAX_PINNED_ARCHIVE, &index_src::fetch_opts())
                .await
                .map_err(|e| fetch_error(url, e))?;
            std::fs::write(&archive, bytes).context("Cannot write the downloaded archive")?;
            url.clone()
        }
        _ => unreachable!("only pinned sources come here"),
    };
    let actual = sha256_file(&archive)?;
    match opts.sha256.map(|s| s.trim().to_ascii_lowercase()) {
        Some(given) if given == actual => {}
        Some(given) => {
            return Err(coded(
                "E_CHECKSUM_MISMATCH",
                format!("the archive has sha256 {actual}, not the {given} you pinned"),
            ));
        }
        None => {
            if !confirm_hash_typed(&actual)? {
                return Err(anyhow::Error::new(super::Refusal(format!(
                    "{origin} has sha256 {actual}. Nothing was installed. If you trust that archive, run the same command again with --sha256 {actual}"
                ))));
            }
        }
    }
    let (staging, manifest, exe_sha256) = open_archive(&archive, opts, &Trust::Pinned)?;
    Ok(Staged {
        dir: staging.path().to_path_buf(),
        _guard: Some(staging),
        manifest,
        exe_sha256,
        archive_sha256: actual.clone(),
        trust: Trust::Pinned,
        index: None,
        source_line: format!("{origin} (pinned by you)"),
        source_tag: format!("pinned:{origin}"),
    })
}

/// Fetch, verify, extract and validate.
pub async fn stage(source: &Source, opts: &StageOpts<'_>) -> Result<Staged> {
    match source {
        Source::Index(name) => stage_indexed(name, false, opts).await,
        Source::Bundled(name) => stage_indexed(name, true, opts).await,
        Source::PinnedFile(_) | Source::PinnedUrl(_) => stage_pinned(source, opts).await,
    }
}

impl Staged {
    /// The trust line of the preview.
    pub fn trust_line(&self) -> String {
        match &self.trust {
            Trust::Signed { key_id } => format!(
                "signed by key {key_id} (official). Archive sha256 {} verified. Program sha256 {}",
                self.archive_sha256, self.exe_sha256
            ),
            Trust::Bundled { signature } => {
                let status = match signature {
                    Ok(signed) => format!("Bundle signature verified with key {}", signed.key_id),
                    Err(_) => "Bundle signature unverified".to_string(),
                };
                format!(
                    "bundled with this release. Archive sha256 {} verified. Program sha256 {}. {status}",
                    self.archive_sha256, self.exe_sha256
                )
            }
            Trust::Pinned => format!(
                "pinned by you: sha256 {}. Program sha256 {}. No signature",
                self.archive_sha256, self.exe_sha256
            ),
        }
    }

    pub fn fingerprint(&self) -> String {
        self.manifest.fingerprint(&self.exe_sha256)
    }
}
