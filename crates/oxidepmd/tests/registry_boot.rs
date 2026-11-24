//! Registry at boot: every app of the database is registered when the daemon
//! starts, `save` and `resurrect` run on that state, and a stop is never overridden.

mod common;

use common::*;
use oxidepm_core::{AppStatus, Error, ErrorCode};
use serde_json::json;
use std::time::Duration;

fn saved_names(h: &Harness) -> Vec<String> {
    let text = std::fs::read_to_string(h.paths.saved()).expect("saved.json");
    saved_entries(&text)
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_string())
        .collect()
}

/// The registry, not `saved.json`, says what comes back: the file is removed before the
/// resurrect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_daemon_resurrects_running_apps() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("boot-a", &format!("exec sleep {m}")))
        .await
        .expect("start");
    let old = h.pid_of("boot-a").await;

    h.restart_daemon().await;
    assert!(
        !Harness::pid_alive(old),
        "children do not survive the daemon"
    );
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    // registered, at rest, before anything is resurrected
    assert_eq!(h.state("boot-a").await.status, AppStatus::Stopped);
    assert_eq!(h.resurrect().await.expect("resurrect"), 1);
    let new = h.pid_of("boot-a").await;
    assert_ne!(old, new, "a new process");
    assert_eq!(
        Harness::procs_with_cmdline(&format!("sleep {m}")),
        vec![new]
    );

    assert_eq!(
        h.resurrect().await.expect("resurrect again"),
        0,
        "nothing left to start"
    );
    assert_eq!(
        Harness::procs_with_cmdline(&format!("sleep {m}")),
        vec![new],
        "no duplicate"
    );
    h.stop("boot-a").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicitly_stopped_app_is_not_resurrected() {
    let mut h = Harness::new().await;
    let (ma, mb) = (h.marker(), h.marker());
    h.start(h.cmd("keep-a", &format!("exec sleep {ma}")))
        .await
        .expect("start a");
    h.start(h.cmd("keep-b", &format!("exec sleep {mb}")))
        .await
        .expect("start b");
    h.stop("keep-b").await.expect("stop b");

    h.restart_daemon().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    assert_eq!(
        h.resurrect().await.expect("resurrect"),
        1,
        "only the app that was meant to run"
    );
    h.pid_of("keep-a").await;
    assert_eq!(h.state("keep-b").await.status, AppStatus::Stopped);
    assert!(Harness::procs_with_cmdline(&format!("sleep {mb}")).is_empty());
    h.stop("keep-a").await.expect("stop");
}

/// A resurrect starts the apps together, and an app that waits for a slow dependency is
/// accepted as `waiting`. A stop that lands while the boot resurrect is still in flight is
/// remembered: the waiting app stays stopped when the dependency comes up, and so does its
/// stored intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_slow_boot_resurrect_is_not_overridden() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "slow-first",
        "exec sleep 305.9",
        json!({ "startup_delay_ms": 1500 }),
    ))
    .await
    .expect("start slow");
    h.start(h.cmd_with(
        "late-x",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["slow-first"] }),
    ))
    .await
    .expect("start x");
    h.restart_daemon().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    let sup = h.sup.clone();
    let resurrect = tokio::spawn(async move { sup.resurrect().await });
    h.wait_status("slow-first", AppStatus::Waiting).await;
    h.wait_status("late-x", AppStatus::Waiting).await;

    h.stop("late-x")
        .await
        .expect("stop x while the boot resurrect is in flight");
    assert_eq!(
        resurrect.await.unwrap().expect("resurrect"),
        2,
        "both were started by it"
    );

    h.wait_status("slow-first", AppStatus::Running).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(h.state("late-x").await.status, AppStatus::Stopped);
    assert!(
        Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty(),
        "x never launched"
    );
    h.save(true).await.expect("save");
    assert_eq!(
        saved_names(&h),
        vec!["slow-first"],
        "desired=stopped was kept"
    );
    h.stop("slow-first").await.expect("stop");
}

