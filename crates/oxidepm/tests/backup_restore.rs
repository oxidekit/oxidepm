//! `oxidepm backup` / `oxidepm restore`.

mod common;

use common::{MockDaemon, TestHome};
use flate2::write::GzEncoder;
use flate2::Compression;
use oxidepm_ipc::Response;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

enum Member<'a> {
    File(&'a str, &'a [u8]),
    Symlink(&'a str, &'a str),
    Hardlink(&'a str, &'a str),
}

/// Build a .tar.gz with raw member names (bypassing the tar crate's path sanitising).
fn build_archive(path: &Path, members: &[Member]) {
    let file = std::fs::File::create(path).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
    for m in members {
        let mut header = tar::Header::new_ustar();
        let (name, data): (&str, &[u8]) = match m {
            Member::File(n, d) => {
                header.set_entry_type(tar::EntryType::Regular);
                (n, d)
            }
            Member::Symlink(n, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name(target).unwrap();
                (n, &[])
            }
            Member::Hardlink(n, target) => {
                header.set_entry_type(tar::EntryType::Link);
                header.set_link_name(target).unwrap();
                (n, &[])
            }
        };
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

fn restore_fails(home: &TestHome, archive: &Path, extra: &[&str]) -> String {
    let out = home
        .cmd()
        .arg("restore")
        .arg(archive)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "restore must be refused; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn restore_rejects_symlink_member() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    let archive = home.path().join("evil.tar.gz");
    build_archive(&archive, &[Member::Symlink("saved.json", "/tmp/victim")]);
    let err = restore_fails(&home, &archive, &["--yes"]);
    assert!(err.to_lowercase().contains("regular file"), "stderr: {err}");
    assert!(listing(&oxhome).is_empty(), "nothing may be extracted");
}

#[test]
fn restore_rejects_hardlink_member() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    let archive = home.path().join("evil.tar.gz");
    // The link target is a member that exists in the archive, so only the
    // entry-type check stands between this archive and a hardlink.
    build_archive(
        &archive,
        &[
            Member::File("saved.json", b"[]"),
            Member::Hardlink("adapters.db", "saved.json"),
        ],
    );
    let err = restore_fails(&home, &archive, &["--yes"]);
    assert!(err.to_lowercase().contains("regular file"), "stderr: {err}");
    assert!(listing(&oxhome).is_empty());
}

#[test]
fn restore_rejects_path_traversal_and_absolute_members() {
    for name in ["../evil", "/tmp/evil", "sub/saved.json", "./../saved.json"] {
        let home = TestHome::new();
        let oxhome = home.oxidepm_dir();
        let archive = home.path().join("evil.tar.gz");
        build_archive(
            &archive,
            &[Member::File("saved.json", b"[]"), Member::File(name, b"x")],
        );
        let err = restore_fails(&home, &archive, &["--yes"]);
        assert!(
            err.contains("Unexpected archive member"),
            "{name}: stderr: {err}"
        );
        assert!(
            listing(&oxhome).is_empty(),
            "{name}: a rejected archive must leave nothing behind"
        );
        assert!(!home.path().join("evil").exists());
    }
}

#[test]
fn restore_rejects_unexpected_members() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    let archive = home.path().join("extra.tar.gz");
    build_archive(
        &archive,
        &[
            Member::File("saved.json", b"[]"),
            Member::File("adapters", b"x"),
        ],
    );
    let err = restore_fails(&home, &archive, &["--yes"]);
    assert!(err.contains("adapters"), "stderr: {err}");
    assert!(listing(&oxhome).is_empty());
}

#[test]
fn restore_rejects_duplicate_members() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    let archive = home.path().join("dup.tar.gz");
    build_archive(
        &archive,
        &[
            Member::File("saved.json", b"1"),
            Member::File("saved.json", b"2"),
        ],
    );
    let err = restore_fails(&home, &archive, &["--yes"]);
    assert!(err.contains("more than once"), "stderr: {err}");
    assert!(listing(&oxhome).is_empty());
}

#[test]
fn restore_rejects_non_archive() {
    let home = TestHome::new();
    home.oxidepm_dir();
    let junk = home.path().join("junk.tar.gz");
    std::fs::write(&junk, b"this is not a tarball").unwrap();
    let err = restore_fails(&home, &junk, &["--yes"]);
    assert!(err.contains("Refusing to restore"), "stderr: {err}");
    assert!(
        err.contains("Corrupt archive") || err.contains("Not a valid"),
        "stderr: {err}"
    );
}

#[test]
fn restore_refuses_while_daemon_is_running() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    let _daemon = MockDaemon::start(&home, |_| Some(Response::Pong));
    let archive = home.path().join("ok.tar.gz");
    build_archive(&archive, &[Member::File("saved.json", b"[]")]);
    let err = restore_fails(&home, &archive, &["--yes"]);
    assert!(
        err.to_lowercase().contains("daemon is running"),
        "stderr: {err}"
    );
    assert!(!oxhome.join("saved.json").exists());
}

