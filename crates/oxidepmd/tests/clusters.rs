//! Clusters: the transactional create, the name pre-check, the start of a cluster
//! that is new and of one that exists, the one-transaction delete and the fixed instance count.

mod common;

use std::time::Duration;

use common::*;
use oxidepm_core::{AppSpec, AppStatus, Error};
use serde_json::json;

fn cluster(h: &Harness, name: &str, marker: &str, instances: u32) -> AppSpec {
    h.cmd_with(
        name,
        &format!("exec sleep {marker}"),
        json!({ "instances": instances }),
    )
}

fn sleepers(marker: &str) -> Vec<u32> {
    Harness::procs_with_cmdline(&format!("sleep {marker}"))
}

async fn wait_for_sleepers(h: &Harness, marker: &str, count: usize) {
    h.eventually(
        "the instances to exec sleep",
        Duration::from_secs(5),
        || sleepers(marker).len() >= count,
    )
    .await;
    assert_eq!(sleepers(marker).len(), count);
}

/// Make the launch of one app fail: its log file is a directory, so the log cannot be opened.
fn break_launch_of(h: &Harness, name: &str) {
    let path = h.paths.log_path(name, "out");
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(&path).expect("block the log file with a directory");
}

async fn names(h: &Harness) -> Vec<String> {
    let mut names: Vec<String> = h.status().await.into_iter().map(|a| a.spec.name).collect();
    names.sort();
    names
}