/// The saved apps are started together: a slow one does not hold up the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_does_not_start_apps_one_after_another() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with(
        "slow-one",
        "exec sleep 305.7",
        json!({ "startup_delay_ms": 3000 }),
    ))
    .await
    .expect("start slow");
    h.start(h.cmd("quick-one", &format!("exec sleep {m}")))
        .await
        .expect("start quick");
    h.restart_daemon().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    let sup = h.sup.clone();
    let resurrect = tokio::spawn(async move { sup.resurrect().await });
    h.wait_status("quick-one", AppStatus::Running).await;
    assert_eq!(
        h.state("slow-one").await.status,
        AppStatus::Waiting,
        "still in its delay"
    );
    assert!(!resurrect.is_finished());
    assert_eq!(resurrect.await.unwrap().expect("resurrect"), 2);
    h.stop("all").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_preserves_full_spec() {
    let mut h = Harness::new().await;
    let web = h.cmd_with(
        "full-spec",
        "exec sleep 305.8",
        json!({
            "tags": ["frontend", "prod"],
            "env": { "KEEP": "yes" },
            "hooks": { "on_start": "true", "on_crash": "echo crashed" },
            "health_check": {
                "script": "true", "expected_status": [200],
                "interval_secs": 30, "timeout_secs": 5, "retries": 3
            },
            "kill_timeout_ms": 4321,
            "restart_cron": "0 4 * * *",
            "log_max_size": 1000000,
            "log_compress": true,
            "max_memory_mb": 512
        }),
    );
    let mut before = serde_json::to_value(&web).unwrap();
    h.start(web).await.expect("start");
    h.pid_of("full-spec").await;

    h.restart_daemon().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");
    assert_eq!(h.resurrect().await.expect("resurrect"), 1);
    h.pid_of("full-spec").await;

    let mut after = serde_json::to_value(
        h.sup
            .show(&oxidepm_core::Selector::ByName("full-spec".into()))
            .await
            .unwrap()
            .expect("shown")
            .spec,
    )
    .unwrap();
    // the ids and the creation time are the daemon's, not the client's
    for value in [&mut before, &mut after] {
        value["id"] = json!(0);
        value["created_at"] = json!("");
    }
    assert_eq!(after, before);
    h.stop("full-spec").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_refuses_to_blank_nonempty_file() {
    let h = Harness::new().await;
    // a saved file that lists an app, a registry with nothing that should run
    let listed = saved_json(&[h.cmd("listed", "exec sleep 305.7")]);
    std::fs::create_dir_all(h.paths.saved().parent().unwrap()).unwrap();
    std::fs::write(h.paths.saved(), &listed).unwrap();

    let err = h
        .save(false)
        .await
        .expect_err("an empty save must be refused");
    assert!(matches!(err, Error::SaveWouldEmpty(_)), "{err:?}");
    assert_eq!(err.code(), ErrorCode::SaveWouldEmpty);
    assert_eq!(
        std::fs::read_to_string(h.paths.saved()).unwrap(),
        listed,
        "file unchanged"
    );

    assert_eq!(h.save(true).await.expect("forced"), 0);
    assert_eq!(saved_names(&h), Vec::<String>::new());
    // an empty list is not worth protecting
    assert_eq!(h.save(false).await.expect("empty over empty"), 0);
}

/// `save` lists what the database says should run, not what is in memory: an app stopped by
/// the operator is left out, and a cluster is written once (never as parent plus instances).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_lists_running_top_level_apps_from_the_database() {
    let h = Harness::new().await;
    let (m1, m2, m3) = (h.marker(), h.marker(), h.marker());
    h.start(h.cmd("sv-up", &format!("exec sleep {m1}")))
        .await
        .expect("up");
    h.start(h.cmd("sv-down", &format!("exec sleep {m2}")))
        .await
        .expect("down");
    h.start(h.cmd_with(
        "sv-clu",
        &format!("exec sleep {m3}"),
        json!({ "instances": 2 }),
    ))
    .await
    .expect("cluster");
    h.stop("sv-down").await.expect("stop");

    assert_eq!(h.save(false).await.expect("save"), 2);
    assert_eq!(saved_names(&h), vec!["sv-up", "sv-clu"]);
    h.stop("all").await.expect("stop all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_does_not_start_cluster_children_twice() {
    let mut h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with(
        "clu-boot",
        &format!("exec sleep {m}"),
        json!({ "instances": 3 }),
    ))
    .await
    .expect("start cluster");
    assert_eq!(Harness::procs_with_cmdline(&needle).len(), 3);

    h.restart_daemon().await;
    assert!(Harness::procs_with_cmdline(&needle).is_empty());
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    assert_eq!(h.resurrect().await.expect("resurrect"), 1, "one cluster");
    h.eventually("three instances", Duration::from_secs(10), || {
        Harness::procs_with_cmdline(&needle).len() == 3
    })
    .await;
    assert_eq!(h.resurrect().await.expect("resurrect again"), 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        Harness::procs_with_cmdline(&needle).len(),
        3,
        "exactly N processes, not 2N"
    );
    let names: Vec<String> = h.status().await.into_iter().map(|i| i.spec.name).collect();
    assert_eq!(names.len(), 4, "the parent and three instances: {names:?}");
    h.stop("all").await.expect("stop all");
}

