//! `depends_on`: the checks before a start, the event-driven wait that fails closed,
//! the rule that only operator first launches wait, and the order in which apps are stopped.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use oxidepm_core::{AppStatus, Error, ErrorCode};
use serde_json::json;

/// `sh` script that notes the nanosecond it started in `marker_file`, then sleeps.
fn noting(marker_file: &std::path::Path, sleep: &str) -> String {
    format!(
        "date +%s%N >> {}; exec sleep {sleep}",
        marker_file.display()
    )
}

fn nanos(text: &str) -> u128 {
    text.lines()
        .next()
        .expect("a timestamp")
        .trim()
        .parse()
        .expect("nanoseconds")
}

/// The text of `path` once it holds a line (a shell creates the file before it writes to it).
async fn timestamp(h: &Harness, path: &std::path::Path) -> u128 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = h.wait_file(path).await;
        if !text.trim().is_empty() {
            return nanos(&text);
        }
        assert!(Instant::now() < deadline, "{} stayed empty", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A name nobody registered is refused at once, and nothing is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_missing_dependency_fails_immediately() {
    let h = Harness::new().await;
    let m = h.marker();
    let spec = h.cmd_with(
        "needs-nope",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["nope"] }),
    );

    let begun = Instant::now();
    let err = h.start(spec).await.expect_err("missing dependency");

    assert!(matches!(err, Error::DependencyMissing(_)), "{err}");
    assert_eq!(err.code(), ErrorCode::DependencyMissing);
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "took {:?}",
        begun.elapsed()
    );
    assert!(h.status().await.is_empty(), "no row, no actor");
    assert!(Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty());
}

/// Two apps that depend on each other in one batch are both refused, naming the cycle; an app
/// of the same batch that depends on neither starts, one that depends on them does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_cycle_is_rejected() {
    let h = Harness::new().await;
    let m = h.marker();
    let sleeper = |name: &str, deps: &[&str]| {
        h.cmd_with(
            name,
            &format!("exec sleep {m}"),
            json!({ "depends_on": deps }),
        )
    };
    let items = h
        .sup
        .start_batch(vec![
            sleeper("cyc-a", &["cyc-b"]),
            sleeper("cyc-b", &["cyc-a"]),
            sleeper("cyc-user", &["cyc-a"]),
            sleeper("cyc-free", &[]),
        ])
        .await
        .expect("batch");

    let by_name = |n: &str| items.iter().find(|i| i.name == n).unwrap();
    for name in ["cyc-a", "cyc-b"] {
        let err = by_name(name).result.as_ref().expect_err("on the cycle");
        assert_eq!(err.code(), ErrorCode::DependencyCycle, "{name}: {err}");
        assert!(
            err.to_string().contains("cyc-a -> cyc-b -> cyc-a")
                || err.to_string().contains("cyc-b -> cyc-a -> cyc-b"),
            "{err}"
        );
    }
    let err = by_name("cyc-user")
        .result
        .as_ref()
        .expect_err("depends on the cycle");
    assert_eq!(err.code(), ErrorCode::DependencyFailed, "{err}");
    assert!(by_name("cyc-free").result.is_ok());

    let names: Vec<String> = h.status().await.into_iter().map(|a| a.spec.name).collect();
    assert_eq!(names, vec!["cyc-free"], "the refused apps left no row");
    h.stop("all").await.expect("stop");
}

/// A change of a registered app's dependencies that closes a cycle is refused too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarting_an_app_with_a_new_dependency_that_closes_a_cycle_is_rejected() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("cl-base", &format!("exec sleep {m}")))
        .await
        .expect("start base");
    h.start(h.cmd_with(
        "cl-top",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["cl-base"] }),
    ))
    .await
    .expect("start top");
    h.stop("all").await.expect("stop");

    // cl-base would now depend on cl-top, which depends on cl-base
    let spec = h.cmd_with(
        "cl-base",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["cl-top"] }),
    );
    let err = h.start(spec).await.expect_err("cycle");
    assert_eq!(err.code(), ErrorCode::DependencyCycle, "{err}");
}

