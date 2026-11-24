//! Validation and compensation at the single entry of `Supervisor::start` and `resurrect`:
//! a spec that is invalid is refused before anything exists (no row, no file, no process), and
//! a start that fails after its row was written takes that row back out.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::daemon::DaemonProc;
use common::fixture::{install, InstallSpec};
use common::*;
use oxidepm_core::{AppSpec, Error, ErrorCode, Runtime};
use serde_json::json;

/// The error of a start that must have been refused as an invalid spec or name.
fn assert_invalid(result: Result<u32, Error>, what: &str) {
    match result {
        Err(e @ Error::InvalidSpec(_)) => {
            assert!(
                matches!(e.code(), ErrorCode::InvalidSpec | ErrorCode::InvalidName),
                "{what}: wrong code {:?}",
                e.code()
            );
        }
        other => panic!("{what}: expected InvalidSpec, got {other:?}"),
    }
}

/// A spec that would write `ran` if it were ever launched.
fn touching(h: &Harness, name: &str, ran: &std::path::Path) -> AppSpec {
    h.cmd(name, &format!("touch '{}'; exec sleep 300", ran.display()))
}

async fn assert_nothing_exists(h: &Harness, ran: &std::path::Path, what: &str) {
    assert!(h.status().await.is_empty(), "{what}: a row was left behind");
    assert!(!ran.exists(), "{what}: the command was launched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_path_traversal_name_and_creates_no_file() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    for bad in ["../../evil", "../evil", "/abs/evil", "a/b", "a b", ""] {
        let mut spec = touching(&h, "placeholder", &ran);
        spec.name = bad.to_string();
        assert_invalid(h.start(spec).await, &format!("name {bad:?}"));
    }
    assert_nothing_exists(&h, &ran, "traversal names").await;

    // log_path("../../evil", ..) would land two levels above the logs directory
    let escaped = h.paths.log_path("../../evil", "out");
    assert!(
        !escaped.exists(),
        "no file outside the logs dir: {}",
        escaped.display()
    );
    let stray: Vec<PathBuf> = std::fs::read_dir(h.dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("evil"))
        })
        .collect();
    assert!(stray.is_empty(), "stray files: {stray:?}");
    assert!(
        !h.paths.logs_dir().exists()
            || std::fs::read_dir(h.paths.logs_dir())
                .unwrap()
                .next()
                .is_none(),
        "no log file at all"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_reserved_names() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    let long = "a".repeat(65);
    for bad in ["all", "ALL", "All", "123", "@x", "-x", long.as_str()] {
        let mut spec = touching(&h, "placeholder", &ran);
        spec.name = bad.to_string();
        assert_invalid(h.start(spec).await, &format!("name {bad:?}"));
    }
    assert_nothing_exists(&h, &ran, "reserved names").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_instances_zero_and_huge() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    for bad in [0u32, 1_000_000_000] {
        let mut spec = touching(&h, "inst", &ran);
        spec.instances = bad;
        assert_invalid(h.start(spec).await, &format!("instances {bad}"));
    }
    assert_nothing_exists(&h, &ran, "instances").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_empty_command_and_relative_cwd() {
    let h = Harness::new().await;
    let ran = h.file("ran");

    let mut empty = touching(&h, "empty-cmd", &ran);
    empty.command = "  ".to_string();
    assert_invalid(h.start(empty).await, "empty command");

    let mut relative = touching(&h, "rel-cwd", &ran);
    relative.cwd = PathBuf::from("some/relative/dir");
    assert_invalid(h.start(relative).await, "relative cwd");

    assert_nothing_exists(&h, &ran, "command and cwd").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_invalid_health_params() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    let probes = [
        json!({ "http_url": "http://127.0.0.1:1/", "interval_secs": 0 }),
        json!({ "http_url": "http://127.0.0.1:1/", "timeout_secs": 0 }),
        json!({ "http_url": "http://127.0.0.1:1/", "retries": 0 }),
        json!({ "http_url": "http://127.0.0.1:1/", "interval_secs": 2, "timeout_secs": 5 }),
        json!({ "script": "true", "interval_secs": 0 }),
        json!({ "script": "true", "timeout_secs": 0 }),
        json!({ "script": "true", "retries": 0 }),
        json!({ "script": "true", "interval_secs": 2, "timeout_secs": 5 }),
    ];
    for (i, probe) in probes.iter().enumerate() {
        let mut check = json!({ "expected_status": [200], "interval_secs": 5, "timeout_secs": 2, "retries": 1 });
        for (k, v) in probe.as_object().unwrap() {
            check[k] = v.clone();
        }
        let spec = h.cmd_with(
            &format!("probe-{i}"),
            &format!("touch '{}'; exec sleep 300", ran.display()),
            json!({ "health_check": check }),
        );
        assert_invalid(h.start(spec).await, &format!("health probe {probe}"));
    }
    assert_nothing_exists(&h, &ran, "health params").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_rejects_log_max_size_zero_and_bad_tags() {
    let h = Harness::new().await;
    let ran = h.file("ran");

    let mut size = touching(&h, "log-size", &ran);
    size.log_max_size = Some(0);
    assert_invalid(h.start(size).await, "log_max_size 0");

    for tag in ["", "has space", "bad/tag"] {
        let mut spec = touching(&h, "tagged", &ran);
        spec.tags = vec![tag.to_string()];
        assert_invalid(h.start(spec).await, &format!("tag {tag:?}"));
    }
    assert_nothing_exists(&h, &ran, "log size and tags").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_leaves_no_row() {
    let h = Harness::new().await;

    let mut missing = h.cmd("no-such-cmd", "true");
    missing.command = "/nonexistent/oxidepm-test-binary".to_string();
    missing.args = Vec::new();
    h.start(missing)
        .await
        .expect_err("a command that does not exist cannot start");
    assert!(
        h.status().await.is_empty(),
        "the row of the failed start is gone"
    );

    let mut no_dir = h.cmd("no-such-dir", "exec sleep 300");
    no_dir.cwd = h.file("does-not-exist");
    h.start(no_dir)
        .await
        .expect_err("a missing cwd cannot start");
    assert!(
        h.status().await.is_empty(),
        "no row for a missing cwd either"
    );
    assert!(
        !h.paths.log_path("no-such-dir", "out").exists(),
        "and no log file for it"
    );

    // the name is free again: the same name now starts
    let m = h.marker();
    h.start(h.cmd("no-such-cmd", &format!("exec sleep {m}")))
        .await
        .expect("retry starts");
    assert_eq!(h.status().await.len(), 1);
    h.stop("no-such-cmd").await.expect("stop");
}

/// A failed start of an app that already had a row (stopped earlier) must not take that row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_keeps_the_row_of_an_existing_app() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("keep-row", &format!("exec sleep {m}")))
        .await
        .expect("start");
    h.stop("keep-row").await.expect("stop");

    let mut broken = h.cmd("keep-row", "true");
    broken.command = "/nonexistent/oxidepm-test-binary".to_string();
    broken.args = Vec::new();
    h.start(broken).await.expect_err("cannot start");

    let rows = h.status().await;
    assert_eq!(rows.len(), 1, "the earlier app is still registered");
    assert_eq!(rows[0].spec.name, "keep-row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_rejects_port_range_smaller_than_instances() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    let mut spec = touching(&h, "narrow", &ran);
    spec.instances = 3;
    spec.port_range = Some((41000, 41001));
    assert_invalid(h.start(spec).await, "port range of 2 for 3 instances");
    assert_nothing_exists(&h, &ran, "narrow port range").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_with_both_port_and_port_range_is_rejected() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    let mut spec = touching(&h, "both-ports", &ran);
    spec.instances = 2;
    spec.port = Some(41100);
    spec.port_range = Some((41100, 41110));
    assert_invalid(h.start(spec).await, "port and port_range");
    assert_nothing_exists(&h, &ran, "port and port_range").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_five_field_expression_is_accepted() {
    let h = Harness::new().await;
    let m = h.marker();
    let mut spec = h.cmd("cron-five", &format!("exec sleep {m}"));
    spec.restart_cron = Some("0 4 * * *".to_string());
    h.start(spec)
        .await
        .expect("a 5-field cron expression starts");
    h.eventually(
        "the restart timer to be armed",
        Duration::from_secs(5),
        || h.sup.cron_tasks_armed() == 1,
    )
    .await;
    h.stop("cron-five").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_cron_is_rejected_at_start() {
    let h = Harness::new().await;
    let ran = h.file("ran");
    let mut spec = touching(&h, "cron-bad", &ran);
    spec.restart_cron = Some("nonsense".to_string());
    assert_invalid(h.start(spec).await, "cron 'nonsense'");
    assert_nothing_exists(&h, &ran, "invalid cron").await;
    assert_eq!(h.sup.cron_tasks_armed(), 0);
}

/// A client does not choose `id` or `instance_id`: a spec that carries them starts as a plain
/// app with a fresh id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_ignores_client_chosen_id_and_instance_id() {
    let h = Harness::new().await;
    let m = h.marker();
    let mut spec = h.cmd("chosen-id", &format!("exec sleep {m}"));
    spec.id = 4242;
    spec.instance_id = Some(7);
    let id = h.start(spec).await.expect("start");
    assert_ne!(id, 4242);
    let state = h.state("chosen-id").await;
    assert_eq!(state.instance_id, None);
    assert_eq!(state.app_id, id);
    h.stop("chosen-id").await.expect("stop");
}

/// An entry of saved.json that no longer validates is skipped with its reason; it leaves no row
/// and does not keep the valid entries from starting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_skips_an_invalid_saved_app_and_leaves_no_row() {
    let h = Harness::new().await;
    let m = h.marker();
    let good = h.cmd("saved-good", &format!("exec sleep {m}"));
    let mut bad = h.cmd("placeholder", "exec sleep 300");
    bad.name = "../../saved-evil".to_string();
    let mut zero = h.cmd("saved-zero", "exec sleep 300");
    zero.instances = 0;
    std::fs::write(h.paths.saved(), saved_json(&[bad, zero, good])).expect("write saved.json");

    assert_eq!(h.resurrect().await.expect("resurrect"), 1);
    let names: Vec<String> = h.status().await.into_iter().map(|a| a.spec.name).collect();
    assert_eq!(
        names,
        vec!["saved-good".to_string()],
        "only the valid app has a row"
    );
    assert!(!h.paths.log_path("../../saved-evil", "out").exists());
    h.stop("saved-good").await.expect("stop");
}

/// A saved cluster child is not an ingress spec: the cluster's own entry recreates it, and the
/// child must not start as an app of its own (nor as a cluster of clusters).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_does_not_start_saved_cluster_children_on_their_own() {
    let h = Harness::new().await;
    let m = h.marker();
    let mut parent = h.cmd("saved-cluster", &format!("exec sleep {m}"));
    parent.instances = 2;
    // the children come first, as they may in a file written from the process map
    let saved = vec![
        parent.for_instance(0, None),
        parent.for_instance(1, None),
        parent,
    ];
    std::fs::write(h.paths.saved(), saved_json(&saved)).expect("write saved.json");

    h.resurrect().await.expect("resurrect");
    let mut names: Vec<String> = h.status().await.into_iter().map(|a| a.spec.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["saved-cluster", "saved-cluster-0", "saved-cluster-1"],
        "one parent and its two children, nothing else"
    );
    assert_eq!(Harness::procs_with_cmdline(&format!("sleep {m}")).len(), 2);
    h.delete("saved-cluster").await.expect("delete cluster");
}

fn assert_start_requires_watch_adapter(home: &Path, cwd: &Path, message: &str) {
    let daemon = DaemonProc::spawn(home);
    let ran = cwd.join("ran");
    let mut spec = AppSpec::new(
        "watched".into(),
        Runtime::CMD,
        "/bin/sh".into(),
        cwd.to_path_buf(),
    );
    spec.args = vec!["-c".into(), "touch ran; exec sleep 300".into()];
    spec.adapters.insert("watch".into(), json!({}));
    let reply = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"].as_str().unwrap().contains(message),
        "{reply}"
    );
    assert_eq!(daemon.rpc(json!({ "type": "status" }))["apps"], json!([]));
    assert!(!ran.exists(), "the command was launched");
}

#[test]
fn start_with_missing_watch_adapter_returns_adapter_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert_start_requires_watch_adapter(
        &dir.path().join("home"),
        dir.path(),
        "adapter 'watch' is not enabled. Run: oxidepm adapter import watch --enable",
    );
}