/// The names in the database, which can differ from the registry's after a half-done create.
async fn rows(h: &Harness) -> Vec<String> {
    use sqlx::Row;
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", h.paths.db().display()))
        .await
        .expect("open a read connection");
    let mut names: Vec<String> = sqlx::query("SELECT name FROM apps")
        .fetch_all(&pool)
        .await
        .expect("read the rows")
        .iter()
        .map(|r| r.get::<String, _>(0))
        .collect();
    pool.close().await;
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_start_name_collision_leaves_no_rows() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("a-1", &format!("exec sleep {m}")))
        .await
        .expect("start the app a-1");
    wait_for_sleepers(&h, &m, 1).await;

    let err = h
        .start(cluster(&h, "a", &m, 3))
        .await
        .expect_err("a-1 is taken");
    assert!(
        matches!(&err, Error::AppAlreadyExists(n) if n == "a-1"),
        "{err:?}"
    );

    assert_eq!(
        names(&h).await,
        ["a-1"],
        "nothing of the cluster is registered"
    );
    assert_eq!(
        rows(&h).await,
        ["a-1"],
        "nothing of the cluster is in the database"
    );
    assert_eq!(sleepers(&m).len(), 1, "no instance was launched");
    // the app that was there is the one app a-1, not an instance of anything
    assert_eq!(h.state("a-1").await.status, AppStatus::Running);
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_start_name_collision_with_a_stopped_row_leaves_no_rows() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("b-2", &format!("exec sleep {m}")))
        .await
        .expect("start b-2");
    h.stop("b-2").await.expect("stop b-2");

    let err = h
        .start(cluster(&h, "b", &m, 3))
        .await
        .expect_err("b-2 is taken");
    assert!(is_already_exists(&err), "{err:?}");
    assert_eq!(rows(&h).await, ["b-2"]);
    assert!(sleepers(&m).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_group_start_with_one_child_stopped_starts_only_that_child() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "web", &m, 3))
        .await
        .expect("start the cluster");
    let pid0 = h.pid_of("web-0").await;
    let pid2 = h.pid_of("web-2").await;

    h.stop("web-1").await.expect("stop one instance");
    h.wait_status("web-1", AppStatus::Stopped).await;

    h.start(cluster(&h, "web", &m, 3))
        .await
        .expect("start the group again");

    assert_eq!(h.state("web-1").await.status, AppStatus::Running);
    assert_eq!(
        h.pid_of("web-0").await,
        pid0,
        "a running sibling is left alone"
    );
    assert_eq!(
        h.pid_of("web-2").await,
        pid2,
        "a running sibling is left alone"
    );
    wait_for_sleepers(&h, &m, 3).await;
    assert_eq!(h.state("web").await.status, AppStatus::Running);

    // every instance is up now: starting the group says so
    let err = h
        .start(cluster(&h, "web", &m, 3))
        .await
        .expect_err("everything is running");
    assert!(is_already_exists(&err), "{err:?}");
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_start_failure_of_new_cluster_deletes_it() {
    let h = Harness::new().await;
    let m = h.marker();
    break_launch_of(&h, "fail-1");

    let err = h
        .start(cluster(&h, "fail", &m, 3))
        .await
        .expect_err("instance fail-1 cannot launch");
    assert!(
        !is_already_exists(&err),
        "the original error comes back: {err:?}"
    );

    h.eventually(
        "the instances that did start to be stopped",
        Duration::from_secs(5),
        || sleepers(&m).is_empty(),
    )
    .await;
    assert!(
        names(&h).await.is_empty(),
        "nothing is registered: {:?}",
        names(&h).await
    );
    assert!(rows(&h).await.is_empty(), "no rows: {:?}", rows(&h).await);

    // a retry is not blocked by the failed attempt
    std::fs::remove_dir_all(h.paths.log_path("fail-1", "out")).expect("unblock the log");
    h.start(cluster(&h, "fail", &m, 3))
        .await
        .expect("the retry works");
    wait_for_sleepers(&h, &m, 3).await;
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_start_failure_of_existing_cluster_leaves_siblings_running() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "keep", &m, 3))
        .await
        .expect("start the cluster");
    let pid0 = h.pid_of("keep-0").await;
    let pid2 = h.pid_of("keep-2").await;
    h.stop("keep-1").await.expect("stop one instance");
    h.wait_status("keep-1", AppStatus::Stopped).await;
    break_launch_of(&h, "keep-1");

    let err = h
        .start(cluster(&h, "keep", &m, 3))
        .await
        .expect_err("keep-1 cannot launch");
    assert!(
        err.to_string().contains("keep-1"),
        "the reply names the instance: {err}"
    );
    assert!(
        !err.to_string().contains("keep-0"),
        "and only the failed one: {err}"
    );

    // nothing is deleted or stopped
    assert_eq!(names(&h).await, ["keep", "keep-0", "keep-1", "keep-2"]);
    assert_eq!(rows(&h).await, ["keep", "keep-0", "keep-1", "keep-2"]);
    assert_eq!(h.pid_of("keep-0").await, pid0);
    assert_eq!(h.pid_of("keep-2").await, pid2);
    let failed = h.state("keep-1").await;
    assert_eq!(failed.status, AppStatus::Errored);
    assert!(
        failed.last_error.is_some(),
        "the reason stays on the instance: {failed:?}"
    );
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_delete_is_one_transaction_before_actors_exit() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "tx", &m, 3))
        .await
        .expect("start the cluster");
    let pids = [
        h.pid_of("tx-0").await,
        h.pid_of("tx-1").await,
        h.pid_of("tx-2").await,
    ];

    // a second connection makes every delete on the table fail
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", h.paths.db().display()))
        .await
        .expect("open a second connection");
    sqlx::query("CREATE TRIGGER inject_failure BEFORE DELETE ON apps BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool)
        .await
        .expect("create the trigger");

    let err = h.delete("tx").await.expect_err("the row cannot be removed");
    assert!(
        matches!(err, Error::DbError(_)) || err.to_string().contains("injected"),
        "{err:?}"
    );
    // nothing changed: the actors, the processes and the rows are all there
    assert_eq!(names(&h).await, ["tx", "tx-0", "tx-1", "tx-2"]);
    assert_eq!(rows(&h).await, ["tx", "tx-0", "tx-1", "tx-2"]);
    for (i, pid) in pids.iter().enumerate() {
        assert_eq!(
            h.pid_of(&format!("tx-{i}")).await,
            *pid,
            "instance {i} still runs"
        );
        assert!(Harness::pid_alive(*pid));
    }
    assert_eq!(h.state("tx").await.status, AppStatus::Running);

    sqlx::query("DROP TRIGGER inject_failure")
        .execute(&pool)
        .await
        .expect("drop the trigger");
    pool.close().await;
    assert_eq!(h.delete("tx").await.expect("delete works now"), 1);
    h.eventually("every instance to be gone", Duration::from_secs(3), || {
        sleepers(&m).is_empty()
    })
    .await;
    assert!(rows(&h).await.is_empty());
    assert!(names(&h).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_instance_count_change_is_rejected() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "fixed", &m, 2))
        .await
        .expect("start the cluster");
    h.stop("fixed").await.expect("stop the cluster");
    h.wait_status("fixed", AppStatus::Stopped).await;

    for instances in [3, 4] {
        let err = h
            .start(cluster(&h, "fixed", &m, instances))
            .await
            .expect_err("the count is fixed");
        assert!(matches!(err, Error::InvalidSpec(_)), "{err:?}");
        assert!(err.to_string().contains("delete and recreate"), "{err}");
    }
    assert_eq!(
        names(&h).await,
        ["fixed", "fixed-0", "fixed-1"],
        "nothing was added"
    );
    assert_eq!(
        h.state("fixed").await.status,
        AppStatus::Stopped,
        "and nothing was started"
    );
    assert!(sleepers(&m).is_empty());

    // the same count starts it again
    h.start(cluster(&h, "fixed", &m, 2))
        .await
        .expect("same count");
    wait_for_sleepers(&h, &m, 2).await;
    h.stop("all").await.expect("stop all");
}

