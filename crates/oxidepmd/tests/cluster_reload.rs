//! Characterisation of cluster start.

mod common;

use common::*;
use oxidepm_core::AppStatus;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_start_creates_parent_and_children_with_unique_names() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("clu", &format!("exec sleep {m}"), json!({ "instances": 3 })))
        .await
        .expect("start cluster");

    let mut names: Vec<String> = h.status().await.into_iter().map(|a| a.spec.name).collect();
    names.sort();
    assert_eq!(names, ["clu", "clu-0", "clu-1", "clu-2"]);

    for child in ["clu-0", "clu-1", "clu-2"] {
        let state = h.state(child).await;
        assert_eq!(state.status, AppStatus::Running, "{child}");
        assert!(Harness::pid_alive(state.pid.expect("child pid")), "{child}");
    }
    assert_eq!(
        h.state("clu").await.pid,
        None,
        "the parent has no process of its own"
    );
    assert_eq!(Harness::procs_with_cmdline(&needle).len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_children_get_sequential_ports() {
    let h = Harness::new().await;
    let m = h.marker();
    let sh = format!("echo PORT=$PORT; exec sleep {m}");
    h.start(h.cmd_with("clu-port", &sh, json!({ "instances": 3, "port": 4100 })))
        .await
        .expect("start cluster");

    for (i, port) in [4100, 4101, 4102].into_iter().enumerate() {
        let child = format!("clu-port-{i}");
        h.wait_log(&child, "out", &format!("PORT={port}")).await;
    }
}
