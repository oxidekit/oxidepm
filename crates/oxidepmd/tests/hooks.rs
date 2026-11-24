//! Characterisation of lifecycle hooks.

mod common;

use std::time::{Duration, Instant};

use common::daemon::{DaemonProc, Signal};
use common::*;
use oxidepm_core::{AppSpec, Runtime};
use serde_json::json;
use tempfile::TempDir;

/// Shell snippet that writes `body` to `path` atomically (so a reader never sees a partial file).
fn write_atomically(path: &std::path::Path, body: &str) -> String {
    format!(
        "{body} > '{p}.tmp' && mv '{p}.tmp' '{p}'",
        body = body,
        p = path.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_start_hook_runs_with_opm_env() {
    let h = Harness::new().await;
    let m = h.marker();
    let out = h.file("start-hook.out");
    let hook = write_atomically(
        &out,
        "printf '%s %s %s\\n' \"$OPM_APP_NAME\" \"$OPM_EVENT\" \"$OPM_PID\"",
    );
    h.start(h.cmd_with(
        "hook-start",
        &format!("exec sleep {m}"),
        json!({ "hooks": { "on_start": hook } }),
    ))
    .await
    .expect("start");
    let pid = h.pid_of("hook-start").await;

    let content = h.wait_file(&out).await;
    assert_eq!(content.trim(), format!("hook-start start {pid}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_stop_hook_runs_after_child_is_dead() {
    let h = Harness::new().await;
    let m = h.marker();
    let out = h.file("stop-hook.out");
    let hook = write_atomically(
        &out,
        "if kill -0 \"$OPM_PID\" 2>/dev/null; then r=alive; else r=dead; fi; echo \"$OPM_EVENT $r\"",
    );
    h.start(h.cmd_with(
        "hook-stop",
        &format!("exec sleep {m}"),
        json!({ "hooks": { "on_stop": hook } }),
    ))
    .await
    .expect("start");

    assert_eq!(h.stop("hook-stop").await.expect("stop"), 1);

    let content = h.wait_file(&out).await;
    assert_eq!(content.trim(), "stop dead");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unhealthy_transition_fires_on_error_hook_once() {
    let h = Harness::new().await;
    let m = h.marker();
    let runs = h.file("error-hook.runs");
    let hook = format!("echo run >> '{}'", runs.display());
    // The failing probe marks the app unhealthy after one failure.
    let mut overlay = failing_probe();
    overlay["hooks"] = json!({ "on_error": hook });
    h.start(h.cmd_with("hook-error", &format!("exec sleep {m}"), overlay))
        .await
        .expect("start");

    h.eventually("the error hook to run", Duration::from_secs(20), || {
        Harness::line_count(&runs) >= 1
    })
    .await;
    // Four more probe intervals: a hook that fired on every failed probe would show up here.
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert_eq!(Harness::line_count(&runs), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_output_goes_to_hook_log_file() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "hook-log",
        &format!("exec sleep {m}"),
        json!({ "hooks": { "on_start": "echo out-line; echo err-line >&2", "on_stop": "echo stop-line" } }),
    ))
    .await
    .expect("start");
    let log = h.paths.hooks_log("hook-log");
    h.eventually("the start hook's output", Duration::from_secs(10), || {
        std::fs::read_to_string(&log)
            .map(|t| t.contains("out-line") && t.contains("err-line"))
            .unwrap_or(false)
    })
    .await;

    // later runs are appended, not written over the earlier ones
    assert_eq!(h.stop("hook-log").await.expect("stop"), 1);
    h.eventually("the stop hook's output", Duration::from_secs(10), || {
        std::fs::read_to_string(&log)
            .map(|t| t.contains("stop-line"))
            .unwrap_or(false)
    })
    .await;
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("out-line") && text.contains("err-line"),
        "{text:?}"
    );
}

/// A marker (a `sleep` duration literal) that nothing else on the machine carries.
fn marker(tag: u32) -> String {
    format!("302.{:05}{:03}", std::process::id() % 100_000, tag)
}

fn start_app(daemon: &DaemonProc, workdir: &std::path::Path, name: &str, hooks: serde_json::Value) {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sh".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec!["-c".to_string(), "exec sleep 300".to_string()];
    let mut value = serde_json::to_value(&spec).unwrap();
    value["hooks"] = hooks;
    let started = daemon.rpc(json!({ "type": "start", "spec": value }));
    assert_eq!(started["type"], "started", "{started}");
}

fn wait_for(what: &str, cap: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + cap;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The hook runs in the daemon's directory, so the scripts below `cd` into the test's own
/// directory first: the leak guard then finds a hook that a failing test leaves behind.
#[test]
fn hung_hook_is_killed_with_its_children_on_timeout() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let m = marker(1);
    let needle = format!("sleep {m}");
    let daemon = DaemonProc::spawn_with_env(
        &tmp.path().join("home"),
        &[("OXIDEPM_HOOK_TIMEOUT_MS", "500")],
    );

    start_app(
        &daemon,
        &workdir,
        "hung-hook",
        json!({ "on_start": format!("cd '{}'; sleep {m} & wait", workdir.display()) }),
    );
    wait_for("the hook's sleep to start", Duration::from_secs(10), || {
        !Harness::procs_with_cmdline(&needle).is_empty()
    });
    // the timeout is 0.5 s; two seconds later neither the hook nor its sleep may be left
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        Harness::procs_with_cmdline(&needle).is_empty(),
        "the hook's child outlived the timeout"
    );
}

#[test]
fn drain_kills_running_hook_groups_on_expiry() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let m = marker(2);
    let needle = format!("sleep {m}");
    let started = workdir.join("stop-hook.started");
    let mut daemon = DaemonProc::spawn_with_env(
        &tmp.path().join("home"),
        &[
            ("OXIDEPM_HOOK_TIMEOUT_MS", "120000"),
            ("OXIDEPM_HOOK_DRAIN_MS", "1000"),
        ],
    );

    start_app(
        &daemon,
        &workdir,
        "drain-hook",
        json!({ "on_stop": format!("cd '{}'; touch '{}'; sleep {m} & wait", workdir.display(), started.display()) }),
    );
    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit_for(Duration::from_secs(20));
    let took = signalled.elapsed();
    assert!(started.exists(), "the stop hook ran during the shutdown");
    // A 1 s drain followed by the kill of the hook's group takes about 1.3 s. Without that
    // kill the daemon only exits when the runtime drops the hook task (about 6 s).
    assert!(
        took < Duration::from_secs(3),
        "the drain did not kill the hook's group: exit took {took:?}"
    );
    assert!(status.success(), "a graceful shutdown exits 0: {status:?}");
    // the daemon is gone; the hook it waited for one second for must be gone with it
    wait_for(
        "the hook's group to be killed",
        Duration::from_secs(3),
        || Harness::procs_with_cmdline(&needle).is_empty(),
    );
}

/// The daemon carries a variable of its own (as a secret in its environment would): a hook gets
/// the base set, the app's own `env` and `OPM_*`, and nothing else.
#[test]
fn hook_does_not_inherit_daemon_environment() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let out = workdir.join("hook.env");
    let daemon =
        DaemonProc::spawn_with_env(&tmp.path().join("home"), &[("OPM_TEST_SECRET", "hunter2")]);

    let mut spec = AppSpec::new(
        "hook-env".to_string(),
        Runtime::CMD,
        "sh".to_string(),
        workdir.clone(),
    );
    spec.args = vec!["-c".to_string(), "exec sleep 300".to_string()];
    spec.env.insert("FROM_SPEC".to_string(), "yes".to_string());
    spec.env_inherit = true; // the app may inherit the daemon's environment; its hooks may not
    spec.hooks.on_start = Some(format!(
        "env > '{o}.tmp'; mv '{o}.tmp' '{o}'",
        o = out.display()
    ));
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");

    let mut env = String::new();
    wait_for(
        "the hook to write its environment",
        Duration::from_secs(10),
        || {
            env = std::fs::read_to_string(&out).unwrap_or_default();
            !env.is_empty()
        },
    );
    assert!(
        !env.contains("OPM_TEST_SECRET"),
        "the daemon's variable leaked:\n{env}"
    );
    assert!(
        !env.contains("OXIDEPM_HOME"),
        "the daemon's home leaked:\n{env}"
    );
    assert!(
        env.lines().any(|l| l == "FROM_SPEC=yes"),
        "the app's own env is missing:\n{env}"
    );
    assert!(env.lines().any(|l| l == "OPM_APP_NAME=hook-env"), "{env}");
    assert!(env.lines().any(|l| l == "OPM_EVENT=start"), "{env}");
    assert!(
        env.lines().any(|l| l.starts_with("PATH=")),
        "the base set has PATH:\n{env}"
    );
}
