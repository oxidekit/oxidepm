//! Checksum verification for the adapter files installed alongside a release.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const DIRECTORY: &str = "share/oxidepm/adapters";
const CHECKSUMS: &str = "SHA256SUMS";
const SIGNATURE: &str = "SHA256SUMS.minisig";
const INDEX: &str = "index.json";
const MAX_CHECKSUMS: u64 = 1024 * 1024;
const MAX_SIGNATURE: u64 = 16 * 1024;

pub fn directory(executable: &Path) -> io::Result<PathBuf> {
    let prefix = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("the executable has no installation prefix"))?;
    Ok(prefix.join(DIRECTORY))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn effective_uid() -> io::Result<u32> {
    let status = fs::read("/proc/thread-self/status")?;
    status
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))
        .and_then(|uids| {
            std::str::from_utf8(uids)
                .ok()?
                .split_ascii_whitespace()
                .nth(1)
        })
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| invalid("cannot read the effective UID"))
}

fn check_owner_and_mode(path: &Path, metadata: &Metadata, uid: u32) -> io::Result<()> {
    let owner = metadata.uid();
    if owner != 0 && owner != uid {
        return Err(invalid(format!(
            "{} is owned by UID {owner}, expected root or UID {uid}",
            path.display()
        )));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(invalid(format!(
            "{} has group or other write access",
            path.display()
        )));
    }
    Ok(())
}

fn descriptor_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn open_directory(path: &Path, display: &Path, uid: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot open {} as a directory without following symbolic links: {e}",
                    display.display()
                ),
            )
        })?;
    check_owner_and_mode(display, &file.metadata()?, uid)?;
    Ok(file)
}

fn open_bundle_directory(dir: &Path, uid: u32) -> io::Result<File> {
    let relative = Path::new(DIRECTORY);
    let mut path = dir
        .ancestors()
        .nth(relative.components().count())
        .filter(|_| dir.ends_with(relative))
        .ok_or_else(|| invalid(format!("{} is outside the bundle layout", dir.display())))?
        .to_path_buf();
    let mut directory = open_directory(&path, &path, uid)?;
    for name in relative {
        path.push(name);
        directory = open_directory(&descriptor_path(&directory).join(name), &path, uid)?;
    }
    Ok(directory)
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

fn open_regular(dir: &File, display: &Path, name: &str, uid: Option<u32>) -> io::Result<File> {
    let path = display.join(name);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(descriptor_path(dir).join(name))
        .map_err(|e| io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display())))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    if let Some(uid) = uid {
        check_owner_and_mode(&path, &metadata, uid)?;
    }
    Ok(file)
}

pub struct ReleaseBundle {
    dir: PathBuf,
    directory: File,
    uid: u32,
    checksums: BTreeMap<String, String>,
    checksums_text: String,
}

