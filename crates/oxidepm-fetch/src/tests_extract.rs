use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tar::{EntryType, Header};

use crate::extract::extract_into;
use crate::{extract_tar_gz, ExtractLimits, ExtractedKind, FetchError};

/// Builds a gzip-compressed tar from hand-made headers, so that names the `tar` crate itself would
/// refuse to write (`..`, absolute paths) can be put into an archive.
struct Arc {
    tar: tar::Builder<Vec<u8>>,
}

impl Arc {
    fn new() -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
        }
    }

    fn raw(
        &mut self,
        name: &str,
        kind: EntryType,
        mode: u32,
        data: &[u8],
        link: Option<&str>,
    ) -> &mut Self {
        let mut header = Header::new_gnu();
        {
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name = [0; 100];
            assert!(name.len() < 100, "test names are short");
            gnu.name[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(link) = link {
                gnu.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
        }
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_size(data.len() as u64);
        header.set_cksum();
        self.tar.append(&header, data).unwrap();
        self
    }

    fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.raw(name, EntryType::Regular, 0o644, data, None)
    }

    fn dir(&mut self, name: &str) -> &mut Self {
        self.raw(name, EntryType::Directory, 0o755, b"", None)
    }

    fn write(&mut self, path: &Path) {
        let bytes = std::mem::replace(&mut self.tar, tar::Builder::new(Vec::new()))
            .into_inner()
            .unwrap();
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        );
        gz.write_all(&bytes).unwrap();
        gz.finish().unwrap();
    }
}

struct Case {
    dir: tempfile::TempDir,
    archive: PathBuf,
    dest: PathBuf,
}

fn case(build: impl FnOnce(&mut Arc)) -> Case {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("a.tar.gz");
    let mut a = Arc::new();
    build(&mut a);
    a.write(&archive);
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    Case { dir, archive, dest }
}

impl Case {
    fn extract(&self, limits: &ExtractLimits) -> crate::Result<Vec<crate::Extracted>> {
        extract_tar_gz(&self.archive, &self.dest, limits)
    }

    fn assert_refused(&self, limits: &ExtractLimits, needle: &str) {
        let err = self
            .extract(limits)
            .expect_err("the archive must be refused");
        assert!(matches!(err, FetchError::BadArchive(_)), "{err}");
        assert!(
            err.to_string().contains(needle),
            "wanted {needle:?} in {err}"
        );
        assert_eq!(
            std::fs::read_dir(&self.dest).unwrap().count(),
            0,
            "a refused archive leaves nothing behind"
        );
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        // read-only files in 0700 directories can be deleted, but make sure of it
        let _ = std::fs::set_permissions(&self.dest, std::fs::Permissions::from_mode(0o700));
        let _ = &self.dir;
    }
}

fn limits() -> ExtractLimits {
    ExtractLimits::default()
}

#[test]
fn a_plain_archive_is_unpacked() {
    let c = case(|a| {
        a.dir("./")
            .dir("./bin/")
            .file("./bin/tool", b"binary")
            .file("./README", b"hi");
    });
    let done = c.extract(&limits()).unwrap();
    assert_eq!(std::fs::read(c.dest.join("bin/tool")).unwrap(), b"binary");
    assert_eq!(std::fs::read(c.dest.join("README")).unwrap(), b"hi");
    assert!(done
        .iter()
        .any(|e| e.path == Path::new("bin/tool") && e.kind == ExtractedKind::File && e.size == 6));
    assert!(done
        .iter()
        .any(|e| e.path == Path::new("bin") && e.kind == ExtractedKind::Dir));
}

#[test]
fn tar_rejects_names_beyond_the_supported_directory_depth() {
    let path = ["nested"; 66].join("/");
    let c = case(|a| {
        let mut header = Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(1);
        a.tar.append_data(&mut header, &path, &b"x"[..]).unwrap();
    });
    c.assert_refused(&limits(), "path components");
}

#[test]
fn tar_rejects_parent_dir() {
    for name in ["../evil", "a/../../evil", "a/..", "a/b/../c"] {
        let c = case(|a| {
            a.file("ok", b"fine").file(name, b"x");
        });
        c.assert_refused(&limits(), "parent directory");
        assert!(!c.dir.path().join("evil").exists());
    }
}

