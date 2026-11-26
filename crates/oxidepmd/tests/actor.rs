//! The structural bugs the actor closes: a stop, a delete or a restart that lands while a crash
//! restart is pending, the exit of a group leader, how a child's death is recorded, what a crash
//! restart and a cron restart fire, clusters, and reload.

mod common;

use std::time::Duration;

use common::*;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use oxidepm_core::AppStatus;
use serde_json::json;

fn kill_child(pid: u32) {
    kill(Pid::from_raw(pid as i32), Signal::SIGKILL).expect("kill the child we started");
}

/// A long restart delay, so the test has time to act inside it.
fn slow_restart() -> serde_json::Value {
    json!({ "restart_policy": { "restart_delay_ms": 1500, "max_restarts": 5 } })
}

/// Wait for the app to enter restart backoff after a crash.
async fn wait_for_crash(h: &Harness, name: &str) {
    h.wait_until(name, "the crash to be seen", |s| {
        s.status == AppStatus::Backoff || (s.status == AppStatus::Errored && s.pid.is_none())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_backoff_cancels_pending_restart() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("act-bo-stop", &format!("exec sleep {m}"), slow_restart()))
        .await
        .expect("start");
    kill_child(h.pid_of("act-bo-stop").await);
    wait_for_crash(&h, "act-bo-stop").await;
    let restarts = h.state("act-bo-stop").await.restarts;

    h.stop("act-bo-stop").await.expect("stop during the delay");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        Harness::procs_with_cmdline(&needle).is_empty(),
        "the pending restart came back"
    );
    let state = h.state("act-bo-stop").await;
    assert_eq!(state.status, AppStatus::Stopped);
    assert_eq!(state.restarts, restarts, "no restart after the stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_during_backoff_leaves_no_process() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("act-bo-del", &format!("exec sleep {m}"), slow_restart()))
        .await
        .expect("start");
    kill_child(h.pid_of("act-bo-del").await);
    wait_for_crash(&h, "act-bo-del").await;

    assert_eq!(
        h.delete("act-bo-del")
            .await
            .expect("delete during the delay"),
        1
    );

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        Harness::procs_with_cmdline(&needle).is_empty(),
        "the pending restart came back"
    );
    assert!(h.status().await.is_empty(), "no row left");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_during_backoff_yields_exactly_one_process() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("act-bo-restart", &format!("exec sleep {m}"), slow_restart()))
        .await
        .expect("start");
    kill_child(h.pid_of("act-bo-restart").await);
    wait_for_crash(&h, "act-bo-restart").await;

    assert_eq!(
        h.restart("act-bo-restart")
            .await
            .expect("restart during the delay"),
        1
    );

    tokio::time::sleep(Duration::from_secs(3)).await;
    let procs = Harness::procs_with_cmdline(&needle);
    assert_eq!(
        procs.len(),
        1,
        "exactly one process after the delay is over: {procs:?}"
    );
    let state = h.state("act-bo-restart").await;
    assert_eq!(state.status, AppStatus::Running);
    assert_eq!(state.pid, Some(procs[0]));
    assert_eq!(
        state.restarts, 1,
        "one restart, not the scheduled one as well"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_exit_reaps_remaining_group_members() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with(
        "act-leader",
        &format!("sleep {m} & exit 1"),
        json!({ "restart_policy": { "auto_restart": false } }),
    ))
    .await
    .expect("start");

    h.wait_until("act-leader", "the exit to be seen", |s| {
        s.last_exit_code == Some(1)
    })
    .await;
    h.eventually(
        "the group member to be gone",
        Duration::from_secs(3),
        || Harness::procs_with_cmdline(&needle).is_empty(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_death_by_signal_records_signal() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "act-signal",
        &format!("exec sleep {m}"),
        json!({ "restart_policy": { "auto_restart": false } }),
    ))
    .await
    .expect("start");
    kill_child(h.pid_of("act-signal").await);

    let state = h
        .wait_until("act-signal", "the death to be recorded", |s| {
            s.status == AppStatus::Errored && s.pid.is_none()
        })
        .await;
    assert_eq!(state.last_exit_signal, Some(9));
    assert_eq!(state.last_exit_code, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_restart_fires_on_restart_hook() {
    let h = Harness::new().await;
    let m = h.marker();
    let marker = h.file("restart-hook.out");
    h.start(h.cmd_with(
        "act-hook",
        &format!("exec sleep {m}"),
        json!({
            "restart_policy": { "restart_delay_ms": 20, "max_restarts": 5 },
            "hooks": { "on_restart": format!("echo restarted > '{}'", marker.display()) }
        }),
    ))
    .await
    .expect("start");
    kill_child(h.pid_of("act-hook").await);

    let content = h.wait_file(&marker).await;
    assert_eq!(content.trim(), "restarted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_restart_with_restart_mode_never_still_restarts() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "act-cron-never",
        &format!("exec sleep {m}"),
        json!({ "restart_mode": "never", "restart_cron": "* * * * * *" }),
    ))
    .await
    .expect("start");
    let first = h.pid_of("act-cron-never").await;

    h.wait_until_for(
        "act-cron-never",
        "a new pid from the cron restart",
        Duration::from_secs(5),
        |s| s.status == AppStatus::Running && s.pid.map(|p| p != first).unwrap_or(false),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_restart_is_not_a_crash() {
    let h = Harness::new().await;
    let m = h.marker();
    let (crashed, restarted) = (h.file("crash-hook.out"), h.file("restart-hook.out"));
    h.start(h.cmd_with(
        "act-cron-hook",
        &format!("exec sleep {m}"),
        json!({
            "restart_cron": "* * * * * *",
            "hooks": {
                "on_crash": format!("echo crashed > '{}'", crashed.display()),
                "on_restart": format!("echo restarted > '{}'", restarted.display())
            }
        }),
    ))
    .await
    .expect("start");

    h.wait_file(&restarted).await;
    // give a late crash hook the time it would need
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!crashed.exists(), "a cron restart is not a crash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_still_fires_after_manual_restart_and_after_crash_restart() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "act-cron-keeps",
        &format!("exec sleep {m}"),
        json!({
            "restart_cron": "*/2 * * * * *",
            "restart_policy": { "restart_delay_ms": 20, "max_restarts": 20 }
        }),
    ))
    .await
    .expect("start");

    // a manual restart, then the cron must still change the pid by itself
    h.restart("act-cron-keeps").await.expect("manual restart");
    let after_manual = h.pid_of("act-cron-keeps").await;
    h.wait_until_for(
        "act-cron-keeps",
        "a cron restart after the manual one",
        Duration::from_secs(6),
        |s| s.status == AppStatus::Running && s.pid.map(|p| p != after_manual).unwrap_or(false),
    )
    .await;

    // a crash restart, then the cron must still change the pid by itself
    let killed = h.pid_of("act-cron-keeps").await;
    kill_child(killed);
    let after_crash = h
        .wait_until_for(
            "act-cron-keeps",
            "the crash restart",
            Duration::from_secs(6),
            |s| s.status == AppStatus::Running && s.pid.map(|p| p != killed).unwrap_or(false),
        )
        .await
        .pid
        .expect("pid");
    h.wait_until_for(
        "act-cron-keeps",
        "a cron restart after the crash one",
        Duration::from_secs(6),
        |s| s.status == AppStatus::Running && s.pid.map(|p| p != after_crash).unwrap_or(false),
    )
    .await;
    h.stop("act-cron-keeps").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_with_prepare_failure_ends_errored_with_build_error() {
    let h = Harness::new().await;
    let m = h.marker();
    // a program that exists for the first start and is gone for the restart
    let program = h.script("act-prog.sh", &format!("exec sleep {m}"));
    let mut spec = h.cmd("act-prepare", "true");
    spec.command = program.display().to_string();
    spec.args = Vec::new();
    h.start(spec).await.expect("start");
    let old = h.pid_of("act-prepare").await;
    std::fs::remove_file(&program).expect("remove the program");

    let err = h
        .restart("act-prepare")
        .await
        .expect_err("the build step fails");
    assert!(matches!(err, oxidepm_core::Error::BuildFailed(_)), "{err}");

    let state = h.state("act-prepare").await;
    assert_eq!(state.status, AppStatus::Errored);
    assert_eq!(state.pid, None);
    assert!(
        state.last_error.is_some(),
        "the reason is recorded: {state:?}"
    );
    assert!(!Harness::pid_alive(old), "the old child was stopped");
}

fn cluster(h: &Harness, name: &str, marker: &str, instances: u32) -> oxidepm_core::AppSpec {
    h.cmd_with(
        name,
        &format!("exec sleep {marker}"),
        json!({ "instances": instances }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_child_restart_keeps_row_and_goes_running() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "act-cl-child", &m, 2))
        .await
        .expect("start cluster");
    let rows = h.status().await.len();
    let old = h.pid_of("act-cl-child-1").await;

    assert_eq!(
        h.restart("act-cl-child-1")
            .await
            .expect("restart the instance"),
        1
    );

    let state = h.state("act-cl-child-1").await;
    assert_eq!(state.status, AppStatus::Running);
    assert_ne!(state.pid, Some(old));
    assert_eq!(h.status().await.len(), rows, "no row was added or lost");
    assert_eq!(Harness::procs_with_cmdline(&format!("sleep {m}")).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_stop_stops_every_child_and_parent() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "act-cl-stop", &m, 3))
        .await
        .expect("start cluster");

    assert_eq!(h.stop("act-cl-stop").await.expect("stop the cluster"), 1);

    h.eventually("every instance to be gone", Duration::from_secs(3), || {
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    })
    .await;
    for name in [
        "act-cl-stop",
        "act-cl-stop-0",
        "act-cl-stop-1",
        "act-cl-stop-2",
    ] {
        assert_eq!(h.state(name).await.status, AppStatus::Stopped, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_delete_removes_children_rows_and_processes() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "act-cl-del", &m, 3))
        .await
        .expect("start cluster");

    assert_eq!(h.delete("act-cl-del").await.expect("delete the cluster"), 1);

    h.eventually("every instance to be gone", Duration::from_secs(3), || {
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    })
    .await;
    assert!(
        h.status().await.is_empty(),
        "no rows: {:?}",
        h.status().await.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_can_be_restarted_after_stop() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "act-cl-again", &m, 3))
        .await
        .expect("start cluster");
    h.stop("act-cl-again").await.expect("stop the cluster");
    h.eventually("every instance to be gone", Duration::from_secs(3), || {
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    })
    .await;

    h.start(cluster(&h, "act-cl-again", &m, 3))
        .await
        .expect("start the cluster again");

    for name in [
        "act-cl-again",
        "act-cl-again-0",
        "act-cl-again-1",
        "act-cl-again-2",
    ] {
        assert_eq!(h.state(name).await.status, AppStatus::Running, "{name}");
    }
    assert_eq!(Harness::procs_with_cmdline(&format!("sleep {m}")).len(), 3);
    assert_eq!(h.status().await.len(), 4, "no rows were added");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_single_keeps_name_id_and_row() {
    let h = Harness::new().await;
    let m = h.marker();
    let id = h
        .start(h.cmd("act-reload", &format!("exec sleep {m}")))
        .await
        .expect("start");
    let old = h.pid_of("act-reload").await;

    assert_eq!(h.reload("act-reload").await.expect("reload"), 1);

    let apps = h.status().await;
    assert_eq!(apps.len(), 1, "no second entry");
    assert_eq!(apps[0].spec.name, "act-reload");
    assert_eq!(apps[0].spec.id, id);
    assert_eq!(apps[0].state.status, AppStatus::Running);
    assert_ne!(apps[0].state.pid, Some(old));
    assert_eq!(h.stop("act-reload").await.expect("stop by name works"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_cluster_children() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "act-cl-down", &m, 3))
        .await
        .expect("start cluster");
    assert_eq!(Harness::procs_with_cmdline(&format!("sleep {m}")).len(), 3);

    h.shutdown().await;

    assert!(
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty(),
        "every instance is dead"
    );
}