/// The start of an app whose dependency is not up yet returns at once with status `waiting`, and
/// the daemon answers other requests meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependent_waits_without_blocking_other_requests() {
    let h = Harness::new().await;
    let m = h.marker();
    let sup = Arc::clone(&h.sup);
    let db = h.cmd_with(
        "wait-db",
        &format!("exec sleep {m}"),
        json!({ "startup_delay_ms": 2500 }),
    );
    let db_start = tokio::spawn(async move { sup.start(db).await });
    h.wait_registered("wait-db").await;
    h.wait_status("wait-db", AppStatus::Waiting).await;

    let begun = Instant::now();
    h.start(h.cmd_with(
        "wait-web",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["wait-db"] }),
    ))
    .await
    .expect("accepted");
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "the start blocked for {:?}",
        begun.elapsed()
    );
    assert_eq!(h.state("wait-web").await.status, AppStatus::Waiting);
    let begun = Instant::now();
    assert_eq!(h.status().await.len(), 2);
    assert!(begun.elapsed() < Duration::from_millis(500));

    db_start.await.unwrap().expect("db started");
    h.wait_status("wait-web", AppStatus::Running).await;
    h.stop("all").await.expect("stop");
}

/// The dependent launches after, never before, its dependency is running.
///
/// What the gate guarantees is that the dependent's process is created after the dependency's
/// process exists and its status is `running`, not that the dependent's shell reaches its first
/// command after the dependency's shell does: the two shells start a moment apart and, on a busy
/// machine, the later one can run its `date` first (up to tens of milliseconds were seen). So the
/// test does not compare the two timestamps with each other. It checks the guarantee from the
/// other side: the dependency is held back by its 2 s startup delay, so a dependent that did not
/// wait for it would start at once. Both timestamps must be after the delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependent_starts_only_after_dependency_is_running() {
    const DELAY_NS: i128 = 2_000_000_000;
    /// Clock slack between the monotonic delay timer and the wall clock the shells read.
    const SLACK_NS: i128 = 20_000_000;
    let h = Harness::new().await;
    let m = h.marker();
    let (db_at, web_at) = (h.file("db.at"), h.file("web.at"));
    let begun = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i128;
    let items = h
        .sup
        .start_batch(vec![
            h.cmd_with(
                "ord-web",
                &noting(&web_at, &m),
                json!({ "depends_on": ["ord-db"] }),
            ),
            h.cmd_with(
                "ord-db",
                &noting(&db_at, &m),
                json!({ "startup_delay_ms": 2000 }),
            ),
        ])
        .await
        .expect("batch");
    assert!(items.iter().all(|i| i.result.is_ok()), "{items:?}");

    let (db, web) = (
        timestamp(&h, &db_at).await as i128,
        timestamp(&h, &web_at).await as i128,
    );
    assert!(
        db >= begun + DELAY_NS - SLACK_NS,
        "db started {} ns after the batch",
        db - begun
    );
    assert!(
        web >= begun + DELAY_NS - SLACK_NS,
        "web started {} ns after the batch, before db's delay ended",
        web - begun
    );
    assert!(
        web - db < DELAY_NS,
        "web followed db closely, not a poll interval later (db {db}, web {web})"
    );
    h.wait_status("ord-web", AppStatus::Running).await;
    h.stop("all").await.expect("stop");
}

