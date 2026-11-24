//! Characterisation of save / resurrect.

mod common;

use common::*;

/// Shutdown (what a daemon does on SIGTERM or `Kill`): the children are dead afterwards and
/// `saved.json` lists the apps that were running, and only those.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_children_and_writes_saved_file() {
    let h = Harness::new().await;
    let (m_up, m_down) = (h.marker(), h.marker());
    h.start(h.cmd("down-run", &format!("exec sleep {m_up}")))
        .await
        .expect("start");
    h.start(h.cmd("down-stopped", &format!("exec sleep {m_down}")))
        .await
        .expect("start");
    let pid = h.pid_of("down-run").await;
    h.stop("down-stopped").await.expect("stop");

    h.shutdown().await;

    assert!(!Harness::pid_alive(pid), "the running app was stopped");
    assert!(Harness::procs_with_cmdline(&format!("sleep {m_up}")).is_empty());
    let saved = std::fs::read_to_string(h.paths.saved()).expect("saved.json");
    let names: Vec<_> = saved_entries(&saved)
        .iter()
        .map(|a| a["name"].clone())
        .collect();
    assert_eq!(
        names,
        vec![serde_json::json!("down-run")],
        "running apps only: {saved}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_skips_apps_already_active() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd("persist-active", &format!("exec sleep {m}")))
        .await
        .expect("start");
    let pid = h.pid_of("persist-active").await;

    assert_eq!(h.save(false).await.expect("save"), 1);
    assert!(
        h.paths.saved().exists(),
        "saved.json lands in the test home"
    );

    assert_eq!(
        h.resurrect().await.expect("resurrect"),
        0,
        "nothing to start"
    );
    assert_eq!(
        Harness::procs_with_cmdline(&needle),
        vec![pid],
        "no duplicate"
    );
    assert_eq!(h.status().await.len(), 1);
}

/// An entry with a key the spec does not have is that entry's problem, reported by name; the
/// file and the other entries are fine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_reports_unknown_keys_in_a_saved_entry() {
    let h = Harness::new().await;
    let spec = serde_json::to_value(h.cmd("persist-unknown", "exec sleep 300")).unwrap();
    let mut spec = spec;
    spec.as_object_mut()
        .unwrap()
        .insert("bogus_key".to_string(), serde_json::json!(1));
    std::fs::create_dir_all(h.paths.saved().parent().unwrap()).unwrap();
    std::fs::write(
        h.paths.saved(),
        serde_json::json!({ "version": 1, "apps": [spec] }).to_string(),
    )
    .unwrap();

    let report = h
        .sup
        .resurrect_report()
        .await
        .expect("the file itself is fine");
    assert_eq!(report.started, 0);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "persist-unknown");
    assert!(
        report.failed[0].error.to_string().contains("bogus_key"),
        "{:?}",
        report.failed[0].error
    );
    assert!(h.status().await.is_empty(), "nothing registered");
}

/// Everything an operator can configure shows up in `status` after the daemon restarted:
/// `status` reads the database, which used to keep 13 of the ~35 spec fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_after_daemon_restart_shows_full_spec() {
    let mut h = Harness::new().await;
    h.start(h.cmd("full-db", "exec sleep 300"))
        .await
        .expect("start dependency");
    let web = h.cmd_with(
        "full-web",
        "exec sleep 300",
        serde_json::json!({
            "tags": ["frontend", "prod"],
            "env": { "KEEP": "yes" },
            "hooks": { "on_start": "true", "on_crash": "echo crashed" },
            "health_check": {
                "script": "true", "expected_status": [200],
                "interval_secs": 30, "timeout_secs": 5, "retries": 3
            },
            "depends_on": ["full-db"],
            "created_at": "2020-01-01T00:00:00Z",
            "kill_timeout_ms": 4321,
            "restart_cron": "0 4 * * *",
            "log_max_size": 1000000,
            "log_compress": true
        }),
    );
    let before = serde_json::to_value(&web).unwrap();
    let started_after = chrono::Utc::now();
    h.start(web).await.expect("start web");
    h.pid_of("full-web").await;

    h.restart_daemon().await;

    let info = h
        .status()
        .await
        .into_iter()
        .find(|i| i.spec.name == "full-web")
        .expect("app is listed after the restart");
    let spec = &info.spec;
    assert_eq!(spec.tags, vec!["frontend", "prod"]);
    assert_eq!(spec.env.get("KEEP").map(String::as_str), Some("yes"));
    assert_eq!(spec.hooks.on_start.as_deref(), Some("true"));
    assert_eq!(spec.hooks.on_crash.as_deref(), Some("echo crashed"));
    assert_eq!(
        spec.health_check.as_ref().and_then(|c| c.script.as_deref()),
        Some("true")
    );
    assert_eq!(spec.depends_on, vec!["full-db"]);
    // a client does not choose `created_at`: the daemon stamps it at ingress, and the stored
    // value survives the restart
    assert!(
        spec.created_at >= started_after,
        "created_at is the start time, not the client's"
    );
    assert_eq!(spec.kill_timeout_ms, 4321);
    assert_eq!(spec.restart_cron.as_deref(), Some("0 4 * * *"));
    assert_eq!(spec.log_max_size, Some(1_000_000));
    assert!(spec.log_compress);

    // whole spec, modulo the id the database assigned and the creation time the daemon stamped
    let mut after = serde_json::to_value(spec).unwrap();
    after["id"] = before["id"].clone();
    after["created_at"] = before["created_at"].clone();
    assert_eq!(after, before);
}

