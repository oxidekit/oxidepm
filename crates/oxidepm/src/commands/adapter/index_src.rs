//! Fetch and verify the adapter catalog in the CLI.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use oxidepm_adapter_proto::{
    manifest::Rule, official_keys, verify_index, Index, IndexKey, VerifiedIndex, INDEX_MAX_BYTES,
};
use oxidepm_core::release_bundle::{self, ReleaseBundle};
use oxidepm_fetch::{fetch_bytes, FetchOpts};

use super::coded;

/// The signed catalogue of the official adapters, published by the release workflow on the
/// `catalogue` branch. Its signature is fetched from the same URL with `.minisig` appended.
pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/oxidekit/oxidepm/catalogue/index.json";

/// The largest signature file read.
const MAX_SIG_BYTES: u64 = 16 * 1024;

/// A fetched, signed and checked index, with the bytes exactly as they were received (they go to
/// the daemon, which checks the signature again).
#[derive(Debug, Clone)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub sig: String,
    pub verified: VerifiedIndex,
    /// The URL of the index.
    pub origin: String,
}

/// The index URL, or why there is none.
pub fn index_url() -> Result<String> {
    let url = DEFAULT_INDEX_URL.to_string();
    #[cfg(feature = "test-support")]
    let url = std::env::var("OXIDEPM_TEST_INDEX_URL").unwrap_or(url);
    if url.is_empty() {
        anyhow::bail!(
            "no index is configured in this build. Import by a pinned hash (`oxidepm adapter import <file or url> --sha256 <hex>`) \
             or from the bundle of a release (`oxidepm adapter import --bundled`)"
        );
    }
    Ok(url)
}

/// The keys an index may be signed with: the embedded ones (a test build reads
/// `OXIDEPM_TEST_INDEX_KEYS` instead, a comma separated list of minisign public keys).
pub fn trusted_keys() -> Vec<IndexKey> {
    #[cfg(feature = "test-support")]
    if let Ok(list) = std::env::var("OXIDEPM_TEST_INDEX_KEYS") {
        return list
            .split(',')
            .filter(|k| !k.trim().is_empty())
            .filter_map(|k| IndexKey::parse(k.trim()).ok())
            .collect();
    }
    official_keys()
}

pub fn fetch_opts() -> FetchOpts {
    #[allow(unused_mut)]
    let mut opts = FetchOpts::default();
    #[cfg(feature = "test-support")]
    {
        opts.allow_loopback_http = true;
    }
    opts
}

fn verify(bytes: Vec<u8>, sig: String, origin: String) -> Result<Fetched> {
    let keys = trusted_keys();
    let verified = verify_index(&bytes, &sig, &keys).map_err(|e| {
        let mut message = format!("the index from {origin}: {}", e.detail);
        if e.rule == Rule::Signature {
            message.push_str(". If the index was just published, retry shortly");
        }
        coded(e.rule.error_code(), message)
    })?;
    verified.index.check_fresh(Utc::now(), None).map_err(|e| {
        coded(
            e.rule.error_code(),
            format!("the index from {origin}: {}", e.detail),
        )
    })?;
    Ok(Fetched {
        bytes,
        sig,
        verified,
        origin,
    })
}

/// Fetch `index.json` and `index.json.minisig`, verify the signature with the embedded keys and
/// check that the index has not expired. The sequence number is compared with the last one seen
/// by the daemon, which keeps it.
pub async fn fetch_index() -> Result<Fetched> {
    let url = index_url()?;
    let opts = fetch_opts();
    let bytes = fetch_bytes(&url, INDEX_MAX_BYTES as u64, &opts)
        .await
        .with_context(|| format!("Could not download {url}"))?;
    let sig_url = format!("{url}.minisig");
    let sig = fetch_bytes(&sig_url, MAX_SIG_BYTES, &opts)
        .await
        .with_context(|| format!("Could not download {sig_url}"))?;
    let sig = String::from_utf8(sig)
        .map_err(|_| coded("E_SIGNATURE_INVALID", "the signature file is not text"))?;
    verify(bytes, sig, url)
}

/// The directory of the adapters a release carries (`share/oxidepm/adapters`).
pub fn bundle_dir() -> Result<PathBuf> {
    #[cfg(feature = "test-support")]
    if let Ok(dir) = std::env::var("OXIDEPM_TEST_BUNDLE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let exe =
        std::env::current_exe().context("Cannot determine the path of the running oxidepm")?;
    Ok(release_bundle::directory(&exe)?)
}

/// Read the release's local catalog after checking it against the bundle checksum list.
pub fn bundled_index() -> Result<(ReleaseBundle, Index)> {
    read_bundled_index(bundle_dir()?)
}

pub(super) fn read_bundled_index(dir: PathBuf) -> Result<(ReleaseBundle, Index)> {
    let bundle = ReleaseBundle::read(dir).map_err(|e| {
        coded(
            "E_ADAPTER_INVALID",
            format!("Could not read the release adapter bundle: {e}"),
        )
    })?;
    let mut bytes = Vec::new();
    bundle
        .copy_verified("index.json", INDEX_MAX_BYTES as u64, &mut bytes)
        .map_err(|e| coded("E_CHECKSUM_MISMATCH", e.to_string()))?;
    let index = Index::parse(&bytes).map_err(|e| coded(e.rule.error_code(), e.detail))?;
    Ok((bundle, index))
}