/// A dependency that never becomes ready fails the dependent closed at the timeout; its process
/// is never created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_never_ready_fails_closed_after_timeout() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with("never-db", &format!("exec sleep {m}"), failing_probe()))
        .await
        .expect("db");
    let web_at = h.file("never-web.at");
    h.start(h.cmd_with(
        "never-web",
        &noting(&web_at, &m),
        json!({ "depends_on": ["never-db"], "depends_timeout_secs": 2 }),
    ))
    .await
    .expect("accepted as waiting");
    assert_eq!(h.state("never-web").await.status, AppStatus::Waiting);

    let state = h
        .wait_until_for("never-web", "the timeout", Duration::from_secs(6), |s| {
            s.status == AppStatus::Errored
        })
        .await;
    // The run state keeps the text of the failure, not its error code (E_DEPENDENCY_TIMEOUT),
    // and a launch that failed in the background has no caller to hand the code to. The text is
    // asserted whole: it names the dependency and the timeout, and it is the message that
    // `Error::DependencyTimeout` carries.
    let why = state.last_error.expect("an explanation");
    assert_eq!(why, "dependency 'never-db' not running after 2s");
    assert_eq!(state.pid, None);
    assert_eq!(state.restarts, 0);
    assert!(!web_at.exists(), "the dependent never launched");
    h.stop("all").await.expect("stop");
}

/// `depends_fail_open` is the one escape hatch: the timeout becomes a warning and a launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depends_fail_open_launches_after_the_timeout() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd_with("open-db", &format!("exec sleep {m}"), failing_probe()))
        .await
        .expect("db");
    let web_at = h.file("open-web.at");
    h.start(h.cmd_with(
        "open-web",
        &noting(&web_at, &m),
        json!({ "depends_on": ["open-db"], "depends_timeout_secs": 1, "depends_fail_open": true }),
    ))
    .await
    .expect("accepted");

    h.wait_status("open-web", AppStatus::Running).await;
    // `running` is published when the shell is spawned; the shell writes the file a moment later
    // (on a loaded machine, a long moment)
    h.wait_file(&web_at).await;
    h.stop("all").await.expect("stop");
}

/// A dependency that ends errored fails the dependent at once, not at the timeout. (The
/// dependency never reaches `running`: its probe fails from the first second and every failure
/// is a restart, until the crash-loop breaker gives up.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_errored_fails_dependent_fast() {
    let h = Harness::new().await;
    let m = h.marker();
    let web_at = h.file("err-web.at");
    let db = h.cmd_with(
        "err-db",
        &format!("exec sleep {m}"),
        json!({
            "health_check": {
                "script": "exit 1", "expected_status": [200], "interval_secs": 1,
                "timeout_secs": 1, "retries": 1, "start_period_secs": 0, "action": "restart"
            },
            "restart_policy": { "max_restarts": 1, "restart_delay_ms": 50 }
        }),
    );
    let items = h
        .sup
        .start_batch(vec![
            db,
            h.cmd_with(
                "err-web",
                &noting(&web_at, &m),
                json!({ "depends_on": ["err-db"] }),
            ),
        ])
        .await
        .expect("batch");
    assert!(items.iter().all(|i| i.result.is_ok()), "{items:?}");
    assert_eq!(h.state("err-web").await.status, AppStatus::Waiting);

    h.wait_until_for(
        "err-db",
        "the dependency to give up",
        Duration::from_secs(15),
        |s| s.status == AppStatus::Errored,
    )
    .await;
    let state = h
        .wait_until_for(
            "err-web",
            "the dependent to fail",
            Duration::from_secs(2),
            |s| s.status == AppStatus::Errored,
        )
        .await;
    assert_eq!(
        state.last_error.as_deref(),
        Some("dependency 'err-db' errored")
    );
    assert!(!web_at.exists(), "the dependent never launched");
}

/// A single start of an app whose dependency is stopped is refused with the remedy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_stopped_is_rejected_on_single_start() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("st-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.stop("st-db").await.expect("stop db");

    let begun = Instant::now();
    let err = h
        .start(h.cmd_with(
            "st-web",
            &format!("exec sleep {m}"),
            json!({ "depends_on": ["st-db"] }),
        ))
        .await
        .expect_err("dependency at rest");

    assert_eq!(err.code(), ErrorCode::DependencyFailed);
    assert_eq!(
        err.to_string(),
        "dependency 'st-db' is stopped; start it first"
    );
    assert!(begun.elapsed() < Duration::from_secs(1));
    assert_eq!(h.status().await.len(), 1, "no row for the refused app");
}

