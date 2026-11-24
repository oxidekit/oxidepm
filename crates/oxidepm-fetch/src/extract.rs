//! `extract_tar_gz`: unpack an archive that came from the network without trusting it.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tar::EntryType;

use crate::error::{FetchError, Result};

/// What an archive may hold.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// Entries of any kind (1000).
    pub max_entries: usize,
    /// Bytes of one file (128 MiB).
    pub max_file: u64,
    /// Bytes of all files together (256 MiB), counted while they are written.
    pub max_total: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            max_file: 128 * 1024 * 1024,
            max_total: 256 * 1024 * 1024,
        }
    }
}

/// Room for the tar headers, padding and extended headers on top of the file bytes.
const HEADER_ALLOWANCE: u64 = 8 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4095;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_COMPONENTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractedKind {
    File,
    Dir,
}

/// One thing that was created, with the path relative to the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub path: PathBuf,
    pub kind: ExtractedKind,
    pub size: u64,
    /// The mode it was given: directories 0700, files 0500 (owner-execute in the archive) or 0400.
    pub mode: u32,
}

fn bad(reason: impl Into<String>) -> FetchError {
    FetchError::BadArchive(reason.into())
}

/// Stops a decompression that produces more bytes than the allowance: a bomb that hides in
/// padding or in extended headers is not covered by the per-file counters.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the archive expands to more than the limit",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Check one entry name and return its components. Absolute paths, `..`, empty components and
/// control characters are errors; a leading `./` is dropped. `None` is the archive root itself.
fn components(raw: &[u8]) -> Result<Option<Vec<&str>>> {
    if raw.len() > MAX_PATH_BYTES {
        return Err(bad("an entry name has more bytes than the limit"));
    }
    let text = std::str::from_utf8(raw).map_err(|_| bad("an entry name is not valid UTF-8"))?;
    let shown: String = text
        .chars()
        .take(80)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if text.starts_with('/') {
        return Err(bad(format!("absolute path {shown:?}")));
    }
    if text.chars().any(|c| c.is_control()) {
        return Err(bad(format!(
            "control character in the entry name {shown:?}"
        )));
    }
    let trimmed = text.strip_suffix('/').unwrap_or(text);
    let mut parts = Vec::new();
    for (i, part) in trimmed.split('/').enumerate() {
        if i >= MAX_COMPONENTS {
            return Err(bad(format!("too many path components in {shown:?}")));
        }
        if parts.is_empty() && part == "." {
            continue;
        }
        if part.len() > MAX_COMPONENT_BYTES {
            return Err(bad(format!(
                "path component exceeds {MAX_COMPONENT_BYTES} bytes in {shown:?}"
            )));
        }
        match part {
            "" if text.is_empty() => return Ok(None),
            "" => return Err(bad(format!("empty path component in {shown:?}"))),
            ".." => return Err(bad(format!("parent directory (..) in {shown:?}"))),
            "." => return Err(bad(format!("'.' inside the path {shown:?}"))),
            _ => parts.push(part),
        }
    }
    Ok((!parts.is_empty()).then_some(parts))
}

fn io_err(context: &str, path: &Path, e: io::Error) -> FetchError {
    FetchError::io(format!("{context} {}", path.display()), e)
}

/// Unpack the gzip-compressed tar `archive` into `dest`, which must be an empty directory.
///
/// Only regular files and directories are created: symbolic and hard links, devices, FIFOs and
/// everything else are errors. A name may not be absolute, hold `..` or an empty component, or
/// repeat an earlier one (also when the names differ only by case). The limits of
/// [`ExtractLimits`] are checked while writing, so a compression bomb is cut off. Modes are
/// masked: directories 0700, files 0500 when the archive had the owner-execute bit and 0400
/// otherwise (a directory without the write bit could not be moved or deleted); ownership is
/// ignored. On any error everything that was written into `dest` is removed again.
pub fn extract_tar_gz(
    archive: &Path,
    dest: &Path,
    limits: &ExtractLimits,
) -> Result<Vec<Extracted>> {
    let meta = fs::symlink_metadata(dest).map_err(|e| io_err("looking at", dest, e))?;
    if !meta.is_dir() {
        return Err(FetchError::Unsafe {
            path: dest.into(),
            reason: "is not a directory".into(),
        });
    }
    let empty = fs::read_dir(dest)
        .map_err(|e| io_err("reading", dest, e))?
        .next()
        .is_none();
    if !empty {
        return Err(FetchError::Unsafe {
            path: dest.into(),
            reason: "must be an empty directory".into(),
        });
    }
    let result = extract_into(archive, dest, limits);
    if result.is_err() {
        clear(dest);
    }
    result
}

/// Remove what an aborted extraction left in `dest` (directories may be read-only by then).
fn clear(dest: &Path) {
    fn open_up(dir: &Path) {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    open_up(&entry.path());
                }
            }
        }
    }
    open_up(dest);
    if let Ok(entries) = fs::read_dir(dest) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

