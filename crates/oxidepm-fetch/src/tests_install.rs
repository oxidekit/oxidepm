use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::{check_version, install_file_atomic, FetchError, StagedFile};

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// A child process this test started, killed and reaped when the guard goes away.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn install_replaces_a_running_binary() {
    let dir = tempfile::tempdir().unwrap();
    let running = dir.path().join("worker");
    std::fs::copy("/bin/bash", &running).unwrap();
    std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Guard(
        Command::new(&running)
            .args(["-c", "printf 'ready\\n'; IFS= read -r line"])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n", "the old executable started");

    let replacement = dir.path().join("new-worker");
    std::fs::write(&replacement, b"#!/bin/sh\nexit 0\n").unwrap();
    let old_inode = std::fs::metadata(&running).unwrap().ino();

    // Linux rejects opening the running executable for an in-place copy.
    #[cfg(target_os = "linux")]
    {
        let copy = std::fs::copy(&replacement, &running);
        assert_eq!(
            copy.unwrap_err().raw_os_error(),
            Some(26),
            "expected ETXTBSY (text file busy)"
        );
    }

    install_file_atomic(&replacement, &running, 0o755)
        .expect("the install over a running binary succeeds");
    assert_ne!(std::fs::metadata(&running).unwrap().ino(), old_inode);
    assert_eq!(std::fs::read(&running).unwrap(), b"#!/bin/sh\nexit 0\n");
    assert_eq!(mode_of(&running), 0o755);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the old process keeps running"
    );
    writeln!(child.0.stdin.take().unwrap(), "continue").unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(
        !dir.path().join("worker.new").exists(),
        "no staging file is left"
    );
}

#[test]
fn install_is_atomic_when_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("tool");
    std::fs::write(&dst, b"old").unwrap();

    // the source cannot be read to the end (a directory opens and then fails to read)
    let bad_src = dir.path().join("src-dir");
    std::fs::create_dir(&bad_src).unwrap();
    let err = install_file_atomic(&bad_src, &dst, 0o755).unwrap_err();
    assert!(matches!(err, FetchError::Io { .. }), "{err}");
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        b"old",
        "the destination is as it was"
    );
    assert!(
        !dir.path().join("tool.new").exists(),
        "the half-written copy is removed"
    );

    // the rename fails (the destination is a directory that is not empty)
    let blocked = dir.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("inside"), b"x").unwrap();
    let good_src = dir.path().join("good");
    std::fs::write(&good_src, b"new").unwrap();
    let err = install_file_atomic(&good_src, &blocked, 0o755).unwrap_err();
    assert!(matches!(err, FetchError::Io { .. }), "{err}");
    assert!(blocked.join("inside").exists());
    assert!(!dir.path().join("blocked.new").exists());

    // a staged copy that is dropped without a commit leaves nothing
    let staged = StagedFile::stage(&good_src, &dst, 0o755).unwrap();
    assert!(staged.staged_path().exists());
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        b"old",
        "staging does not touch the destination"
    );
    drop(staged);
    assert!(!dir.path().join("tool.new").exists());

    // an existing entry keeps its identity while the install uses its own staging file
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    std::os::unix::fs::symlink(&victim, dir.path().join("tool.new")).unwrap();
    install_file_atomic(&good_src, &dst, 0o755).unwrap();
    assert!(std::fs::symlink_metadata(dir.path().join("tool.new"))
        .unwrap()
        .is_symlink());
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(std::fs::read(&dst).unwrap(), b"new");
}

#[test]
fn staged_copies_of_the_same_destination_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("tool");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::write(&a, b"first").unwrap();
    std::fs::write(&b, b"second").unwrap();
    let first = StagedFile::stage(&a, &dst, 0o755).unwrap();
    let second = StagedFile::stage(&b, &dst, 0o755).unwrap();
    assert_ne!(first.staged_path(), second.staged_path());
    first.commit().unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"first");
    second.commit().unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"second");
}

#[test]
fn a_reader_never_sees_half_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("tool");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::write(&a, vec![b'A'; 512 * 1024]).unwrap();
    std::fs::write(&b, vec![b'B'; 512 * 1024]).unwrap();
    install_file_atomic(&a, &dst, 0o755).unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (stop, dst) = (stop.clone(), dst.clone());
        std::thread::spawn(move || {
            let mut reads = 0u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let data = std::fs::read(&dst).unwrap();
                assert_eq!(data.len(), 512 * 1024, "a partial file was visible");
                assert!(
                    data.iter().all(|&c| c == data[0]),
                    "a mixed file was visible"
                );
                reads += 1;
            }
            reads
        })
    };
    for i in 0..60 {
        install_file_atomic(if i % 2 == 0 { &b } else { &a }, &dst, 0o755).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
}

#[test]
fn version_charset_and_semver() {
    for ok in [
        "0.3.2",
        "1.0.0",
        "10.20.30",
        "1.2.3-rc.1",
        "1.2.3+build.5",
        "1.2.3-alpha-1+x.y",
    ] {
        check_version(ok, "0.3.2", true).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [
        "",
        "v1.0.0",
        "1.0",
        "1.0.0.0",
        "01.0.0",
        "1.0.0-",
        "1.0.0-01",
        "1.0.0+",
        "a.b.c",
        "1/../../x",
        "1.0.0 ",
        "1.0.0\n",
        "1.0.0;rm",
        "1.0.0%2f",
        "../1.0.0",
        "1.0.0?x=1",
        "1.0.0#x",
        "1.0.0_1",
        "99999999999999999999.0.0",
    ] {
        let err = check_version(bad, "0.3.2", true).unwrap_err();
        assert!(matches!(err, FetchError::BadVersion(..)), "{bad:?}: {err}");
    }
    let long = format!("1.0.0-{}", "a".repeat(80));
    assert!(matches!(
        check_version(&long, "0.3.2", true),
        Err(FetchError::BadVersion(..))
    ));
    // the running build's own version must be comparable too
    assert!(matches!(
        check_version("1.0.0", "garbage", true),
        Err(FetchError::BadVersion(..))
    ));
}

#[test]
fn downgrade_needs_force() {
    let err = check_version("0.3.1", "0.3.2", false).unwrap_err();
    assert!(matches!(err, FetchError::Downgrade { .. }), "{err}");
    assert!(err.to_string().contains("--force"));
    check_version("0.3.1", "0.3.2", true).unwrap();
    // same and newer versions need no force; a pre-release is below its release
    check_version("0.3.2", "0.3.2", false).unwrap();
    check_version("0.4.0", "0.3.2", false).unwrap();
    check_version("0.10.0", "0.9.0", false).unwrap();
    assert!(matches!(
        check_version("0.4.0-rc.1", "0.4.0", false),
        Err(FetchError::Downgrade { .. })
    ));
    check_version("0.4.0", "0.4.0-rc.1", false).unwrap();
}
