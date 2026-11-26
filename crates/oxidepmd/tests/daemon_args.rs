use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str], code: i32) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxidepmd"))
        .args(args)
        .env("HOME", dir.path())
        .env("OXIDEPM_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxidepmd");
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().unwrap();
            panic!(
                "oxidepmd {args:?} did not exit; home created: {}",
                home.exists()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!home.exists(), "oxidepmd {args:?} created its home");
    assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
    output
}

fn assert_version(flag: &str) {
    let output = run(&[flag], 0);
    assert_eq!(
        output.stdout,
        concat!("oxidepmd ", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
    );
    assert!(output.stderr.is_empty());
}

fn assert_help(flag: &str) {
    let output = run(&[flag], 0);
    let stdout = String::from_utf8(output.stdout).unwrap();
    for text in [
        "Usage: oxidepmd",
        "--check",
        "--version",
        "-V",
        "--help",
        "-h",
    ] {
        assert!(stdout.contains(text), "{stdout:?}");
    }
    assert!(output.stderr.is_empty());
}

fn assert_unknown(args: &[&str], arg: &str) {
    let output = run(args, 2);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.lines().next().unwrap(),
        format!("oxidepmd: unknown argument '{arg}'")
    );
    assert!(stderr
        .lines()
        .skip(1)
        .any(|line| line.contains("oxidepmd --help")));
}

#[test]
fn version_prints_the_daemon_version() {
    assert_version("--version");
}

#[test]
fn short_version_prints_the_daemon_version() {
    assert_version("-V");
}

#[test]
fn help_lists_daemon_options() {
    assert_help("--help");
}

#[test]
fn short_help_lists_daemon_options() {
    assert_help("-h");
}

#[test]
fn an_unknown_argument_is_rejected() {
    assert_unknown(&["--unknown"], "--unknown");
}

#[test]
fn a_second_argument_is_rejected() {
    assert_unknown(&["--check", "--version"], "--version");
}

#[test]
fn the_unknown_argument_is_named_before_a_known_one() {
    assert_unknown(&["--bogus", "--version"], "--bogus");
}