#[test]
fn tar_rejects_absolute_path() {
    let c = case(|a| {
        a.file("/tmp/oxidepm-fetch-test-should-not-exist", b"x");
    });
    c.assert_refused(&limits(), "absolute path");
    assert!(!Path::new("/tmp/oxidepm-fetch-test-should-not-exist").exists());
}

#[test]
fn tar_rejects_empty_components_and_control_characters() {
    let c = case(|a| {
        a.file("a//b", b"x");
    });
    c.assert_refused(&limits(), "empty path component");
    let c = case(|a| {
        a.file("a/./b", b"x");
    });
    c.assert_refused(&limits(), "'.'");
    let c = case(|a| {
        a.file("bad\u{1b}[31mname", b"x");
    });
    c.assert_refused(&limits(), "control character");
}

#[test]
fn tar_rejects_symlink() {
    let c = case(|a| {
        a.raw("link", EntryType::Symlink, 0o777, b"", Some("/etc/passwd"));
    });
    c.assert_refused(&limits(), "symbolic link");
    // a link that points inside the archive is refused as well, and so is one placed first
    // so that a later "file" would be written through it
    let c = case(|a| {
        a.raw("dir", EntryType::Symlink, 0o777, b"", Some("/tmp"))
            .file("dir/escape", b"x");
    });
    c.assert_refused(&limits(), "symbolic link");
}

#[test]
fn tar_rejects_hard_link() {
    let c = case(|a| {
        a.file("real", b"x")
            .raw("hard", EntryType::Link, 0o644, b"", Some("real"));
    });
    c.assert_refused(&limits(), "hard link");
}

#[test]
fn tar_rejects_device() {
    for (kind, what) in [
        (EntryType::Char, "device"),
        (EntryType::Block, "device"),
        (EntryType::Fifo, "FIFO"),
    ] {
        let c = case(|a| {
            a.raw("node", kind, 0o666, b"", None);
        });
        c.assert_refused(&limits(), what);
    }
}

#[test]
fn tar_rejects_duplicate_and_case_duplicate() {
    let c = case(|a| {
        a.file("tool", b"1").file("tool", b"2");
    });
    c.assert_refused(&limits(), "duplicate");
    let c = case(|a| {
        a.file("Tool", b"1").file("tool", b"2");
    });
    c.assert_refused(&limits(), "only by case");
    let c = case(|a| {
        a.dir("sub/").file("sub", b"2");
    });
    c.assert_refused(&limits(), "duplicate");
    // a name used as a file and as a directory
    let c = case(|a| {
        a.file("a", b"1").file("a/b", b"2");
    });
    let err = c.extract(&limits()).unwrap_err();
    assert!(matches!(err, FetchError::BadArchive(_)), "{err}");
}

#[test]
fn tar_rejects_too_many_entries() {
    let c = case(|a| {
        for i in 0..6 {
            a.file(&format!("f{i}"), b"x");
        }
    });
    c.assert_refused(
        &ExtractLimits {
            max_entries: 5,
            ..limits()
        },
        "more than 5 entries",
    );
    // the real default: 1000 are fine, 1001 are not
    let c = case(|a| {
        for i in 0..1000 {
            a.file(&format!("f{i}"), b"");
        }
    });
    assert_eq!(c.extract(&limits()).unwrap().len(), 1000);
    let c = case(|a| {
        for i in 0..1001 {
            a.file(&format!("f{i}"), b"");
        }
    });
    c.assert_refused(&limits(), "more than 1000 entries");
}

#[test]
fn tar_bomb_is_cut_off_at_the_total_cap() {
    // 200 files of 1 MiB zeros: 200 MiB when unpacked, a few hundred KiB as an archive
    let zeros = vec![0u8; 1024 * 1024];
    let c = case(|a| {
        for i in 0..200 {
            a.file(&format!("f{i}"), &zeros);
        }
    });
    let small = ExtractLimits {
        max_entries: 1000,
        max_file: 1024 * 1024,
        max_total: 3 * 1024 * 1024,
    };
    // no clean-up here, to see how far it got
    let err = extract_into(&c.archive, &c.dest, &small).unwrap_err();
    assert!(err.to_string().contains("in total"), "{err}");
    let written: u64 = std::fs::read_dir(&c.dest)
        .unwrap()
        .flatten()
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(
        written <= small.max_total,
        "{written} bytes were written, the cap is {}",
        small.max_total
    );
    assert!(
        written >= 2 * 1024 * 1024,
        "it stopped at the cap, not earlier ({written})"
    );
    // the public call refuses it too and removes what was written
    c.assert_refused_after_clean(&small);

    // one file above the per-file cap is refused from its header, before any byte is written
    let c = case(|a| {
        a.file("big", &zeros);
    });
    c.assert_refused(
        &ExtractLimits {
            max_file: 1000,
            ..limits()
        },
        "larger than 1000 bytes",
    );
}

