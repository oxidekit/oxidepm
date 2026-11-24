//! Checksum verification for the adapter files installed alongside a release.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const DIRECTORY: &str = "share/oxidepm/adapters";
const CHECKSUMS: &str = "SHA256SUMS";
const MAX_CHECKSUMS: u64 = 1024 * 1024;

pub fn directory(executable: &Path) -> io::Result<PathBuf> {
    let bin = executable
        .parent()
        .ok_or_else(|| invalid("the executable has no directory"))?;
    Ok(bin.join("..").join(DIRECTORY))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn safe_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// Bundles store an archive under the last component of its index URL.
pub fn archive_name(url: &str) -> io::Result<&str> {
    let name = url.rsplit('/').next().unwrap_or_default();
    if !safe_name(name) || !name.ends_with(".tar.gz") {
        return Err(invalid(format!("invalid bundled archive name {name:?}")));
    }
    Ok(name)
}

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display())))?;
    if !file.metadata()?.is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    Ok(file)
}

pub struct ReleaseBundle {
    dir: PathBuf,
    checksums: BTreeMap<String, String>,
}

impl ReleaseBundle {
    pub fn read(dir: PathBuf) -> io::Result<Self> {
        let mut input = open_regular(&dir.join(CHECKSUMS))?.take(MAX_CHECKSUMS + 1);
        let mut text = String::new();
        input.read_to_string(&mut text)?;
        if input.limit() == 0 {
            return Err(invalid("the bundle checksum list is too large"));
        }
        let mut checksums = BTreeMap::new();
        for line in text.lines() {
            let (hash, name) = line
                .split_once(' ')
                .ok_or_else(|| invalid("invalid bundle checksum line"))?;
            let name = name.strip_prefix([' ', '*']).unwrap_or_default();
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || !safe_name(name)
            {
                return Err(invalid("invalid bundle checksum line"));
            }
            if checksums
                .insert(name.to_string(), hash.to_ascii_lowercase())
                .is_some()
            {
                return Err(invalid(format!(
                    "the bundle lists more than one checksum for {name}"
                )));
            }
        }
        Ok(Self { dir, checksums })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Copy the exact bytes checked against the release list. The caller discards the copy on error.
    pub fn copy_verified(&self, name: &str, max: u64, mut out: impl Write) -> io::Result<String> {
        let expected = self
            .checksums
            .get(name)
            .ok_or_else(|| invalid(format!("{name} is missing from the bundle checksum list")))?;
        let mut file = open_regular(&self.dir.join(name))?.take(max + 1);
        let mut digest = Sha256::new();
        let mut buf = [0; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if file.limit() == 0 {
                return Err(invalid(format!("the bundled file {name} is too large")));
            }
            digest.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        let actual = format!("{:x}", digest.finalize());
        if &actual != expected {
            return Err(invalid(format!(
                "the bundled file {name} has sha256 {actual}, expected {expected}"
            )));
        }
        Ok(actual)
    }
}