// ---- health: the starting and unhealthy states, recovery, action = restart -----------------

/// A script probe that passes while `flag` exists, probing every second from the start.
fn flag_probe(flag: &std::path::Path, extra: serde_json::Value) -> serde_json::Value {
    let mut hc = json!({
        "script": format!("test -f '{}'", flag.display()),
        "expected_status": [200],
        "interval_secs": 1,
        "timeout_secs": 1,
        "retries": 1,
        "start_period_secs": 0
    });
    if let (Some(base), Some(more)) = (hc.as_object_mut(), extra.as_object()) {
        base.extend(more.clone());
    }
    json!({ "health_check": hc })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_with_health_check_is_starting_until_first_healthy_probe() {
    let h = Harness::new().await;
    let m = h.marker();
    let flag = h.file("ok");
    // retries = 3: two failing probes before the flag appears are not yet a streak
    h.start(h.cmd_with(
        "act-health-start",
        &format!("exec sleep {m}"),
        flag_probe(&flag, json!({ "retries": 3 })),
    ))
    .await
    .expect("start");

    let state = h.state("act-health-start").await;
    assert_eq!(
        state.status,
        AppStatus::Starting,
        "alive but not proven healthy"
    );
    assert!(state.pid.is_some());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        h.state("act-health-start").await.status,
        AppStatus::Starting
    );

    std::fs::write(&flag, "").unwrap();
    let state = h.wait_status("act-health-start", AppStatus::Running).await;
    assert!(state.healthy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unhealthy_app_recovers_when_probe_passes_again() {
    let h = Harness::new().await;
    let m = h.marker();
    let flag = h.file("ok");
    h.start(h.cmd_with(
        "act-health-recover",
        &format!("exec sleep {m}"),
        flag_probe(&flag, json!({})),
    ))
    .await
    .expect("start");

    // no flag: the probe fails, one failure is a streak
    let state = h
        .wait_status("act-health-recover", AppStatus::Unhealthy)
        .await;
    let pid = state.pid.expect("an unhealthy app keeps its child");

    std::fs::write(&flag, "").unwrap();
    let state = h
        .wait_status("act-health-recover", AppStatus::Running)
        .await;
    assert_eq!(state.pid, Some(pid), "the child was never touched");

    // the probe is still running: taking the flag away makes the app unhealthy again
    std::fs::remove_file(&flag).unwrap();
    let state = h
        .wait_status("act-health-recover", AppStatus::Unhealthy)
        .await;
    assert_eq!(state.pid, Some(pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_action_restart_replaces_unhealthy_child() {
    let h = Harness::new().await;
    let m = h.marker();
    let flag = h.file("never");
    h.start(
        h.cmd_with("act-health-restart", &format!("exec sleep {m}"), {
            // the first probe comes a second after the launch, so the test can note the first pid
            let mut spec = flag_probe(
                &flag,
                json!({ "action": "restart", "start_period_secs": 1 }),
            );
            spec["restart_policy"] = json!({ "restart_delay_ms": 20, "max_restarts": 50 });
            spec
        }),
    )
    .await
    .expect("start");
    let first = h.pid_of("act-health-restart").await;

    h.wait_until_for(
        "act-health-restart",
        "a replacement child",
        Duration::from_secs(8),
        |s| s.pid.map(|p| p != first).unwrap_or(false),
    )
    .await;
    assert!(
        !Harness::pid_alive(first),
        "the unhealthy child was stopped"
    );
    h.stop("act-health-restart").await.expect("stop");
}
