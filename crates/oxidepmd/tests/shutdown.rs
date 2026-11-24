//! The supervisor's shutdown, driven through the facade: the gate that refuses new work, the
//! order in which apps are stopped, and what an in-flight start or a pending crash restart do.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use oxidepm_core::AppStatus;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_restart_and_resurrect_are_refused_once_shut_down() {
    let h = Harness::new().await;
    let m = h.marker();
    let id = h
        .start(h.cmd("gate-a", &format!("exec sleep {m}")))
        .await
        .expect("start");
    assert_eq!(h.save(false).await.expect("save"), 1);

    h.shutdown().await;

    let err = h
        .start(h.cmd("gate-b", &format!("exec sleep {m}")))
        .await
        .expect_err("start");
    assert!(is_shutting_down(&err), "{err}");
    let err = h
        .sup
        .start(h.cmd("gate-a", &format!("exec sleep {m}")))
        .await
        .expect_err("start stopped app");
    assert!(is_shutting_down(&err), "{err}");
    let err = h.sup.restart(id).await.expect_err("restart");
    assert!(is_shutting_down(&err), "{err}");
    let err = h.sup.reload(id).await.expect_err("reload");
    assert!(is_shutting_down(&err), "{err}");
    let err = h.resurrect().await.expect_err("resurrect");
    assert!(is_shutting_down(&err), "{err}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty(),
        "nothing was started"
    );
    // reads still work
    assert_eq!(h.status().await.len(), 1);
}

/// An app that depends on another is stopped before it, so it never runs without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_dependents_before_their_dependencies() {
    let h = Harness::new().await;
    let order = h.file("stop-order");
    let app = |name: &str, depends: &[&str]| {
        // on SIGTERM: note the name, then exit; the loop keeps the shell responsive to it
        let ready = h.file(&format!("{name}.ready"));
        let sh = format!(
            "trap 'echo {name} >> {}; exit 0' TERM; touch {}; while :; do sleep 0.05; done",
            order.display(),
            ready.display()
        );
        h.cmd_with(name, &sh, json!({ "depends_on": depends }))
    };
    for spec in [
        app("order-db", &[]),
        app("order-web", &["order-db"]),
        app("order-api", &["order-web"]),
    ] {
        let name = spec.name.clone();
        h.start(spec).await.expect("start");
        h.wait_file(&h.file(&format!("{name}.ready"))).await;
    }

    h.shutdown().await;

    let stopped: Vec<String> = std::fs::read_to_string(&order)
        .expect("stop order file")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(stopped, vec!["order-api", "order-web", "order-db"]);
}

/// A start that is still waiting out its `startup_delay_ms` is cancelled at once: the shutdown
/// does not have to wait for it and nothing is started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_a_start_that_is_waiting_out_its_delay() {
    let h = Harness::new().await;
    let m = h.marker();
    let spec = h.cmd_with(
        "slow-start",
        &format!("exec sleep {m}"),
        json!({ "startup_delay_ms": 20000 }),
    );
    let sup = Arc::clone(&h.sup);
    let start = tokio::spawn(async move { sup.start(spec).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let begun = Instant::now();
    h.shutdown().await;
    let result = tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("the start returns promptly")
        .expect("task");
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "took {:?}",
        begun.elapsed()
    );
    assert!(
        is_shutting_down(&result.expect_err("cancelled")),
        "refused as shutting down"
    );
    assert!(Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty());
}

/// A crash whose restart is waiting out `restart_delay_ms` does not come back to life once the
/// daemon is shutting down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_restart_pending_at_shutdown_starts_nothing() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with(
        "pending-restart",
        &format!("exec sleep {m}"),
        json!({ "restart_policy": { "restart_delay_ms": 2500, "max_restarts": 5 } }),
    ))
    .await
    .expect("start");
    let pid = h.pid_of("pending-restart").await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the child");
    h.wait_until("pending-restart", "the crash to be seen", |s| {
        s.status == AppStatus::Backoff || (s.status == AppStatus::Errored && s.pid.is_none())
    })
    .await;

    h.shutdown().await;

    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(
        Harness::procs_with_cmdline(&needle).is_empty(),
        "the pending restart started a process after the shutdown"
    );
}

/// The window between the spawn of a start's child and its insertion into the process map: the
/// gate closes there, so the child must be killed again and the start refused. A hook in the
/// supervisor holds the start exactly at that point (the other race tests only hit it by luck).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_that_spawned_as_the_gate_closed_is_killed_again() {
    let h = Harness::new().await;
    let m = h.marker();
    let pause = h.sup.pause_starts_after_spawn();

    let start = {
        let sup = Arc::clone(&h.sup);
        let spec = h.cmd("spawn-then-close", &format!("exec sleep {m}"));
        tokio::spawn(async move { sup.start(spec).await })
    };
    // the child exists, the start has not registered it yet
    tokio::time::timeout(Duration::from_secs(10), pause.reached.notified())
        .await
        .expect("the start reached the pause");
    assert_eq!(
        Harness::procs_with_cmdline(&format!("sleep {m}")).len(),
        1,
        "the child was spawned"
    );

    let report = h.sup.shutdown(Duration::from_secs(10)).await;
    assert!(
        report.running.is_empty(),
        "nothing was registered when the gate closed"
    );
    pause.release.notify_one();

    let err = start
        .await
        .expect("join")
        .expect_err("the start is refused");
    assert!(is_shutting_down(&err), "{err}");
    h.eventually(
        "the spawned child is killed again",
        Duration::from_secs(5),
        || Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty(),
    )
    .await;
    assert!(h
        .status()
        .await
        .iter()
        .all(|a| a.spec.name != "spawn-then-close" || !a.state.status.has_child()));
}
