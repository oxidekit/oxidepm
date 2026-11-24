//! Memory and uptime limits: the sampler measures, the actor restarts.

mod common;

use std::time::Duration;

use common::*;
use oxidepm_core::AppStatus;
use serde_json::json;

/// An uptime limit replaces the child with a new one and the app is running again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_uptime_restarts_with_new_pid() {
    let h = Harness::new().await;
    let m = h.marker();
    let spec = h.cmd_with(
        "lim-up",
        &format!("exec sleep {m}"),
        json!({ "max_uptime_secs": 2 }),
    );
    h.start(spec).await.expect("start");
    let first = h.pid_of("lim-up").await;

    let state = h
        .wait_until_for(
            "lim-up",
            "a new pid after the uptime limit",
            Duration::from_secs(8),
            |s| s.status == AppStatus::Running && s.pid.is_some() && s.pid != Some(first),
        )
        .await;

    assert!(state.restarts >= 1, "{state:?}");
    assert!(!Harness::pid_alive(first), "the first child is gone");
    h.stop("lim-up").await.expect("stop");
}

/// A memory limit replaces a child that grew past it (skips without perl).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_limit_restarts_process() {
    if std::process::Command::new("perl")
        .arg("-v")
        .output()
        .is_err()
    {
        skip("perl is not installed");
        return;
    }
    let h = Harness::new().await;
    let spec = h.cmd_with(
        "lim-mem",
        "exec perl -e '$x=\"a\" x 60_000_000; sleep 300'",
        json!({ "max_memory_mb": 20 }),
    );
    h.start(spec).await.expect("start");
    let first = h.pid_of("lim-mem").await;

    let state = h
        .wait_until_for(
            "lim-mem",
            "a restart over the memory limit",
            Duration::from_secs(15),
            |s| s.restarts >= 1 && s.pid.is_some() && s.pid != Some(first),
        )
        .await;

    assert!(state.restarts >= 1, "{state:?}");
    assert!(!Harness::pid_alive(first), "the first child is gone");
    h.stop("lim-mem").await.expect("stop");
}

/// A periodic restart the operator asked for (a limit that fires after a run of at least five
/// seconds) feeds the delay and never the crash-loop history, so `max_restarts` is not spent.
/// A limit of `max_uptime_secs = 2` is below the five-second floor of
/// `MIN_RUN_FLOOR`, where the tracker deliberately counts the restart (see the next test).
#[cfg(feature = "slow-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_restarts_do_not_end_in_crash_loop() {
    let h = Harness::new().await;
    let m = h.marker();
    let spec = h.cmd_with(
        "lim-loop",
        &format!("exec sleep {m}"),
        json!({
            "max_uptime_secs": 6,
            "restart_policy": { "restart_delay_ms": 100, "max_restarts": 1 }
        }),
    );
    h.start(spec).await.expect("start");

    // three launches, two limit restarts: more than max_restarts = 1 would have tripped
    let state = h
        .wait_until_for(
            "lim-loop",
            "two limit restarts",
            Duration::from_secs(30),
            |s| s.restarts >= 2,
        )
        .await;
    assert_ne!(state.status, AppStatus::Errored, "{state:?}");
    let state = h
        .wait_until_for("lim-loop", "running again", Duration::from_secs(5), |s| {
            s.status == AppStatus::Running
        })
        .await;
    assert_eq!(state.last_error, None);
    h.stop("lim-loop").await.expect("stop");
}

/// A limit that fires within five seconds of every start is a misconfiguration: it counts as a
/// crash and the breaker ends the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_restart_inside_the_floor_trips_the_breaker() {
    let h = Harness::new().await;
    let m = h.marker();
    let spec = h.cmd_with(
        "lim-floor",
        &format!("exec sleep {m}"),
        json!({
            "max_uptime_secs": 1,
            "restart_policy": { "restart_delay_ms": 20, "max_restarts": 1 }
        }),
    );
    h.start(spec).await.expect("start");

    let state = h
        .wait_until_for(
            "lim-floor",
            "the crash loop breaker",
            Duration::from_secs(10),
            |s| s.status == AppStatus::Errored,
        )
        .await;

    assert_eq!(
        state.last_error.as_deref(),
        Some("crash loop: 2 exits in 600 s")
    );
    assert_eq!(state.pid, None);
    assert_eq!(state.restarts, 1);
    assert!(
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty(),
        "no child is left"
    );
}
