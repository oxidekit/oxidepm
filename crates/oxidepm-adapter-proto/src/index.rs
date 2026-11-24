//! The index: one file, `index.json`, signed by a detached minisign signature that
//! the CLI checks (not here). This module parses and validates the bytes, answers which release
//! is offered for a target, and applies the freshness rules. The newest release is computed here
//! from `releases`; no field of the file says which one is latest.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::limits::INDEX_MAX_BYTES;
use crate::manifest::{invalid, Invalid, Kind, Rule};
use crate::text::{has_control, is_name, is_sha256_hex};

pub const MAX_ADAPTERS: usize = 500;
pub const MAX_RELEASES: usize = 50;
/// The client looks at the newest this many releases of an adapter.
pub const READ_RELEASES: usize = 10;
const MAX_SUMMARY: usize = 120;
const MAX_DESCRIPTION: usize = 4096;
const MAX_URL: usize = 512;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub exe_sha256: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub exe_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub file_sha256: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub released: NaiveDate,
    pub min_daemon: String,
    pub protocol: Vec<u32>,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub yank_reason: Option<String>,
    /// Artifacts by target triple.
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexAdapter {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub tags: Vec<String>,
    pub summary: String,
    #[serde(default)]
    pub description: String,
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub sequence: u64,
    pub generated: DateTime<Utc>,
    pub expires: DateTime<Utc>,
    #[serde(default)]
    pub revoked: Vec<Revoked>,
    pub adapters: Vec<IndexAdapter>,
}

/// The target triple of this build, as the index names it (an exact string match).
pub fn host_target() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        (arch, os) => format!("{arch}-{os}"),
    }
}

/// A release that can be installed on this machine.
#[derive(Debug, Clone, Copy)]
pub struct Offer<'a> {
    pub adapter: &'a IndexAdapter,
    pub release: &'a Release,
    pub artifact: &'a Artifact,
}

fn text(field: &str, s: &str, max: usize, allow_newline: bool) -> Result<(), Invalid> {
    if s.chars().count() > max {
        return invalid(
            Rule::TextLength,
            format!("{field} is longer than {max} characters"),
        );
    }
    if has_control(s, allow_newline) {
        return invalid(
            Rule::ControlCharacters,
            format!("{field} holds a control character"),
        );
    }
    Ok(())
}

/// Test builds only: `http` to the loopback interface is accepted too, so that a test can serve
/// an index from a local server.
#[cfg(feature = "test-support")]
fn loopback_http(url: &str) -> Option<&str> {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .filter(|rest| rest.starts_with([':', '/']))
        .map(|_| url)
}

#[cfg(not(feature = "test-support"))]
fn loopback_http(_url: &str) -> Option<&str> {
    None
}

fn check_url(url: &str) -> Result<(), Invalid> {
    if loopback_http(url).is_some()
        && url.len() <= MAX_URL
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Ok(());
    }
    let rest = url.strip_prefix("https://");
    let ok = rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/') && !r.starts_with('@'))
        && url.len() <= MAX_URL
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    if !ok {
        return invalid(
            Rule::IndexUrl,
            format!("{url:?} is not an https URL of at most {MAX_URL} characters"),
        );
    }
    Ok(())
}

impl Index {
    /// Parse the bytes as received (never re-serialised before the signature is checked) and
    /// validate the catalog structure and size limits. Time is not looked at here.
    pub fn parse(bytes: &[u8]) -> Result<Index, Invalid> {
        if bytes.len() > INDEX_MAX_BYTES {
            return invalid(
                Rule::Size,
                format!("the index is larger than {INDEX_MAX_BYTES} bytes"),
            );
        }
        let index: Index = serde_json::from_slice(bytes).map_err(|e| Invalid {
            rule: Rule::Parse,
            detail: e.to_string(),
        })?;
        index.validate()?;
        Ok(index)
    }

