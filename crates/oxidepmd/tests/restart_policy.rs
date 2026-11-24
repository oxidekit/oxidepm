//! Characterisation of the crash restart budget.

mod common;

use std::time::Duration;

use common::*;
use oxidepm_core::AppStatus;
use serde_json::json;

const CRASH_LOOP: &str = "echo launch >> '{}'; exit 1";

fn crash_loop_spec(h: &Harness, name: &str) -> (oxidepm_core::AppSpec, std::path::PathBuf) {
    let launches = h.file("launches");
    let sh = CRASH_LOOP.replace("{}", &launches.display().to_string());
    let spec = h.cmd_with(
        name,
        &sh,
        json!({ "restart_policy": { "restart_delay_ms": 20, "max_restarts": 3 } }),
    );
    (spec, launches)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_loop_stops_after_max_restarts() {
    let h = Harness::new().await;
    let (spec, launches) = crash_loop_spec(&h, "pol-loop");
    h.start(spec).await.expect("start");

    // `errored` also appears briefly between a crash and its restart, so wait for the budget too.
    h.wait_until("pol-loop", "errored with the budget spent", |s| {
        s.status == AppStatus::Errored && s.restarts >= 3
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let state = h.state("pol-loop").await;
    assert_eq!(state.status, AppStatus::Errored);
    assert_eq!(state.restarts, 3);
    assert_eq!(state.pid, None);
    assert_eq!(
        Harness::line_count(&launches),
        4,
        "first launch plus three restarts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_restart_of_errored_app_gets_fresh_budget() {
    let h = Harness::new().await;
    let (spec, launches) = crash_loop_spec(&h, "pol-fresh");
    h.start(spec).await.expect("start");
    h.wait_until("pol-fresh", "errored with the budget spent", |s| {
        s.status == AppStatus::Errored && s.restarts >= 3
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(Harness::line_count(&launches), 4);

    assert_eq!(h.restart("pol-fresh").await.expect("operator restart"), 1);

    // A fresh budget means another full round: four more launches, then errored again.
    h.eventually("a second round of four launches", WAIT_CAP, || {
        Harness::line_count(&launches) >= 8
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(Harness::line_count(&launches), 8);
    let state = h.state("pol-fresh").await;
    assert_eq!(state.status, AppStatus::Errored);
    assert_eq!(state.restarts, 3);
}

/// Wall-clock nanoseconds of every launch, one per line.
fn launch_times(path: &std::path::Path) -> Vec<u128> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_backoff_delay_grows() {
    let h = Harness::new().await;
    let launches = h.file("times");
    let spec = h.cmd_with(
        "pol-grow",
        &format!("date +%s%N >> '{}'; exit 1", launches.display()),
        json!({ "restart_policy": { "restart_delay_ms": 100 } }),
    );
    h.start(spec).await.expect("start");
    h.eventually("four launches", WAIT_CAP, || {
        launch_times(&launches).len() >= 4
    })
    .await;
    let t = launch_times(&launches);
    let gap1 = t[1] - t[0];
    let gap3 = t[3] - t[2];
    assert!(
        gap3 > 2 * gap1,
        "delays 100, 200, 400 ms: gap1 {} ms, gap3 {} ms",
        gap1 / 1_000_000,
        gap3 / 1_000_000
    );
    h.stop("pol-grow").await.expect("stop");
}

/// The crash loop message names the exits and the window, and the status carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracker_integration_gives_up_with_crash_loop_message() {
    let h = Harness::new().await;
    let spec = h.cmd_with(
        "pol-msg",
        "exit 1",
        json!({ "restart_policy": { "restart_delay_ms": 20, "max_restarts": 2 } }),
    );
    h.start(spec).await.expect("start");
    let state = h
        .wait_until("pol-msg", "errored by the crash loop", |s| {
            s.status == AppStatus::Errored && s.last_error.is_some()
        })
        .await;
    assert_eq!(
        state.last_error.as_deref(),
        Some("crash loop: 3 exits in 600 s")
    );
    assert_eq!(state.restarts, 2);
    assert_eq!(state.last_exit_code, Some(1));
    assert_eq!(state.backoff_until, None);
}

/// The file the restart wants disappears while the app waits out its delay: the relaunch fails,
/// which is one more attempt with a longer delay, not the end. Putting the file back revives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_during_auto_restart_backs_off_and_retries() {
    let h = Harness::new().await;
    let m = h.marker();
    let path = h.script("flaky.sh", "exit 1");
    let mut spec = h.cmd_with(
        "pol-spawn",
        "",
        json!({ "restart_policy": { "restart_delay_ms": 500, "max_restart_delay_ms": 600 } }),
    );
    spec.command = path.display().to_string();
    spec.args = Vec::new();
    h.start(spec).await.expect("start");

    h.wait_status("pol-spawn", AppStatus::Backoff).await;
    std::fs::remove_file(&path).expect("remove the script");

    let state = h
        .wait_until("pol-spawn", "backoff after a failed relaunch", |s| {
            s.status == AppStatus::Backoff && s.backoff_attempt >= 2
        })
        .await;
    assert!(state.backoff_until.is_some());
    assert!(
        state.last_error.is_some(),
        "the failed launch is explained: {state:?}"
    );

    h.script("flaky.sh", &format!("exec sleep {m}"));
    let state = h.wait_status("pol-spawn", AppStatus::Running).await;
    assert!(state.pid.is_some());
    assert_eq!(
        state.last_error, None,
        "a launch that worked clears the explanation"
    );
    h.stop("pol-spawn").await.expect("stop");
}

/// An operator restart during a backoff gives the app a fresh budget and a fresh delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_restart_during_backoff_resets_the_attempt() {
    let h = Harness::new().await;
    let spec = h.cmd_with(
        "pol-reset",
        "exit 1",
        json!({ "restart_policy": { "restart_delay_ms": 200, "max_restart_delay_ms": 3000 } }),
    );
    h.start(spec).await.expect("start");
    h.wait_until("pol-reset", "the third attempt", |s| {
        s.status == AppStatus::Backoff && s.backoff_attempt >= 3
    })
    .await;

    h.restart("pol-reset").await.expect("operator restart");
    // the restarted process crashes at once; its backoff is the first of a new series
    h.wait_until("pol-reset", "a first attempt again", |s| {
        s.status == AppStatus::Backoff && s.backoff_attempt == 1
    })
    .await;
    h.stop("pol-reset").await.expect("stop");
}

/// A run that outlasts `stable_uptime_secs` starts the delay over, so a crash that happens every
/// 1.2 s never adds up to a crash loop although `max_restarts` is 2 and the window one second.
#[cfg(feature = "slow-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_crashes_never_exhaust_the_budget() {
    let h = Harness::new().await;
    let launches = h.file("launches");
    let spec = h.cmd_with(
        "pol-slow",
        &format!("echo launch >> '{}'; sleep 1.2; exit 1", launches.display()),
        json!({
            "restart_policy": {
                "restart_delay_ms": 500,
                "max_restarts": 2,
                "crash_window_secs": 1,
                "stable_uptime_secs": 1
            }
        }),
    );
    h.start(spec).await.expect("start");
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(
        Harness::line_count(&launches) >= 4,
        "launches: {}",
        Harness::line_count(&launches)
    );
    let state = h.state("pol-slow").await;
    assert_ne!(state.status, AppStatus::Errored, "{state:?}");
    h.stop("pol-slow").await.expect("stop");
}