#[tokio::test]
async fn start_with_disabled_watch_adapter_returns_adapter_missing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    install(
        &home,
        InstallSpec {
            enabled: false,
            ..InstallSpec::service("watch", json!([]))
        },
    )
    .await;
    assert_start_requires_watch_adapter(
        &home,
        dir.path(),
        "adapter 'watch' is installed but not enabled. Run: oxidepm adapter enable watch",
    );
}

/// `command` is a program: a relative path in it is relative to the app's cwd, and the check
/// (`prepare`) and the launch resolve it the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cmd_relative_program_resolves_against_cwd() {
    let h = Harness::new().await;
    let m = h.marker();
    let ran = h.file("relative.ran");
    h.script(
        "run.sh",
        &format!("touch '{}'; exec sleep {m}", ran.display()),
    );

    let mut spec = h.cmd("rel-prog", "true");
    spec.command = "./run.sh".to_string();
    spec.args = Vec::new();

    let prepared = oxidepm_runtime::get_runner(&Runtime::CMD)
        .unwrap()
        .prepare(&spec)
        .await
        .expect("prepare");
    assert!(prepared.success, "prepare agrees: {}", prepared.output);

    h.start(spec).await.expect("start");
    h.wait_file(&ran).await;
    let pid = h.pid_of("rel-prog").await;
    assert!(Harness::pid_alive(pid));
    assert_eq!(h.stop("rel-prog").await.expect("stop"), 1);
}

/// `command="sleep 1"` used to pass `prepare` (which looked at the first word) and fail at the
/// launch (which executed the whole string). It now fails once, at the start, and says how to fix it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cmd_multiword_command_gets_clear_error() {
    let h = Harness::new().await;

    let mut spec = h.cmd("multi-word", "true");
    spec.command = "sleep 1".to_string();
    spec.args = Vec::new();
    let err = h
        .start(spec)
        .await
        .expect_err("a shell line is not a program")
        .to_string();
    assert!(err.contains("args"), "{err}");
    assert!(err.contains("command='sleep', args=['1']"), "{err}");
    assert!(h.status().await.is_empty(), "no row is left behind");
}