impl ReleaseBundle {
    pub fn read(dir: PathBuf) -> io::Result<Self> {
        let uid = effective_uid()?;
        let directory = open_bundle_directory(&dir, uid)?;
        let mut input =
            open_regular(&directory, &dir, CHECKSUMS, Some(uid))?.take(MAX_CHECKSUMS + 1);
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
        Ok(Self {
            dir,
            directory,
            uid,
            checksums,
            checksums_text: text,
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The exact bytes from which the checksum list was parsed.
    pub fn checksums_bytes(&self) -> &[u8] {
        self.checksums_text.as_bytes()
    }

    /// Read the detached signature from the same checked directory as the checksums.
    pub fn read_signature(&self) -> io::Result<String> {
        let mut input = open_regular(&self.directory, &self.dir, SIGNATURE, Some(self.uid))?
            .take(MAX_SIGNATURE + 1);
        let mut text = String::new();
        input
            .read_to_string(&mut text)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot read {SIGNATURE}: {e}")))?;
        if input.limit() == 0 {
            return Err(invalid(format!("{SIGNATURE} is too large")));
        }
        Ok(text)
    }

    /// Copy the exact bytes checked against the release list. The caller discards the copy on error.
    pub fn copy_verified(&self, name: &str, max: u64, mut out: impl Write) -> io::Result<String> {
        let expected = self
            .checksums
            .get(name)
            .ok_or_else(|| invalid(format!("{name} is missing from the bundle checksum list")))?;
        let uid = (name == INDEX).then_some(self.uid);
        let mut file = open_regular(&self.directory, &self.dir, name, uid)?.take(max + 1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn bundle() -> (tempfile::TempDir, PathBuf) {
        let prefix = tempfile::tempdir().unwrap();
        let dir = prefix.path().join(DIRECTORY);
        fs::create_dir_all(&dir).unwrap();
        let index = b"{}";
        fs::write(dir.join("index.json"), index).unwrap();
        fs::write(
            dir.join(CHECKSUMS),
            format!("{:x}  index.json\n", Sha256::digest(index)),
        )
        .unwrap();
        (prefix, dir)
    }

    fn read_index(dir: PathBuf) -> io::Result<()> {
        ReleaseBundle::read(dir)?.copy_verified("index.json", 1024, io::sink())?;
        Ok(())
    }

    #[test]
    fn keeps_the_exact_checksums_read_from_the_bundle() {
        let (_prefix, dir) = bundle();
        let path = dir.join(CHECKSUMS);
        let contents = format!("{:X} *index.json\r\n", Sha256::digest(b"{}"));
        fs::write(&path, &contents).unwrap();
        let bundle = ReleaseBundle::read(dir).unwrap();
        fs::write(path, "").unwrap();
        assert_eq!(bundle.checksums_bytes(), contents.as_bytes());
        bundle
            .copy_verified("index.json", 1024, io::sink())
            .unwrap();
    }

    #[test]
    fn reads_only_regular_bundle_signatures_with_safe_permissions_and_size() {
        let (prefix, dir) = bundle();
        let path = dir.join(SIGNATURE);
        let bundle = ReleaseBundle::read(dir).unwrap();
        assert_eq!(
            bundle.read_signature().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let outside = prefix.path().join("signature");
        fs::write(&outside, "detached signature").unwrap();
        symlink(&outside, &path).unwrap();
        let err = bundle.read_signature().unwrap_err();
        assert!(err.to_string().contains(SIGNATURE), "{err}");
        fs::remove_file(&path).unwrap();

        fs::rename(outside, &path).unwrap();
        assert_eq!(bundle.read_signature().unwrap(), "detached signature");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
        let err = bundle.read_signature().unwrap_err();
        assert!(
            err.to_string().contains("group or other write access"),
            "{err}"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, "x".repeat(MAX_SIGNATURE as usize + 1)).unwrap();
        let err = bundle.read_signature().unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn refuses_group_or_other_writable_bundle_entries() {
        for (entry, mode) in [("", 0o775), (CHECKSUMS, 0o664), ("index.json", 0o646)] {
            let (_prefix, dir) = bundle();
            let path = if entry.is_empty() {
                dir.clone()
            } else {
                dir.join(entry)
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let error = read_index(dir).expect_err("a writable entry must be refused");
            assert!(error.to_string().contains("group or other"), "{error}");
            assert!(
                error.to_string().contains(path.to_str().unwrap()),
                "{error}"
            );
        }
    }

    #[test]
    fn refuses_symbolic_links_in_the_bundle_path() {
        for entry in ["share", "share/oxidepm", DIRECTORY] {
            let (prefix, dir) = bundle();
            let path = prefix.path().join(entry);
            let moved = prefix.path().join("moved");
            fs::rename(&path, &moved).unwrap();
            symlink(moved, path).unwrap();
            let error = read_index(dir).expect_err("a symbolic link must be refused");
            assert!(error.to_string().contains("symbolic link"), "{error}");
        }
    }
}