    fn validate(&self) -> Result<(), Invalid> {
        if self.schema != 1 {
            return invalid(
                Rule::IndexSchema,
                format!("index schema {} is not supported", self.schema),
            );
        }
        if self.expires <= self.generated {
            return invalid(Rule::IndexDate, "the index expires before it was generated");
        }
        if self.adapters.len() > MAX_ADAPTERS {
            return invalid(Rule::Limit, format!("more than {MAX_ADAPTERS} adapters"));
        }
        for r in &self.revoked {
            if !is_sha256_hex(&r.exe_sha256) {
                return invalid(Rule::IndexHash, "a revoked entry has no valid exe_sha256");
            }
            text("revoked reason", &r.reason, MAX_DESCRIPTION, false)?;
        }
        let mut names: Vec<&str> = Vec::new();
        for a in &self.adapters {
            if !is_name(&a.name, 2, 32, true) {
                return invalid(
                    Rule::NameCharset,
                    format!(
                        "adapter name {:?} must match ^[a-z][a-z0-9-]{{1,31}}$",
                        a.name
                    ),
                );
            }
            if names.contains(&a.name.as_str()) {
                return invalid(
                    Rule::IndexSchema,
                    format!("adapter {:?} is listed twice", a.name),
                );
            }
            names.push(&a.name);
            text("summary", &a.summary, MAX_SUMMARY, false)?;
            text("description", &a.description, MAX_DESCRIPTION, true)?;
            if a.tags.len() > 10 {
                return invalid(
                    Rule::Limit,
                    format!("adapter {:?} has more than 10 tags", a.name),
                );
            }
            for t in &a.tags {
                text("tag", t, 32, false)?;
            }
            if a.releases.len() > MAX_RELEASES {
                return invalid(
                    Rule::Limit,
                    format!("adapter {:?} has more than {MAX_RELEASES} releases", a.name),
                );
            }
            let mut versions: Vec<&str> = Vec::new();
            for r in &a.releases {
                if semver::Version::parse(&r.version).is_err() {
                    return invalid(
                        Rule::Version,
                        format!(
                            "{}: version {:?} is not a semantic version",
                            a.name, r.version
                        ),
                    );
                }
                if versions.contains(&r.version.as_str()) {
                    return invalid(
                        Rule::IndexSchema,
                        format!("{}: version {} is listed twice", a.name, r.version),
                    );
                }
                versions.push(&r.version);
                if semver::Version::parse(&r.min_daemon).is_err() {
                    return invalid(
                        Rule::MinDaemon,
                        format!(
                            "{}: min_daemon {:?} is not a semantic version",
                            a.name, r.min_daemon
                        ),
                    );
                }
                if r.protocol.is_empty() {
                    return invalid(
                        Rule::Protocol,
                        format!("{} {}: no protocol listed", a.name, r.version),
                    );
                }
                if let Some(reason) = &r.yank_reason {
                    text("yank_reason", reason, MAX_DESCRIPTION, false)?;
                }
                for (target, art) in &r.artifacts {
                    let target_ok = !target.is_empty()
                        && target.len() <= 64
                        && target
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
                    if !target_ok {
                        return invalid(
                            Rule::IndexSchema,
                            format!("{}: {target:?} is not a target triple", a.name),
                        );
                    }
                    check_url(&art.url)?;
                    if art.size == 0 || art.size > MAX_ARCHIVE_BYTES {
                        return invalid(
                            Rule::Limit,
                            format!(
                                "{} {}: archive size {} is out of range",
                                a.name, r.version, art.size
                            ),
                        );
                    }
                    if !is_sha256_hex(&art.sha256)
                        || !is_sha256_hex(&art.exe_sha256)
                        || art
                            .manifest_sha256
                            .as_deref()
                            .is_some_and(|digest| !is_sha256_hex(digest))
                        || art
                            .file_sha256
                            .values()
                            .any(|digest| !is_sha256_hex(digest))
                    {
                        return invalid(
                            Rule::IndexHash,
                            format!(
                                "{} {}: sha256 values must be 64 lowercase hex digits",
                                a.name, r.version
                            ),
                        );
                    }
                    if art.file_sha256.len() > 1000
                        || art.file_sha256.keys().any(|path| {
                            path.len() > 4095
                                || has_control(path, false)
                                || path
                                    .split('/')
                                    .any(|part| matches!(part, "" | "." | "..") || part.len() > 255)
                        })
                    {
                        return invalid(
                            Rule::IndexSchema,
                            format!("{} {}: invalid artifact file paths", a.name, r.version),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// The freshness rules: an expired index is refused, and so is one whose sequence is lower
    /// than the highest accepted before under the same signing key (`last_seen`). Both are
    /// `E_INDEX_STALE`.
    pub fn check_fresh(&self, now: DateTime<Utc>, last_seen: Option<u64>) -> Result<(), Invalid> {
        if now >= self.expires {
            return invalid(
                Rule::IndexStale,
                format!("the index expired at {}", self.expires),
            );
        }
        if let Some(last) = last_seen {
            if self.sequence < last {
                return invalid(
                    Rule::IndexStale,
                    format!(
                        "the index has sequence {}; one with {last} was accepted before",
                        self.sequence
                    ),
                );
            }
        }
        Ok(())
    }

    pub fn adapter(&self, name: &str) -> Option<&IndexAdapter> {
        self.adapters.iter().find(|a| a.name == name)
    }

    /// Is this executable on the revoked list?
    pub fn is_revoked(&self, exe_sha256: &str) -> bool {
        self.revoked
            .iter()
            .any(|r| r.exe_sha256.eq_ignore_ascii_case(exe_sha256))
    }

    /// The releases of `name` that can be installed for `target` on a daemon of `daemon_version`
    /// speaking `protocols`, newest first. Only the newest [`READ_RELEASES`] releases are looked at.
    /// A yanked release, a revoked executable, a release without an artifact for the target, a
    /// release that needs a newer daemon, and a release with no protocol in common are not offered.
    pub fn offered<'a>(
        &'a self,
        name: &str,
        target: &str,
        daemon_version: &semver::Version,
        protocols: &[u32],
    ) -> Vec<Offer<'a>> {
        let Some(adapter) = self.adapter(name) else {
            return Vec::new();
        };
        let mut releases: Vec<(semver::Version, &Release)> = adapter
            .releases
            .iter()
            .filter_map(|r| semver::Version::parse(&r.version).ok().map(|v| (v, r)))
            .collect();
        releases.sort_by(|a, b| b.0.cmp(&a.0));
        releases.truncate(READ_RELEASES);
        releases
            .into_iter()
            .filter(|(_, r)| !r.yanked)
            .filter(|(_, r)| {
                semver::Version::parse(&r.min_daemon).is_ok_and(|min| min <= *daemon_version)
            })
            .filter(|(_, r)| r.protocol.iter().any(|p| protocols.contains(p)))
            .filter_map(|(_, r)| {
                let artifact = r.artifacts.get(target)?;
                (!self.is_revoked(&artifact.exe_sha256)).then_some(Offer {
                    adapter,
                    release: r,
                    artifact,
                })
            })
            .collect()
    }

    /// The newest release that is offered, if any.
    pub fn latest_offer<'a>(
        &'a self,
        name: &str,
        target: &str,
        daemon_version: &semver::Version,
        protocols: &[u32],
    ) -> Option<Offer<'a>> {
        self.offered(name, target, daemon_version, protocols)
            .into_iter()
            .next()
    }
}