/// A row whose stored spec does not parse is registered as an errored placeholder: the boot
/// succeeds, the other apps come back, the placeholder cannot be started and can be deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparsable_spec_json_row_loads_as_broken_placeholder() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("bp-good", &format!("exec sleep {m}")))
        .await
        .expect("start good");
    h.start(h.cmd("bp-bad", "exec sleep 305.6"))
        .await
        .expect("start bad");
    h.shutdown().await;

    let db = oxidepm_db::Database::new(&h.paths.db())
        .await
        .expect("open the database");
    sqlx::query("UPDATE apps SET spec_json = '{ not json' WHERE name = 'bp-bad'")
        .execute(db.pool())
        .await
        .expect("corrupt the row");
    db.close().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");
    h.start_daemon().await;

    let bad = h.state("bp-bad").await;
    assert_eq!(bad.status, AppStatus::Errored);
    let message = bad.last_error.clone().unwrap_or_default();
    assert!(message.contains("stored spec is invalid"), "{message}");
    assert!(message.contains("delete the app"), "{message}");

    let report = h.sup.resurrect_report().await.expect("the boot goes on");
    assert_eq!(report.started, 1, "the good app came back");
    h.pid_of("bp-good").await;
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "bp-bad");
    assert!(
        matches!(report.failed[0].error, Error::StateIncompatible(_)),
        "{:?}",
        report.failed[0].error
    );

    let err = h
        .start(h.cmd("bp-bad", "exec sleep 305.6"))
        .await
        .expect_err("start answers E_STATE_INCOMPATIBLE");
    assert_eq!(err.code(), ErrorCode::StateIncompatible, "{err:?}");
    let err = h.restart("bp-bad").await.expect_err("restart too");
    assert_eq!(err.code(), ErrorCode::StateIncompatible, "{err:?}");

    assert_eq!(h.delete("bp-bad").await.expect("delete works"), 1);
    assert!(h.status().await.iter().all(|i| i.spec.name != "bp-bad"));
    h.stop("bp-good").await.expect("stop");
}

/// `show` and the selectors read the registry: an app that is stopped is as addressable as a
/// running one, and a tag finds it after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_apps_are_addressable_after_a_restart() {
    let mut h = Harness::new().await;
    h.start(h.cmd_with("reg-a", "exec sleep 305.5", json!({ "tags": ["blue"] })))
        .await
        .expect("start");
    h.stop("reg-a").await.expect("stop");
    h.restart_daemon().await;

    let ids = h
        .sup
        .resolve_selector(&oxidepm_core::Selector::parse("@blue"))
        .await
        .expect("tag");
    assert_eq!(ids.len(), 1);
    assert!(h
        .sup
        .resolve_selector(&oxidepm_core::Selector::parse("@green"))
        .await
        .is_err());
    assert!(h
        .sup
        .resolve_selector(&oxidepm_core::Selector::parse("nope"))
        .await
        .is_err());
    assert_eq!(
        h.sup
            .resolve_selector(&oxidepm_core::Selector::All)
            .await
            .unwrap(),
        ids
    );
    assert_eq!(
        h.restart("reg-a")
            .await
            .expect("a stopped, registered app restarts"),
        1
    );
    h.pid_of("reg-a").await;
    h.stop("reg-a").await.expect("stop");
}

// ---- 13b: the versioned, atomic saved file and the import rule ---------------------------