/// `stop all` stops dependents before their dependencies: a layer is told to stop only when the
/// layer above it is gone. (Each app records the moment its TERM arrives.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_all_stops_dependents_before_dependencies() {
    let h = Harness::new().await;
    let order = h.file("stop-order");
    let app = |name: &str, deps: &[&str]| {
        let ready = h.file(&format!("{name}.ready"));
        let sh = format!(
            "trap 'date +%s%N >> {o}.{name}; echo {name} >> {o}; exit 0' TERM; touch {r}; while :; do sleep 0.05; done",
            o = order.display(),
            r = ready.display()
        );
        h.cmd_with(name, &sh, json!({ "depends_on": deps }))
    };
    // registered dependencies first, so that stopping by id would stop them first
    let specs = vec![
        app("all-db", &[]),
        app("all-cache", &[]),
        app("all-web", &["all-db", "all-cache"]),
        app("all-api", &["all-web"]),
    ];
    let items = h.sup.start_batch(specs).await.expect("batch");
    assert!(items.iter().all(|i| i.result.is_ok()), "{items:?}");
    for name in ["all-api", "all-web", "all-cache", "all-db"] {
        h.wait_file(&h.file(&format!("{name}.ready"))).await;
    }

    let stopped = h.stop("all").await.expect("stop all");
    assert_eq!(stopped, 4);

    let lines: Vec<String> = std::fs::read_to_string(&order)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "all-api");
    assert_eq!(lines[1], "all-web");
    let rest: std::collections::BTreeSet<&str> = lines[2..].iter().map(String::as_str).collect();
    assert_eq!(rest, ["all-cache", "all-db"].into_iter().collect());
}

/// An operator restart of an app that is up skips the dependency gate: its dependency being
/// stopped now must not turn the restart into an outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_of_running_app_does_not_wait_for_stopped_dependency() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("rs-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.start(h.cmd_with(
        "rs-web",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["rs-db"] }),
    ))
    .await
    .expect("web");
    h.wait_status("rs-web", AppStatus::Running).await;
    let before = h.pid_of("rs-web").await;
    h.stop("rs-db").await.expect("stop db");

    let begun = Instant::now();
    assert_eq!(h.restart("rs-web").await.expect("restart"), 1);

    let state = h.state("rs-web").await;
    assert_eq!(state.status, AppStatus::Running, "{state:?}");
    assert_ne!(state.pid, Some(before));
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "no wait: {:?}",
        begun.elapsed()
    );
    h.stop("all").await.expect("stop");
}

/// A start of an app that is already up changes nothing, and whatever its dependencies are
/// doing now is none of its business: the answer is the one an app without dependencies gets
/// (`E_APP_EXISTS`), not `E_DEPENDENCY_FAILED`. The same holds for each app of a batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_of_a_running_app_with_a_stopped_dependency_is_unchanged() {
    let h = Harness::new().await;
    let m = h.marker();
    let db = || h.cmd("up-db", &format!("exec sleep {m}"));
    let web = || {
        h.cmd_with(
            "up-web",
            &format!("exec sleep {m}"),
            json!({ "depends_on": ["up-db"] }),
        )
    };
    h.start(db()).await.expect("db");
    h.start(web()).await.expect("web");
    h.wait_status("up-web", AppStatus::Running).await;
    let before = h.pid_of("up-web").await;
    h.stop("up-db").await.expect("stop db");

    // a single start
    let err = h.start(web()).await.expect_err("already running");
    assert!(is_already_exists(&err), "{err}");
    assert_eq!(err.code(), ErrorCode::AppExists, "{err}");

    // the same file started again as a batch: web is reported as already running, and db, which
    // is stopped and in the set, is started
    let items = h.sup.start_batch(vec![web(), db()]).await.expect("batch");
    let web_item = items.iter().find(|i| i.name == "up-web").unwrap();
    let err = web_item.result.as_ref().expect_err("already running");
    assert_eq!(err.code(), ErrorCode::AppExists, "{err}");
    let db_item = items.iter().find(|i| i.name == "up-db").unwrap();
    assert!(db_item.result.is_ok(), "{:?}", db_item.result);

    assert_eq!(
        h.pid_of("up-web").await,
        before,
        "the running app was not touched"
    );
    assert_eq!(h.state("up-web").await.status, AppStatus::Running);
    h.stop("all").await.expect("stop");
}

