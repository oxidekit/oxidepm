//! File helpers shared by the daemon and the adapters.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes` so that a crash leaves either the old file or the new one, and
/// never a file anybody else could read half way: the parent directory of `path` is created if it
/// is missing, the bytes go to a temporary file in the same directory that is created with mode
/// 0600 (there is no moment at which it is readable by others), and the temporary file is renamed
/// over `path`. `durable`: also fsync the file and the directory, so the new content survives a
/// power cut.
///
/// Every failure is returned; the temporary file is removed when anything went wrong.
pub fn write_atomic(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    write_atomic_with_counter(path, durable, |file| file.write_all(bytes), &TMP_COUNTER)
}

fn write_atomic_with_counter(
    path: &Path,
    durable: bool,
    write_contents: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    counter: &AtomicU64,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let (tmp, mut file) = loop {
        let suffix = counter.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{suffix}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };

    let result = (|| {
        write_contents(&mut file)?;
        if durable {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if durable {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_atomic_creates_the_parent_of_the_path_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("file.toml");
        write_atomic(&path, b"x = 1\n", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x = 1\n");
    }

    #[test]
    fn write_atomic_file_is_owner_only_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic(&path, b"token", false).unwrap();
        assert_eq!(mode(&path), 0o600);
        // an existing file with wider rights is replaced by one that has none
        let wide = dir.path().join("wide");
        std::fs::write(&wide, b"old").unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&wide, b"new", false).unwrap();
        assert_eq!(
            (std::fs::read(&wide).unwrap(), mode(&wide)),
            (b"new".to_vec(), 0o600)
        );
    }

    #[test]
    fn write_atomic_skips_an_existing_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let occupied = dir
            .path()
            .join(format!(".state.{}.0.tmp", std::process::id()));
        std::fs::write(&occupied, b"other writer").unwrap();

        let result = write_atomic_with_counter(
            &path,
            false,
            |file| file.write_all(b"new state"),
            &AtomicU64::new(0),
        );
        assert_eq!(std::fs::read(&occupied).unwrap(), b"other writer");
        result.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new state");
    }

    #[test]
    fn write_atomic_leaves_no_temporary_file_and_reports_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        write_atomic(&path, b"1", true).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("file")]);
        // the target is a directory: the rename fails, the error comes back, nothing is left over
        let target = dir.path().join("dir");
        std::fs::create_dir(&target).unwrap();
        assert!(write_atomic(&target, b"1", false).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // the parent is a file: no directory can be made, and that is an error too
        assert!(write_atomic(&path.join("child"), b"1", false).is_err());
    }

    #[test]
    fn write_atomic_writers_in_one_process_do_not_share_a_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared");
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        write_atomic(&path, format!("{n}").repeat(2000).as_bytes(), false).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.len(), 2000);
        assert!(
            content
                .chars()
                .all(|c| c == content.chars().next().unwrap()),
            "never a mixture of two writes"
        );
    }
}