pub(crate) fn extract_into(
    archive: &Path,
    dest: &Path,
    limits: &ExtractLimits,
) -> Result<Vec<Extracted>> {
    let file = File::open(archive).map_err(|e| io_err("opening", archive, e))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let capped = Capped {
        inner: decoder,
        left: limits.max_total.saturating_add(HEADER_ALLOWANCE),
    };
    let mut tar = tar::Archive::new(capped);
    let entries = tar
        .entries()
        .map_err(|e| bad(format!("cannot read the archive: {e}")))?
        .raw(true);

    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_folded: HashSet<String> = HashSet::new();
    let mut out: Vec<Extracted> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut total: u64 = 0;
    let mut count = 0usize;
    let mut long_name: Option<Vec<u8>> = None;

    for entry in entries {
        let mut entry = entry.map_err(|e| bad(format!("cannot read an entry: {e}")))?;
        if entry.header().entry_type() == EntryType::GNULongName {
            if long_name.is_some() {
                return Err(bad("two extended names precede the same entry"));
            }
            if entry.size() > MAX_PATH_BYTES as u64 + 1 {
                return Err(bad(
                    "an extended name is more than the limit for path bytes",
                ));
            }
            let mut name = Vec::new();
            entry
                .read_to_end(&mut name)
                .map_err(|e| bad(format!("cannot read an extended name: {e}")))?;
            if name.last() == Some(&0) {
                name.pop();
            }
            long_name = Some(name);
            continue;
        }
        count += 1;
        if count > limits.max_entries {
            return Err(bad(format!("more than {} entries", limits.max_entries)));
        }
        let kind = match entry.header().entry_type() {
            EntryType::Regular => ExtractedKind::File,
            EntryType::Directory => ExtractedKind::Dir,
            other => {
                let what = match other {
                    EntryType::Symlink => "a symbolic link",
                    EntryType::Link => "a hard link",
                    EntryType::Char | EntryType::Block => "a device",
                    EntryType::Fifo => "a FIFO",
                    _ => "an entry of a kind that is not a plain file or directory",
                };
                return Err(bad(format!(
                    "the archive holds {what}; only files and directories are accepted"
                )));
            }
        };
        let raw = long_name
            .take()
            .map_or_else(|| entry.path_bytes(), std::borrow::Cow::Owned);
        let Some(parts) = components(&raw)? else {
            if kind == ExtractedKind::Dir {
                continue; // "./" as the root of the archive
            }
            return Err(bad("a file without a name"));
        };
        let relative: PathBuf = parts.iter().collect();
        let joined = parts.join("/");
        if !seen.insert(joined.clone()) {
            return Err(bad(format!("duplicate entry {joined:?}")));
        }
        if !seen_folded.insert(joined.to_lowercase()) {
            return Err(bad(format!(
                "entry {joined:?} differs from an earlier one only by case"
            )));
        }
        let target = dest.join(&relative);
        make_dirs(dest, &parts[..parts.len() - 1])?;
        match kind {
            ExtractedKind::Dir => {
                match fs::DirBuilder::new().mode(0o700).create(&target) {
                    Ok(()) => {}
                    // made earlier as the parent of a file
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && target.is_dir() => {}
                    Err(e) => return Err(io_err("creating", &target, e)),
                }
                dirs.push(target);
                out.push(Extracted {
                    path: relative,
                    kind,
                    size: 0,
                    mode: 0o700,
                });
            }
            ExtractedKind::File => {
                let declared = entry
                    .header()
                    .size()
                    .map_err(|e| bad(format!("bad size in a header: {e}")))?;
                if declared > limits.max_file {
                    return Err(bad(format!(
                        "{joined:?} is larger than {} bytes",
                        limits.max_file
                    )));
                }
                if total.saturating_add(declared) > limits.max_total {
                    return Err(bad(format!(
                        "the archive holds more than {} bytes in total",
                        limits.max_total
                    )));
                }
                let executable = entry
                    .header()
                    .mode()
                    .map(|m| m & 0o100 != 0)
                    .unwrap_or(false);
                let mode = if executable { 0o500 } else { 0o400 };
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)
                    .map_err(|e| io_err("creating", &target, e))?;
                let mut size = 0u64;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = entry
                        .read(&mut buf)
                        .map_err(|e| bad(format!("cannot read {joined:?}: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    // checked before writing: what is on disk never passes the limits
                    if size + n as u64 > limits.max_file {
                        return Err(bad(format!(
                            "{joined:?} is larger than {} bytes",
                            limits.max_file
                        )));
                    }
                    if total + n as u64 > limits.max_total {
                        return Err(bad(format!(
                            "the archive holds more than {} bytes in total",
                            limits.max_total
                        )));
                    }
                    output
                        .write_all(&buf[..n])
                        .map_err(|e| io_err("writing", &target, e))?;
                    size += n as u64;
                    total += n as u64;
                }
                output
                    .set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(|e| io_err("setting the mode of", &target, e))?;
                out.push(Extracted {
                    path: relative,
                    kind,
                    size,
                    mode,
                });
            }
        }
    }
    if long_name.is_some() {
        return Err(bad("an extended name has no following entry"));
    }
    // last, so that the 0700 directories could be written to while they were being filled
    for dir in dirs {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("setting the mode of", &dir, e))?;
    }
    Ok(out)
}

/// Create the parent directories below `dest` (all 0700), refusing to pass through
/// anything that is not a directory.
fn make_dirs(dest: &Path, parts: &[&str]) -> Result<()> {
    let mut dir = dest.to_path_buf();
    for part in parts {
        dir.push(part);
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&dir).map_err(|e| io_err("looking at", &dir, e))?;
                if !meta.is_dir() {
                    return Err(bad(format!(
                        "{} is both a file and a directory in the archive",
                        dir.display()
                    )));
                }
            }
            Err(e) => return Err(io_err("creating", &dir, e)),
        }
    }
    Ok(())
}