/// A cluster whose instances are all up is up as well; one with an instance at rest is not,
/// and its start still checks the dependencies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_of_a_running_cluster_with_a_stopped_dependency_is_unchanged() {
    let h = Harness::new().await;
    let m = h.marker();
    let web = || {
        h.cmd_with(
            "uc-web",
            &format!("exec sleep {m}"),
            json!({ "instances": 2, "depends_on": ["uc-db"] }),
        )
    };
    h.start(h.cmd("uc-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.start(web()).await.expect("cluster");
    h.wait_status("uc-web", AppStatus::Running).await;
    h.stop("uc-db").await.expect("stop db");

    let err = h.start(web()).await.expect_err("already running");
    assert_eq!(err.code(), ErrorCode::AppExists, "{err}");

    h.stop("uc-web-1").await.expect("stop one instance");
    h.wait_status("uc-web-1", AppStatus::Stopped).await;
    let err = h
        .start(web())
        .await
        .expect_err("an instance is at rest and the dependency is not");
    assert_eq!(err.code(), ErrorCode::DependencyFailed, "{err}");
    h.stop("all").await.expect("stop");
}

/// A batch whose only app is up, with a dependency outside the set that is stopped, is the
/// same case without the second app to hide behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_a_running_app_with_a_stopped_outside_dependency_is_unchanged() {
    let h = Harness::new().await;
    let m = h.marker();
    let web = || {
        h.cmd_with(
            "bo-web",
            &format!("exec sleep {m}"),
            json!({ "depends_on": ["bo-db"] }),
        )
    };
    h.start(h.cmd("bo-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.start(web()).await.expect("web");
    h.wait_status("bo-web", AppStatus::Running).await;
    h.stop("bo-db").await.expect("stop db");

    let items = h.sup.start_batch(vec![web()]).await.expect("batch");

    let err = items[0].result.as_ref().expect_err("already running");
    assert_eq!(err.code(), ErrorCode::AppExists, "{err}");
    h.stop("all").await.expect("stop");
}

/// A running app whose dependency is in the same batch and is refused at admission (here a
/// relative cwd) is still answered `E_APP_EXISTS`: it is up, so the "dependency cannot start"
/// pass over the failed apps must skip it, as the dependency check does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_a_running_app_whose_dependency_is_refused_at_admission_is_unchanged() {
    let h = Harness::new().await;
    let m = h.marker();
    let web = || {
        h.cmd_with(
            "ba-web",
            &format!("exec sleep {m}"),
            json!({ "depends_on": ["ba-db"] }),
        )
    };
    h.start(h.cmd("ba-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.start(web()).await.expect("web");
    h.wait_status("ba-web", AppStatus::Running).await;
    h.stop("ba-db").await.expect("stop db");

    let bad_db = h.cmd_with(
        "ba-db",
        &format!("exec sleep {m}"),
        json!({ "cwd": "relative/dir" }),
    );
    let items = h.sup.start_batch(vec![web(), bad_db]).await.expect("batch");

    let by_name = |name: &str| items.iter().find(|i| i.name == name).expect("item");
    let db_err = by_name("ba-db")
        .result
        .as_ref()
        .expect_err("refused at admission");
    assert_eq!(db_err.code(), ErrorCode::InvalidSpec, "{db_err}");
    let web_err = by_name("ba-web")
        .result
        .as_ref()
        .expect_err("already running");
    assert_eq!(web_err.code(), ErrorCode::AppExists, "{web_err}");
    assert_eq!(h.state("ba-web").await.status, AppStatus::Running);
    assert_eq!(h.state("ba-db").await.status, AppStatus::Stopped);
    h.stop("all").await.expect("stop");
}

/// An app in backoff is up as well: its start is answered `E_APP_EXISTS` while a dependency is
/// stopped, and the pending restart is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_of_an_app_in_backoff_with_a_stopped_dependency_is_unchanged() {
    let h = Harness::new().await;
    let m = h.marker();
    let go = h.file("bo.go");
    let web = || {
        h.cmd_with(
            "bk-web",
            &format!(
                "while [ ! -f {} ]; do sleep 0.05; done; exit 1",
                go.display()
            ),
            json!({ "depends_on": ["bk-db"], "restart_policy": { "restart_delay_ms": 60000 } }),
        )
    };
    h.start(h.cmd("bk-db", &format!("exec sleep {m}")))
        .await
        .expect("db");
    h.start(web()).await.expect("web");
    h.wait_status("bk-web", AppStatus::Running).await;
    h.stop("bk-db").await.expect("stop db");
    std::fs::write(&go, "go").expect("go file");
    h.wait_status("bk-web", AppStatus::Backoff).await;

    let err = h.start(web()).await.expect_err("in backoff");
    assert_eq!(err.code(), ErrorCode::AppExists, "{err}");
    assert_eq!(h.state("bk-web").await.status, AppStatus::Backoff);
    h.stop("all").await.expect("stop");
}

/// An automatic restart skips the gate as well: a crashed app comes back through the normal
/// backoff even while its dependency is errored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_dependent_restarts_while_dependency_is_down() {
    let h = Harness::new().await;
    let m = h.marker();
    let go = h.file("db.go");
    // db runs until the file appears, then exits non-zero; with one restart allowed it ends errored
    let db = h.cmd_with(
        "cr-db",
        &format!(
            "while [ ! -f {} ]; do sleep 0.05; done; exit 1",
            go.display()
        ),
        json!({ "restart_policy": { "max_restarts": 1, "restart_delay_ms": 50 } }),
    );
    h.start(db).await.expect("db");
    h.start(h.cmd_with(
        "cr-web",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["cr-db"], "restart_policy": { "restart_delay_ms": 300 } }),
    ))
    .await
    .expect("web");
    h.wait_status("cr-web", AppStatus::Running).await;
    std::fs::write(&go, "").unwrap();
    h.wait_until_for(
        "cr-db",
        "the dependency to give up",
        Duration::from_secs(8),
        |s| s.status == AppStatus::Errored,
    )
    .await;

    let first = h.pid_of("cr-web").await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(first as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill web");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = h.state("cr-web").await;
        assert_ne!(
            state.status,
            AppStatus::Errored,
            "the gate must not strand it: {state:?}"
        );
        if state.status == AppStatus::Running && state.pid.is_some() && state.pid != Some(first) {
            break;
        }
        assert!(Instant::now() < deadline, "no relaunch: {state:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.stop("all").await.expect("stop");
}

/// A dependent of a cluster is woken by the group's derived status, not by a poll or the timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_dependency_wakes_on_group_snapshot() {
    let h = Harness::new().await;
    let m = h.marker();
    let sup = Arc::clone(&h.sup);
    let cluster = h.cmd_with(
        "cd-web",
        &format!("exec sleep {m}"),
        json!({ "instances": 2, "startup_delay_ms": 2000 }),
    );
    let cluster_start = tokio::spawn(async move { sup.start(cluster).await });
    h.wait_registered("cd-web").await;
    h.wait_until("cd-web", "the cluster on its way up", |s| {
        matches!(
            s.status,
            AppStatus::Starting | AppStatus::Waiting | AppStatus::Building
        )
    })
    .await;

    h.start(h.cmd_with(
        "cd-api",
        &format!("exec sleep {m}"),
        json!({ "depends_on": ["cd-web"] }),
    ))
    .await
    .expect("accepted");
    assert_eq!(h.state("cd-api").await.status, AppStatus::Waiting);

    cluster_start.await.unwrap().expect("cluster started");
    h.wait_status("cd-web", AppStatus::Running).await;
    let up = Instant::now();
    h.wait_until_for(
        "cd-api",
        "the dependent to follow",
        Duration::from_secs(5),
        |s| s.status == AppStatus::Running,
    )
    .await;
    assert!(
        up.elapsed() < Duration::from_secs(3),
        "woken after {:?}",
        up.elapsed()
    );
    h.stop("all").await.expect("stop");
}