/// A single app cannot be turned into a cluster by starting it with `instances > 1`, and a
/// cluster cannot be turned into a single app by starting it with one instance: both are refused
/// with the same kind of error as a change of a cluster's count, whether the app is at rest or
/// up, in a single start and in a batch, and nothing is created, started or stopped. Both apps
/// depend on an app that is stopped, and the answer is still `E_INVALID_SPEC` (not
/// `E_DEPENDENCY_FAILED`): the count is checked before the dependencies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_app_and_cluster_cannot_swap_their_instance_count() {
    let h = Harness::new().await;
    let m = h.marker();
    let dependent = |name: &str, instances: u32| {
        h.cmd_with(
            name,
            &format!("exec sleep {m}"),
            json!({ "instances": instances, "depends_on": ["swap-db"] }),
        )
    };
    h.start(h.cmd("swap-db", &format!("exec sleep {m}")))
        .await
        .expect("start the dependency");
    h.start(dependent("lone", 1))
        .await
        .expect("start the single app");
    h.start(dependent("pack", 2))
        .await
        .expect("start the cluster");
    h.stop("swap-db").await.expect("stop the dependency");
    h.wait_status("swap-db", AppStatus::Stopped).await;
    let before = names(&h).await;
    assert_eq!(before, ["lone", "pack", "pack-0", "pack-1", "swap-db"]);
    let pid = h.pid_of("lone").await;

    let refused = |err: &Error| {
        assert!(matches!(err, Error::InvalidSpec(_)), "{err:?}");
        assert!(err.to_string().contains("delete and recreate"), "{err}");
    };
    let refused_in_batch = |items: Vec<oxidepmd::supervisor::BatchItem>| {
        assert_eq!(items.len(), 2);
        for item in &items {
            refused(item.result.as_ref().expect_err("refused in a batch"));
        }
    };
    // both up
    refused(
        &h.start(dependent("lone", 3))
            .await
            .expect_err("a single app is not a cluster"),
    );
    refused(
        &h.start(dependent("pack", 1))
            .await
            .expect_err("a cluster is not a single app"),
    );
    refused_in_batch(
        h.sup
            .start_batch(vec![dependent("lone", 3), dependent("pack", 1)])
            .await
            .expect("batch"),
    );

    // both at rest
    h.stop("lone").await.expect("stop the single app");
    h.stop("pack").await.expect("stop the cluster");
    h.wait_status("lone", AppStatus::Stopped).await;
    h.wait_status("pack", AppStatus::Stopped).await;
    refused(
        &h.start(dependent("lone", 3))
            .await
            .expect_err("a single app is not a cluster"),
    );
    refused(
        &h.start(dependent("pack", 1))
            .await
            .expect_err("a cluster is not a single app"),
    );
    refused_in_batch(
        h.sup
            .start_batch(vec![dependent("lone", 3), dependent("pack", 1)])
            .await
            .expect("batch"),
    );
    assert_eq!(names(&h).await, before, "nothing was created or removed");
    assert_eq!(rows(&h).await, before);
    assert_eq!(
        h.state("lone").await.status,
        AppStatus::Stopped,
        "and nothing was started"
    );
    assert_eq!(h.state("pack").await.status, AppStatus::Stopped);
    assert!(sleepers(&m).is_empty());

    // the right counts start them again, once the dependency is back
    h.start(h.cmd("swap-db", &format!("exec sleep {m}")))
        .await
        .expect("restart the dependency");
    h.start(dependent("lone", 1)).await.expect("same count");
    h.start(dependent("pack", 2)).await.expect("same count");
    assert_ne!(h.pid_of("lone").await, pid, "a new process");
    wait_for_sleepers(&h, &m, 4).await;
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_instances_are_single_instance_apps() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(cluster(&h, "solo", &m, 2))
        .await
        .expect("start the cluster");
    for name in ["solo-0", "solo-1"] {
        let info = h
            .status()
            .await
            .into_iter()
            .find(|a| a.spec.name == name)
            .expect("instance");
        assert_eq!(
            info.spec.instances, 1,
            "{name} can never be mistaken for a parent"
        );
    }
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tag_that_matches_a_cluster_and_its_instances_acts_once() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "tagged",
        &format!("exec sleep {m}"),
        json!({ "instances": 2, "tags": ["grp"] }),
    ))
    .await
    .expect("start the cluster");
    let before = [h.pid_of("tagged-0").await, h.pid_of("tagged-1").await];

    // one restart of the group, not one more for each instance
    assert_eq!(h.restart("@grp").await.expect("restart by tag"), 1);
    let after = [h.pid_of("tagged-0").await, h.pid_of("tagged-1").await];
    assert_ne!(before[0], after[0]);
    assert_ne!(before[1], after[1]);
    assert_eq!(
        h.state("tagged-0").await.restarts,
        1,
        "restarted once, not once per match"
    );
    assert_eq!(
        h.state("tagged-1").await.restarts,
        1,
        "restarted once, not once per match"
    );
    assert_eq!(
        h.delete("@grp").await.expect("delete by tag"),
        1,
        "the cluster is deleted once"
    );
    assert!(names(&h).await.is_empty());
}
