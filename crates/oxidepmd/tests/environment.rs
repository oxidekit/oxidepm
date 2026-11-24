//! The environment of an app: isolated from the daemon's unless the spec says `env_inherit`.
//! The daemon runs as a real process with a variable of its own that no app should see.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::daemon::{DaemonProc, Signal};
use oxidepm_core::{AppSpec, Runtime};
use serde_json::json;
use tempfile::TempDir;

const LEAK: (&str, &str) = ("OXIDEPM_TEST_LEAK", "1");

/// Start an app that writes its environment to `env.out` (one `KEY=value` per line) and then
/// sleeps; returns that environment as text once the app has written it.
fn env_of_app(
    daemon: &DaemonProc,
    workdir: &Path,
    name: &str,
    tweak: impl FnOnce(&mut AppSpec),
) -> String {
    let out = workdir.join(format!("{name}.env"));
    let script = format!(
        "env > '{}.tmp'; mv '{}.tmp' '{}'; exec sleep 300",
        out.display(),
        out.display(),
        out.display()
    );
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sh".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec!["-c".to_string(), script];
    tweak(&mut spec);
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&out) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never wrote its environment"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn has_var(env: &str, key: &str) -> bool {
    env.lines().any(|l| l.split('=').next() == Some(key))
}

fn var<'a>(env: &'a str, key: &str) -> Option<&'a str> {
    env.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

fn finish(mut daemon: DaemonProc) {
    daemon.signal(Signal::SIGTERM);
    daemon.wait_exit();
}

#[test]
fn env_inherit_false_hides_daemon_env() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let daemon = DaemonProc::spawn_with_env(&tmp.path().join("home"), &[LEAK]);

    let env = env_of_app(&daemon, &workdir, "isolated", |spec| {
        spec.env.insert("FROM_SPEC".to_string(), "yes".to_string());
    });
    assert!(
        !has_var(&env, "OXIDEPM_TEST_LEAK"),
        "the daemon's variable leaked:\n{env}"
    );
    assert!(
        !has_var(&env, "OXIDEPM_HOME"),
        "the daemon's home leaked:\n{env}"
    );
    assert!(
        has_var(&env, "PATH"),
        "PATH is part of the base set:\n{env}"
    );
    assert!(
        has_var(&env, "HOME"),
        "HOME is part of the base set:\n{env}"
    );
    assert_eq!(
        var(&env, "FROM_SPEC"),
        Some("yes"),
        "the spec's env is passed"
    );
    assert_eq!(var(&env, "OPM_APP_NAME"), Some("isolated"));
    assert!(var(&env, "OPM_APP_ID").is_some(), "{env}");
    finish(daemon);
}

#[test]
fn env_inherit_true_passes_daemon_env() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let daemon = DaemonProc::spawn_with_env(&tmp.path().join("home"), &[LEAK]);

    let env = env_of_app(&daemon, &workdir, "inheriting", |spec| {
        spec.env_inherit = true;
        spec.env.insert("FROM_SPEC".to_string(), "yes".to_string());
    });
    assert_eq!(
        var(&env, "OXIDEPM_TEST_LEAK"),
        Some("1"),
        "the daemon's variable is passed:\n{env}"
    );
    assert!(has_var(&env, "PATH"));
    assert_eq!(var(&env, "FROM_SPEC"), Some("yes"));
    assert_eq!(var(&env, "OPM_APP_NAME"), Some("inheriting"));
    finish(daemon);
}