/// The batch reports each app on its own: one that cannot start does not stop the others, and
/// the result lists them in the order given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_reports_each_app_in_order() {
    let h = Harness::new().await;
    let m = h.marker();
    let items = h
        .sup
        .start_batch(vec![
            h.cmd("b-one", &format!("exec sleep {m}")),
            h.cmd_with(
                "b-bad",
                &format!("exec sleep {m}"),
                json!({ "depends_on": ["b-nowhere"] }),
            ),
            h.cmd_with(
                "b-dependent",
                &format!("exec sleep {m}"),
                json!({ "depends_on": ["b-bad"] }),
            ),
            h.cmd_with(
                "b-two",
                &format!("exec sleep {m}"),
                json!({ "depends_on": ["b-one"] }),
            ),
        ])
        .await
        .expect("batch");

    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["b-one", "b-bad", "b-dependent", "b-two"]);
    assert!(items[0].result.is_ok());
    assert_eq!(
        items[1].result.as_ref().unwrap_err().code(),
        ErrorCode::DependencyMissing
    );
    let dependent = items[2].result.as_ref().unwrap_err();
    assert_eq!(dependent.code(), ErrorCode::DependencyFailed);
    assert!(dependent.to_string().contains("'b-bad'"), "{dependent}");
    assert!(items[3].result.is_ok());
    h.wait_status("b-two", AppStatus::Running).await;

    // a name twice is a request error
    let err = h
        .sup
        .start_batch(vec![h.cmd("dup", "true"), h.cmd("dup", "true")])
        .await
        .expect_err("duplicate");
    assert!(err.to_string().contains("more than once"), "{err}");
    h.stop("all").await.expect("stop");
}