/// The cap on the decompressed stream catches what the file counters do not see: a bomb in the
/// extended headers (a GNU long name of 12 MiB of zeros) in front of a harmless file.
#[test]
fn tar_bomb_in_an_extended_header_is_cut_off() {
    let zeros = vec![0u8; 12 * 1024 * 1024];
    let c = case(|a| {
        a.raw("././@LongLink", EntryType::GNULongName, 0o644, &zeros, None)
            .file("ok", b"x");
    });
    let err = c
        .extract(&ExtractLimits {
            max_total: 1024 * 1024,
            ..limits()
        })
        .unwrap_err();
    assert!(err.to_string().contains("more than the limit"), "{err}");
    assert_eq!(std::fs::read_dir(&c.dest).unwrap().count(), 0);
}

impl Case {
    fn assert_refused_after_clean(&self, limits: &ExtractLimits) {
        // `extract_into` left files behind: start again from an empty destination
        for e in std::fs::read_dir(&self.dest).unwrap().flatten() {
            std::fs::remove_file(e.path()).unwrap();
        }
        self.assert_refused(limits, "in total");
    }
}

#[test]
fn tar_masks_modes() {
    let c = case(|a| {
        a.raw("d/", EntryType::Directory, 0o777, b"", None)
            .raw("d/run", EntryType::Regular, 0o755, b"x", None)
            .raw("d/suid", EntryType::Regular, 0o4755, b"x", None)
            .raw("d/data", EntryType::Regular, 0o666, b"x", None)
            .raw("d/ro", EntryType::Regular, 0o000, b"x", None)
            .raw("implicit/deep/file", EntryType::Regular, 0o777, b"x", None)
            .raw("closed/", EntryType::Directory, 0o500, b"", None)
            .raw("closed/inside", EntryType::Regular, 0o644, b"x", None);
    });
    let done = c.extract(&limits()).unwrap();
    let mode = |p: &str| {
        std::fs::metadata(c.dest.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("d"), 0o700);
    assert_eq!(mode("d/run"), 0o500);
    assert_eq!(mode("d/suid"), 0o500, "setuid bits are dropped");
    assert_eq!(mode("d/data"), 0o400);
    assert_eq!(mode("d/ro"), 0o400);
    assert_eq!(
        mode("implicit"),
        0o700,
        "a directory the archive never listed is private too"
    );
    assert_eq!(mode("implicit/deep"), 0o700);
    assert_eq!(mode("implicit/deep/file"), 0o500);
    assert_eq!(
        mode("closed"),
        0o700,
        "a directory without the write bit could not be moved or deleted"
    );
    assert!(done
        .iter()
        .any(|e| e.path == Path::new("d/run") && e.mode == 0o500));
    // the whole tree can be removed again
    std::fs::remove_dir_all(&c.dest).unwrap();
}

#[test]
fn the_destination_must_be_an_empty_directory() {
    let c = case(|a| {
        a.file("a", b"x");
    });
    std::fs::write(c.dest.join("old"), b"keep").unwrap();
    let err = c.extract(&limits()).unwrap_err();
    assert!(matches!(err, FetchError::Unsafe { .. }), "{err}");
    assert_eq!(
        std::fs::read(c.dest.join("old")).unwrap(),
        b"keep",
        "nothing of a refused destination is touched"
    );
    let err = extract_tar_gz(&c.archive, &c.dir.path().join("missing"), &limits()).unwrap_err();
    assert!(matches!(err, FetchError::Io { .. }), "{err}");
    let err = extract_tar_gz(&c.archive, &c.archive, &limits()).unwrap_err();
    assert!(matches!(err, FetchError::Unsafe { .. }), "{err}");
}

#[test]
fn something_that_is_not_an_archive_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.tar.gz");
    std::fs::write(&junk, b"this is not gzip").unwrap();
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let err = extract_tar_gz(&junk, &dest, &limits()).unwrap_err();
    assert!(matches!(err, FetchError::BadArchive(_)), "{err}");
}