#[test]
fn restore_requires_yes_to_overwrite() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"OLD").unwrap();
    let archive = home.path().join("ok.tar.gz");
    build_archive(&archive, &[Member::File("saved.json", b"NEW")]);

    let err = restore_fails(&home, &archive, &[]);
    assert!(err.contains("--yes"), "stderr: {err}");
    assert!(err.contains("saved.json"), "stderr: {err}");
    assert_eq!(std::fs::read(oxhome.join("saved.json")).unwrap(), b"OLD");

    let out = home
        .cmd()
        .arg("restore")
        .arg(&archive)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(oxhome.join("saved.json")).unwrap(), b"NEW");
    let mode = std::fs::metadata(oxhome.join("saved.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "restored files must be private");
    assert_eq!(
        listing(&oxhome),
        vec!["saved.json"],
        "no temp files left behind"
    );
}

#[test]
fn restore_into_empty_home_needs_no_confirmation() {
    let home = TestHome::new();
    let archive = home.path().join("ok.tar.gz");
    build_archive(
        &archive,
        &[
            Member::File("./saved.json", b"[]"),
            Member::File("adapters.db", b"adb"),
            Member::File("oxidepm.db", b"db"),
        ],
    );
    let out = home.cmd().arg("restore").arg(&archive).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let oxhome = home.oxidepm_dir();
    assert_eq!(
        listing(&oxhome),
        vec!["adapters.db", "oxidepm.db", "saved.json"]
    );
}

#[test]
fn restore_removes_stale_db_journal() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("oxidepm.db"), b"old").unwrap();
    std::fs::write(oxhome.join("oxidepm.db-journal"), b"stale").unwrap();
    let archive = home.path().join("ok.tar.gz");
    build_archive(&archive, &[Member::File("oxidepm.db", b"new")]);
    let out = home
        .cmd()
        .arg("restore")
        .arg(&archive)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(listing(&oxhome), vec!["oxidepm.db"]);
}

#[test]
fn restore_removes_stale_state_db_wal_and_shm() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("state.db"), b"old").unwrap();
    std::fs::write(oxhome.join("state.db-wal"), b"stale").unwrap();
    std::fs::write(oxhome.join("state.db-shm"), b"stale").unwrap();
    let archive = home.path().join("ok.tar.gz");
    build_archive(&archive, &[Member::File("state.db", b"new")]);
    let out = home
        .cmd()
        .arg("restore")
        .arg(&archive)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(listing(&oxhome), vec!["state.db"]);
    assert_eq!(std::fs::read(oxhome.join("state.db")).unwrap(), b"new");
}

// ---- backup ----

#[test]
fn backup_help_describes_the_archive_contents() {
    let home = TestHome::new();
    let out = home.cmd().args(["backup", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(
        help.contains("saved process list, state database, and adapter registry"),
        "help: {help}"
    );
}

fn read_members(archive: &Path) -> Vec<String> {
    let f = std::fs::File::open(archive).unwrap();
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    a.entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
        .collect()
}

#[test]
fn backup_is_private_and_written_to_the_requested_relative_path() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"[]").unwrap();
    std::fs::write(oxhome.join("adapters.db"), b"secret = 1").unwrap();
    std::fs::write(
        oxhome.join("notify.toml"),
        b"left over from an older release",
    )
    .unwrap();
    std::fs::create_dir(home.path().join("out")).unwrap();

    // Run under a permissive umask: the archive must still be 0600.
    let out = home
        .cmd()
        .args(["backup", "-o", "out/b.tar.gz"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let archive: PathBuf = home.path().join("out/b.tar.gz");
    assert!(
        archive.exists(),
        "backup must land at the relative path given"
    );
    let mode = std::fs::metadata(&archive).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        read_members(&archive),
        vec!["saved.json", "adapters.db"],
        "notify.toml is no longer a member, whatever is in the home"
    );
    assert!(
        listing(&oxhome).iter().all(|n| !n.ends_with(".tar.gz")),
        "no stray archive inside ~/.oxidepm"
    );
}

#[test]
fn backup_then_restore_roundtrip() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"[1,2,3]").unwrap();
    let out = home
        .cmd()
        .args(["backup", "-o", "b.tar.gz"])
        .output()
        .unwrap();
    assert!(out.status.success());
    std::fs::remove_file(oxhome.join("saved.json")).unwrap();
    let out = home.cmd().args(["restore", "b.tar.gz"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(oxhome.join("saved.json")).unwrap(),
        b"[1,2,3]"
    );
}

#[test]
fn backup_includes_the_state_database() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"[]").unwrap();
    std::fs::write(oxhome.join("state.db"), b"db bytes").unwrap();
    let out = home
        .cmd()
        .args(["backup", "-o", "b.tar.gz"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let archive: PathBuf = home.path().join("b.tar.gz");
    assert_eq!(read_members(&archive), vec!["saved.json", "state.db"]);

    std::fs::remove_file(oxhome.join("state.db")).unwrap();
    let out = home
        .cmd()
        .args(["restore", "b.tar.gz", "--yes"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read(oxhome.join("state.db")).unwrap(), b"db bytes");
}