/// A batch that meets a closed gate refuses the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_is_refused_once_shut_down() {
    let h = Harness::new().await;
    h.shutdown().await;
    let err = h
        .sup
        .start_batch(vec![h.cmd("late", "true")])
        .await
        .expect_err("closed");
    assert!(is_shutting_down(&err), "{err}");
}

/// `db <- web <- api` with TERM-trapping apps and long kill timeouts: the budget is shared out
/// over the layers, the order is api, web, db, and the daemon is done within the budget plus
/// the grace.
#[cfg(feature = "slow-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_with_dependency_chain_stops_in_reverse_order_within_budget() {
    let h = Harness::new().await;
    let order = h.file("chain-order");
    let app = |name: &str, deps: &[&str]| {
        let ready = h.file(&format!("{name}.ready"));
        // ignores TERM after noting it, so only the kill cap ends it
        let sh = format!(
            "trap 'echo {name} >> {}' TERM; touch {}; while :; do sleep 0.05; done",
            order.display(),
            ready.display()
        );
        h.cmd_with(
            name,
            &sh,
            json!({ "depends_on": deps, "kill_timeout_ms": 30000 }),
        )
    };
    let items = h
        .sup
        .start_batch(vec![
            app("ch-db", &[]),
            app("ch-web", &["ch-db"]),
            app("ch-api", &["ch-web"]),
        ])
        .await
        .expect("batch");
    assert!(items.iter().all(|i| i.result.is_ok()));
    for name in ["ch-db", "ch-web", "ch-api"] {
        h.wait_file(&h.file(&format!("{name}.ready"))).await;
    }

    let begun = Instant::now();
    h.sup.shutdown(Duration::from_secs(12)).await;

    let stopped: Vec<String> = std::fs::read_to_string(&order)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(stopped, vec!["ch-api", "ch-web", "ch-db"]);
    assert!(
        begun.elapsed() < Duration::from_secs(12 + 20),
        "took {:?}",
        begun.elapsed()
    );
}