/// Tags are part of the stored spec, so `@tag` still resolves after a restart (and does not
/// pick up the app that happens to be *named* like the tag).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_selector_by_tag_works_after_restart() {
    let mut h = Harness::new().await;
    h.start(h.cmd_with(
        "tag-a",
        "exec sleep 300",
        serde_json::json!({ "tags": ["frontend"] }),
    ))
    .await
    .expect("start tagged");
    h.start(h.cmd("frontend", "exec sleep 300"))
        .await
        .expect("start same-named");
    h.start(h.cmd("tag-b", "exec sleep 300"))
        .await
        .expect("start untagged");

    h.restart_daemon().await;

    let ids = h
        .sup
        .resolve_selector(&oxidepm_core::Selector::parse("@frontend"))
        .await
        .expect("tag resolves after restart");
    let names: Vec<String> = h
        .status()
        .await
        .into_iter()
        .filter(|i| ids.contains(&i.spec.id))
        .map(|i| i.spec.name)
        .collect();
    assert_eq!(names, vec!["tag-a"]);
}

// ---- dependencies ---------------------------------------------------------------

/// Resurrect starts the apps together and the dependency wait orders them: a saved file that
/// lists the dependent first still brings the dependency up before it.
///
/// "Running" means the process was spawned, not that its shell has got as far as writing the
/// marker, so the test waits for both marker files and does not compare the two stamps (the
/// dependency's shell can be slower than the dependent's after the gate opens). What proves the
/// order is the dependency's `startup_delay_ms`: the dependent can only be launched after it, so
/// its stamp cannot be earlier than the start of the resurrect plus the delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_orders_dependencies_before_dependents() {
    const DELAY_MS: u128 = 800;
    let h = Harness::new().await;
    let m = h.marker();
    let (db_at, web_at) = (h.file("rd-db.at"), h.file("rd-web.at"));
    let noting =
        |file: &std::path::Path| format!("date +%s%N >> {}; exec sleep {m}", file.display());
    let web = h.cmd_with(
        "rd-web",
        &noting(&web_at),
        serde_json::json!({ "depends_on": ["rd-db"] }),
    );
    let db = h.cmd_with(
        "rd-db",
        &noting(&db_at),
        serde_json::json!({ "startup_delay_ms": DELAY_MS as u64 }),
    );
    std::fs::write(h.paths.saved(), saved_json(&[web, db])).expect("write saved.json");

    let now_ns = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    };
    let started_at = now_ns();
    let report = h.sup.resurrect_report().await.expect("resurrect");
    assert_eq!(report.started, 2, "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    // both markers, written by the shells some milliseconds after the spawn
    let stamp = |p: &std::path::Path| -> Option<u128> {
        std::fs::read_to_string(p).ok()?.trim().parse().ok()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let (db_stamp, web_stamp) = loop {
        if let (Some(d), Some(w)) = (stamp(&db_at), stamp(&web_at)) {
            break (d, w);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "markers not written: db {:?}, web {:?}",
            stamp(&db_at),
            stamp(&web_at)
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    h.wait_status("rd-web", oxidepm_core::AppStatus::Running)
        .await;

    let earliest = started_at + DELAY_MS * 1_000_000;
    assert!(
        db_stamp >= earliest,
        "db launched before its startup delay passed"
    );
    assert!(
        web_stamp >= earliest,
        "web launched before db was up (db first)"
    );
    h.stop("all").await.expect("stop");
}

/// Five dependents and their dependency with four launch slots: if a dependent held a slot while
/// it waited, four of them would hold them all and the dependency could never launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resurrect_with_more_dependents_than_launch_slots() {
    let h = Harness::new().await;
    let m = h.marker();
    // the dependents are registered first (lowest ids), the dependency last
    let mut specs: Vec<_> = (0..5)
        .map(|i| {
            h.cmd_with(
                &format!("slots-web-{i}"),
                &format!("exec sleep {m}"),
                serde_json::json!({ "depends_on": ["slots-db"] }),
            )
        })
        .collect();
    specs.push(h.cmd("slots-db", &format!("exec sleep {m}")));
    std::fs::write(h.paths.saved(), saved_json(&specs)).expect("write saved.json");

    let report = h.sup.resurrect_report().await.expect("resurrect");
    assert_eq!(report.started, 6, "{report:?}");
    for spec in &specs {
        h.wait_status(&spec.name, oxidepm_core::AppStatus::Running)
            .await;
    }
    h.stop("all").await.expect("stop");
}

/// Fifty apps in a chain, listed in the reverse of their start order: all of them come up and
/// none of them fails because a dependency looked missing while the batch registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_50_apps_in_reversed_dependency_order_succeeds() {
    let h = Harness::new().await;
    let m = h.marker();
    // app 49 depends on nothing, app i depends on app i + 1; the list starts with app 0
    let specs: Vec<_> = (0..50)
        .map(|i| {
            let deps: Vec<String> = if i < 49 {
                vec![format!("chain-{}", i + 1)]
            } else {
                vec![]
            };
            h.cmd_with(
                &format!("chain-{i}"),
                &format!("exec sleep {m}"),
                serde_json::json!({ "depends_on": deps }),
            )
        })
        .collect();

    let items = h.sup.start_batch(specs).await.expect("batch");

    assert_eq!(items.len(), 50);
    for item in &items {
        assert!(item.result.is_ok(), "{}: {:?}", item.name, item.result);
    }
    for i in 0..50 {
        h.wait_status(&format!("chain-{i}"), oxidepm_core::AppStatus::Running)
            .await;
    }
    h.stop("all").await.expect("stop");
}
