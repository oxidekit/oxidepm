//! What the daemon binary is built from. The daemon keeps no HTTP client library: fetching,
//! signature checks and archive extraction run in the CLI, never in the process that supervises
//! the apps and the adapters.

use std::process::Command;

/// The names of every crate `package` is built from (normal and build dependencies; the tests'
/// own dependencies are not part of the binary).
fn crates_of(package: &str) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            package,
            "-e",
            "normal,build",
            "--prefix",
            "none",
            "--offline",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    assert!(
        names.iter().any(|n| n == "oxidepm-core"),
        "the tree looks wrong: {names:?}"
    );
    names
}

#[test]
fn daemon_binary_has_no_reqwest_dependency() {
    let names = crates_of("oxidepmd");
    for banned in ["reqwest", "oxidepm-fetch", "tar"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "the daemon depends on {banned}"
        );
    }
}

/// The terminal dashboard is an adapter now: neither binary is built with a terminal UI library.
#[test]
fn no_binary_is_built_with_a_terminal_ui() {
    for package in ["oxidepmd", "oxidepm"] {
        let names = crates_of(package);
        for banned in ["ratatui", "crossterm", "oxidepm-tui"] {
            assert!(
                !names.iter().any(|n| n == banned),
                "{package} is built with {banned}"
            );
        }
    }
}

/// The web server is an adapter now: neither binary is built with a web framework, and the
/// daemon has no TLS server library either (its probe is a client).
#[test]
fn no_binary_is_built_with_a_web_server() {
    for package in ["oxidepmd", "oxidepm"] {
        let names = crates_of(package);
        for banned in ["axum", "tower-http", "oxidepm-web", "tungstenite"] {
            assert!(
                !names.iter().any(|n| n == banned),
                "{package} is built with {banned}"
            );
        }
    }
}
