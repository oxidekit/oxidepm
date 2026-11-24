//! Process groups: an app runs as the leader of its own group, and a stop reaches everything
//! in that group, not only the direct child.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

fn test_pgid() -> u32 {
    nix::unistd::getpgrp().as_raw() as u32
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_runs_in_its_own_process_group() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("pg-own", &format!("exec sleep {m}")))
        .await
        .expect("start");

    let pid = h.pid_of("pg-own").await;
    assert_eq!(
        Harness::pgid_of(pid),
        Some(pid),
        "the child leads its own group"
    );
    assert_ne!(
        Harness::pgid_of(pid),
        Some(test_pgid()),
        "not the group of the daemon"
    );
}

/// `sh -c 'sleep M & wait'`: the sleep is a grandchild of the supervisor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_grandchildren() {
    let h = Harness::new().await;
    let m = h.marker();
    let ready = h.file("gc.ready");
    h.start(h.cmd(
        "pg-grandchild",
        &format!("sleep {m} & touch '{}'; wait", ready.display()),
    ))
    .await
    .expect("start");
    h.wait_file(&ready).await;
    assert!(!Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty());

    assert_eq!(h.stop("pg-grandchild").await.expect("stop"), 1);
    h.eventually("the grandchild is gone", Duration::from_secs(3), || {
        Harness::procs_with_cmdline(&m).is_empty()
    })
    .await;
}

/// The whole group ignores SIGTERM: the group is killed when `kill_timeout_ms` is up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_escalates_to_sigkill_for_whole_group() {
    let h = Harness::new().await;
    let m = h.marker();
    let ready = h.file("stubborn.ready");
    let spec = h.cmd_with(
        "pg-stubborn",
        &format!(
            "trap '' TERM; touch '{}'; sleep {m}; exit 0",
            ready.display()
        ),
        json!({ "kill_timeout_ms": 500 }),
    );
    h.start(spec).await.expect("start");
    h.wait_file(&ready).await;
    h.eventually("the sleep runs", Duration::from_secs(3), || {
        !Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    })
    .await;

    let stopping = Instant::now();
    assert_eq!(h.stop("pg-stubborn").await.expect("stop"), 1);
    assert!(
        stopping.elapsed() < Duration::from_secs(2),
        "stop took {:?}",
        stopping.elapsed()
    );
    h.eventually(
        "nothing of the group is left",
        Duration::from_secs(2),
        || Harness::procs_with_cmdline(&m).is_empty(),
    )
    .await;
}

/// A stop that finds the leader gone on SIGTERM but a member that ignores it still kills the
/// member (the members of the group are not left behind, whoever exits first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_members_that_outlive_the_leader() {
    let h = Harness::new().await;
    let m = h.marker();
    let ready = h.file("member.ready");
    let spec = h.cmd_with(
        "pg-member",
        &format!(
            "(trap '' TERM; exec sleep {m}) & touch '{}'; trap 'exit 0' TERM; wait",
            ready.display()
        ),
        json!({ "kill_timeout_ms": 500 }),
    );
    h.start(spec).await.expect("start");
    h.wait_file(&ready).await;
    h.eventually("the member runs", Duration::from_secs(3), || {
        !Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    })
    .await;

    assert_eq!(h.stop("pg-member").await.expect("stop"), 1);
    h.eventually("the member is gone", Duration::from_secs(2), || {
        Harness::procs_with_cmdline(&m).is_empty()
    })
    .await;
}