/// `{"version":1,..}`, owner-only, written through a temporary file that is gone afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_file_is_versioned_atomic_and_0600() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new().await;
    h.start(h.cmd("atomic-a", "exec sleep 305.4"))
        .await
        .expect("start");
    // a looser old log file must not matter: the mode is set when the file is created
    for _ in 0..5 {
        h.save(false).await.expect("save");
    }

    let path = h.paths.saved();
    let text = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["version"], 1, "{text}");
    assert!(
        chrono::DateTime::parse_from_rfc3339(value["saved_at"].as_str().unwrap()).is_ok(),
        "{text}"
    );
    assert_eq!(value["apps"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let stale_files: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(
        stale_files.is_empty(),
        "no temporary file is left: {stale_files:?}"
    );
    h.stop("atomic-a").await.expect("stop");
}

/// The format of earlier releases, a bare array, is refused with the remedy; nothing is
/// started from it, and the apps of the registry are not affected by the refusal at boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_array_saved_file_is_rejected() {
    let h = Harness::new().await;
    let legacy =
        serde_json::to_string_pretty(&vec![h.cmd("legacy-a", "exec sleep 305.3")]).unwrap();
    std::fs::create_dir_all(h.paths.saved().parent().unwrap()).unwrap();
    std::fs::write(h.paths.saved(), &legacy).unwrap();

    let err = h
        .sup
        .resurrect_report()
        .await
        .expect_err("a bare array is refused");
    assert_eq!(err.code(), ErrorCode::StateIncompatible, "{err:?}");
    let message = err.to_string();
    assert!(message.contains("pre-1.0 format"), "{message}");
    assert!(message.contains("oxidepm save"), "{message}");
    assert!(h.status().await.is_empty(), "nothing was imported");
    assert_eq!(
        std::fs::read_to_string(h.paths.saved()).unwrap(),
        legacy,
        "the file is untouched"
    );

    // the boot resurrect reports it instead of failing, and still does the registry's part
    let report = h
        .sup
        .resurrect_with(|_, _| {})
        .await
        .expect("boot resurrect");
    assert!(
        matches!(report.saved_error, Some(Error::StateIncompatible(_))),
        "{:?}",
        report.saved_error
    );

    // and a save writes the new format over it
    h.start(h.cmd("legacy-b", "exec sleep 305.3"))
        .await
        .expect("start");
    assert_eq!(h.save(false).await.expect("save over the legacy file"), 1);
    assert_eq!(saved_names(&h), vec!["legacy-b"]);
    h.stop("legacy-b").await.expect("stop");
}

/// Save, delete everything, resurrect: the saved list restores the apps (and a cluster with
/// its instances), as `pm2 resurrect` does after `pm2 delete all`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_all_then_resurrect_restores_saved_apps() {
    let h = Harness::new().await;
    let (m1, m2) = (h.marker(), h.marker());
    h.start(h.cmd("restore-one", &format!("exec sleep {m1}")))
        .await
        .expect("start one");
    h.start(h.cmd_with(
        "restore-clu",
        &format!("exec sleep {m2}"),
        json!({ "instances": 2 }),
    ))
    .await
    .expect("start cluster");
    assert_eq!(h.save(false).await.expect("save"), 2);
    // the cluster is one thing to delete; its two instances go with it
    assert_eq!(
        h.delete("all").await.expect("delete all"),
        2,
        "one app and the cluster"
    );
    assert!(h.status().await.is_empty());
    assert!(Harness::procs_with_cmdline(&format!("sleep {m1}")).is_empty());

    let report = h.sup.resurrect_report().await.expect("resurrect");
    assert_eq!((report.started, report.imported), (2, 2), "{report:?}");
    h.pid_of("restore-one").await;
    h.eventually("the cluster is up", Duration::from_secs(10), || {
        Harness::procs_with_cmdline(&format!("sleep {m2}")).len() == 2
    })
    .await;
    let mut names: Vec<String> = h.status().await.into_iter().map(|i| i.spec.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "restore-clu",
            "restore-clu-0",
            "restore-clu-1",
            "restore-one"
        ]
    );

    // the registry has them now: a second resurrect imports and starts nothing
    let again = h.sup.resurrect_report().await.expect("resurrect again");
    assert_eq!((again.started, again.imported), (0, 0), "{again:?}");
    h.stop("all").await.expect("stop all");
}

/// A saved entry with an unknown field is reported for
/// that app; the other entries are imported and started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_saved_entry_does_not_block_other_apps() {
    let h = Harness::new().await;
    let (m1, m2) = (h.marker(), h.marker());
    let first = serde_json::to_value(h.cmd("entry-first", &format!("exec sleep {m1}"))).unwrap();
    let mut removed = serde_json::to_value(h.cmd("entry-invalid", "exec sleep 305.2")).unwrap();
    removed["unknown_options"] = json!({ "mode": "x" });
    let last = serde_json::to_value(h.cmd("entry-last", &format!("exec sleep {m2}"))).unwrap();
    std::fs::create_dir_all(h.paths.saved().parent().unwrap()).unwrap();
    let file =
        json!({ "version": 1, "saved_at": "2026-01-01T00:00:00Z", "apps": [first, removed, last] });
    std::fs::write(h.paths.saved(), file.to_string()).unwrap();

    let report = h.sup.resurrect_report().await.expect("the file is usable");
    assert_eq!(report.started, 2, "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].name, "entry-invalid");
    assert!(report.failed[0]
        .error
        .to_string()
        .contains("unknown_options"));
    assert_eq!(report.saved_rejected, 1);
    h.pid_of("entry-first").await;
    h.pid_of("entry-last").await;
    assert!(h
        .status()
        .await
        .iter()
        .all(|i| i.spec.name != "entry-invalid"));
    h.stop("all").await.expect("stop all");
}
