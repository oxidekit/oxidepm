//! Characterisation of start / stop / restart / delete against real processes.
//! Everything here passes on the legacy supervisor and must keep passing through the rebuild.

mod common;

use std::time::Duration;

use common::*;
use oxidepm_core::AppStatus;
use serde_json::json;

fn fast_restart() -> serde_json::Value {
    json!({ "restart_policy": { "restart_delay_ms": 20, "max_restarts": 5 } })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_sleep_app_reaches_running_with_live_pid() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("life-start", &format!("exec sleep {m}")))
        .await
        .expect("start");

    let state = h.state("life-start").await;
    assert_eq!(state.status, AppStatus::Running);
    let pid = state.pid.expect("running app has a pid");
    assert!(Harness::pid_alive(pid), "child {pid} should be alive");
    assert!(h.paths.log_path("life-start", "out").exists());
    assert!(h.paths.log_path("life-start", "err").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_terminates_child_and_reports_stopped() {
    let h = Harness::new().await;
    // Traps SIGTERM and exits 7, so the recorded exit code is observable. The ready file tells
    // the test the trap is installed; a SIGTERM before that would kill the shell outright.
    let ready = h.file("trap.ready");
    let sh = format!(
        "trap 'exit 7' TERM; touch '{}'; while :; do sleep 0.05; done",
        ready.display()
    );
    h.start(h.cmd("life-stop", &sh)).await.expect("start");
    h.wait_file(&ready).await;
    let pid = h.pid_of("life-stop").await;

    assert_eq!(h.stop("life-stop").await.expect("stop"), 1);

    h.eventually("child to die", Duration::from_secs(3), || {
        !Harness::pid_alive(pid)
    })
    .await;
    let state = h.state("life-stop").await;
    assert_eq!(state.status, AppStatus::Stopped);
    assert_eq!(state.pid, None);
    assert_eq!(state.last_exit_code, Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_is_idempotent_on_stopped_app() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("life-idem", &format!("exec sleep {m}")))
        .await
        .expect("start");

    assert_eq!(h.stop("life-idem").await.expect("first stop"), 1);
    assert_eq!(h.stop("life-idem").await.expect("second stop"), 0);
    assert_eq!(h.state("life-idem").await.status, AppStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replaces_pid_and_leaves_exactly_one_process() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd("life-restart", &format!("exec sleep {m}")))
        .await
        .expect("start");
    let old = h.pid_of("life-restart").await;

    assert_eq!(h.restart("life-restart").await.expect("restart"), 1);

    let state = h.state("life-restart").await;
    let new = state.pid.expect("restarted app has a pid");
    assert_ne!(new, old);
    assert_eq!(state.status, AppStatus::Running);
    h.eventually("old child to die", Duration::from_secs(3), || {
        !Harness::pid_alive(old)
    })
    .await;
    assert_eq!(Harness::procs_with_cmdline(&needle), vec![new]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_kills_child_and_removes_app() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("life-delete", &format!("exec sleep {m}")))
        .await
        .expect("start");
    let pid = h.pid_of("life-delete").await;

    assert_eq!(h.delete("life-delete").await.expect("delete"), 1);

    h.eventually("child to die", Duration::from_secs(3), || {
        !Harness::pid_alive(pid)
    })
    .await;
    assert!(h.status().await.is_empty());
    let err = h.stop("life-delete").await.expect_err("app is gone");
    assert!(is_not_found(&err), "unexpected error: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_while_running_is_rejected_with_app_exists() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd("life-dup", &format!("exec sleep {m}")))
        .await
        .expect("first start");

    let err = h
        .start(h.cmd("life-dup", &format!("exec sleep {m}")))
        .await
        .expect_err("second start must be rejected");
    assert!(is_already_exists(&err), "unexpected error: {err}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(Harness::procs_with_cmdline(&needle).len(), 1);
    assert_eq!(h.status().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_unknown_app_is_app_not_found() {
    let h = Harness::new().await;
    let err = h.stop("no-such-app").await.expect_err("unknown app");
    assert!(is_not_found(&err), "unexpected error: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkilled_child_is_restarted() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("life-kill", &format!("exec sleep {m}"), fast_restart()))
        .await
        .expect("start");
    let old = h.pid_of("life-kill").await;

    kill(Pid::from_raw(old as i32), Signal::SIGKILL).expect("kill the child we started");

    let state = h
        .wait_until_for(
            "life-kill",
            "a new live pid and running",
            Duration::from_secs(5),
            |s| {
                s.status == AppStatus::Running
                    && s.pid
                        .map(|p| p != old && Harness::pid_alive(p))
                        .unwrap_or(false)
            },
        )
        .await;
    assert_eq!(
        Harness::procs_with_cmdline(&needle),
        vec![state.pid.unwrap()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_zero_under_restart_always_restarts() {
    let h = Harness::new().await;
    let launches = h.file("launches");
    let sh = format!("echo launch >> '{}'; exit 0", launches.display());
    h.start(h.cmd_with(
        "life-always",
        &sh,
        json!({
            "restart_mode": "always",
            "restart_policy": { "restart_delay_ms": 20, "max_restarts": 2 }
        }),
    ))
    .await
    .expect("start");

    h.eventually("a second launch", WAIT_CAP, || {
        Harness::line_count(&launches) >= 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_child_keeps_running_after_manual_restart_storm() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd("life-storm", &format!("exec sleep {m}")))
        .await
        .expect("start");

    for _ in 0..3 {
        assert_eq!(h.restart("life-storm").await.expect("restart"), 1);
    }

    // A late duplicate (from a older supervision task) would show up within a poll or two.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let state = h.state("life-storm").await;
    assert_eq!(state.status, AppStatus::Running);
    assert_eq!(
        Harness::procs_with_cmdline(&needle),
        vec![state.pid.expect("pid")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_exit_under_on_crash_is_not_restarted() {
    let h = Harness::new().await;
    let launches = h.file("launches");
    let sh = format!("echo launch >> '{}'; exit 0", launches.display());
    h.start(h.cmd_with(
        "life-clean",
        &sh,
        json!({ "restart_policy": { "restart_delay_ms": 20 } }),
    ))
    .await
    .expect("start");

    h.wait_until("life-clean", "the exit to be observed", |s| {
        s.last_exit_code.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        Harness::line_count(&launches),
        1,
        "exactly one launch, no relaunch"
    );
    let state = h.state("life-clean").await;
    assert_eq!(state.last_exit_code, Some(0));
    assert_eq!(state.pid, None);
    // HEAD reports a clean exit that is not restarted as `errored`. This is pinned on purpose:
    // The restart default is part of the documented behaviour.
    assert_eq!(state.status, AppStatus::Errored);
    // the message names the key a user can set (`restart`), not the internal field name
    let error = state.last_error.expect("the reason it was not restarted");
    assert!(
        error.contains("not restarted (restart=on-crash)"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_live_child_of_unhealthy_app() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with(
        "life-unhealthy-stop",
        &format!("exec sleep {m}"),
        failing_probe(),
    ))
    .await
    .expect("start");
    // The legacy checker waits 5 s before its first probe.
    let state = h
        .wait_until_for(
            "life-unhealthy-stop",
            "unhealthy",
            Duration::from_secs(20),
            |s| h.is_unhealthy(s),
        )
        .await;
    let pid = state.pid.expect("unhealthy app keeps its pid");
    assert!(Harness::pid_alive(pid));

    assert_eq!(h.stop("life-unhealthy-stop").await.expect("stop"), 1);

    h.eventually("child to die", Duration::from_secs(3), || {
        !Harness::pid_alive(pid)
    })
    .await;
    assert!(Harness::procs_with_cmdline(&needle).is_empty());
    let state = h.state("life-unhealthy-stop").await;
    assert_eq!(state.status, AppStatus::Stopped);
    assert_eq!(state.pid, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_of_unhealthy_app_leaves_one_process() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with(
        "life-unhealthy-restart",
        &format!("exec sleep {m}"),
        failing_probe(),
    ))
    .await
    .expect("start");
    let old = h
        .wait_until_for(
            "life-unhealthy-restart",
            "unhealthy",
            Duration::from_secs(20),
            |s| h.is_unhealthy(s),
        )
        .await
        .pid
        .expect("unhealthy app keeps its pid");

    assert_eq!(
        h.restart("life-unhealthy-restart").await.expect("restart"),
        1
    );

    h.eventually("old child to die", Duration::from_secs(3), || {
        !Harness::pid_alive(old)
    })
    .await;
    // A duplicate would show up within a poll or two.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let procs = Harness::procs_with_cmdline(&needle);
    assert_eq!(
        procs.len(),
        1,
        "exactly one process after the restart: {procs:?}"
    );
    assert_ne!(procs[0], old);
}

/// Without the per-app operation lock, two restarts both pass the "not active" check and each
/// spawn a child; the one the registry forgets is orphaned. 16 at once make that certain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_restarts_of_one_app_leave_one_process() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    let id = h
        .start(h.cmd("life-race", &format!("exec sleep {m}")))
        .await
        .expect("start");

    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let sup = std::sync::Arc::clone(&h.sup);
            tokio::spawn(async move { sup.restart(id).await })
        })
        .collect();
    for task in tasks {
        task.await.expect("task").expect("restart");
    }

    // A leaked child would still be there once the dust has settled.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let procs = Harness::procs_with_cmdline(&needle);
    assert_eq!(
        procs.len(),
        1,
        "exactly one process after 16 concurrent restarts: {procs:?}"
    );
    assert_eq!(
        h.pid_of("life-race").await,
        procs[0],
        "the registry tracks the survivor"
    );
    assert_eq!(h.status().await.len(), 1, "one registry entry");
    assert_eq!(h.state("life-race").await.status, AppStatus::Running);
}

/// The same race with `start`, `stop` and `restart` mixed: whichever order they land in, there
/// is never more than one process, and one registry entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_stop_restart_never_leave_two_processes() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    let id = h
        .start(h.cmd("life-mix", &format!("exec sleep {m}")))
        .await
        .expect("start");

    let tasks: Vec<_> = (0..18)
        .map(|n| {
            let sup = std::sync::Arc::clone(&h.sup);
            let spec = h.cmd("life-mix", &format!("exec sleep {m}"));
            tokio::spawn(async move {
                // A start or restart that a stop overtakes while it is still launching is
                // answered E_CANCELLED (the stop wins); already running is the other refusal.
                let allowed = |r: oxidepm_core::Result<()>| match r {
                    Err(oxidepm_core::Error::AppAlreadyExists(_)) => Ok(()),
                    Err(oxidepm_core::Error::Cancelled(_)) => Ok(()),
                    other => other,
                };
                match n % 3 {
                    0 => allowed(sup.restart(id).await.map(|_| ())),
                    1 => allowed(sup.stop(id).await.map(|_| ())),
                    _ => allowed(sup.start(spec).await.map(|_| ())),
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.expect("task").expect("operation");
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let procs = Harness::procs_with_cmdline(&needle);
    assert!(procs.len() <= 1, "never two processes: {procs:?}");
    assert_eq!(h.status().await.len(), 1, "one registry entry");

    // Bring it back up if the last operation was a stop: still exactly one process.
    if procs.is_empty() {
        h.start(h.cmd("life-mix", &format!("exec sleep {m}")))
            .await
            .expect("start again");
    }
    assert_eq!(Harness::procs_with_cmdline(&needle).len(), 1);
}
