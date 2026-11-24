//! Test the adapter connection, supervision and daemon integration using the fixture adapter.

mod common;

use std::time::{Duration, Instant};

use common::fixture::{initialize_params, wait_until, Fixture};
use oxidepm_core::ErrorCode;
use oxidepmd::adapters::conn::{CallError, CloseReason, ConnConfig};
use serde_json::json;

const SECS_10: Duration = Duration::from_secs(10);

/// A short configuration for tests that must not wait real seconds.
fn cfg() -> ConnConfig {
    ConnConfig::protocol1()
}

// ---- 24a: the connection ------------------------------------------------------------------

#[tokio::test]
async fn frame_before_initialize_is_a_strike() {
    // the fixture speaks at once, before the daemon said anything: one valid notification, which
    // would be ignored at any other time
    let mut fx = Fixture::spawn(
        json!([{"on": "start", "do": {"flood_stdout": {"fps": 1, "bytes": 50, "count": 1}}}]),
    );
    let conn = fx.connect(cfg());
    assert!(
        wait_until(10, || conn.counters().strikes == 1).await,
        "{:?}",
        conn.counters()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(conn.counters().strikes, 1, "one frame, one strike");
    assert!(!conn.is_closed(), "one strike does not end the connection");

    // after the daemon has spoken, the adapter may answer
    let reply = conn
        .call(
            "initialize",
            &initialize_params("fx", "1.0.0", "tool"),
            SECS_10,
        )
        .await;
    assert!(reply.is_ok(), "{reply:?}");
}

#[tokio::test]
async fn deadline_is_enforced_by_the_host_not_the_adapter() {
    // the adapter answers after 700 ms; the caller's deadline is 150 ms
    let mut fx = Fixture::spawn(json!([
        {"on": "runtime.prepare", "do": [{"sleep": {"ms": 700}}, {"reply": {}}]}
    ]));
    let conn = fx.connect(cfg());
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .unwrap();

    let started = Instant::now();
    let result = conn
        .call("runtime.prepare", &json!({}), Duration::from_millis(150))
        .await;
    let took = started.elapsed();
    assert_eq!(result, Err(CallError::Timeout));
    assert!(
        took >= Duration::from_millis(140) && took < Duration::from_millis(600),
        "returned after {took:?}"
    );
    assert_eq!(conn.in_flight(), 0, "the entry is gone at once");
    assert_eq!(conn.counters().strikes, 0);
}

#[tokio::test]
async fn late_reply_after_deadline_is_dropped_and_counted() {
    let mut fx = Fixture::spawn(json!([
        {"on": "runtime.prepare", "do": [{"sleep": {"ms": 400}}, {"reply": {}}]}
    ]));
    let conn = fx.connect(cfg());
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .unwrap();

    assert_eq!(
        conn.call("runtime.prepare", &json!({}), Duration::from_millis(100))
            .await,
        Err(CallError::Timeout)
    );
    assert!(
        wait_until(10, || conn.counters().late_dropped == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(
        conn.counters().strikes,
        0,
        "a late reply is not a violation"
    );
    assert!(!conn.is_closed());
    // the connection still works
    assert!(conn.call("ping", &json!({}), SECS_10).await.is_ok());
}

#[tokio::test]
async fn cancelled_reply_after_host_cancel_is_not_a_strike() {
    // a polite adapter answers -32024 once it receives `cancel`
    let mut fx = Fixture::spawn(json!([
        {"on": "runtime.prepare", "do": "reply_cancelled_after_cancel"}
    ]));
    let conn = fx.connect(cfg());
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .unwrap();

    assert_eq!(
        conn.call("runtime.prepare", &json!({}), Duration::from_millis(100))
            .await,
        Err(CallError::Timeout)
    );
    assert!(
        wait_until(10, || conn.counters().late_dropped == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(
        conn.counters().strikes,
        0,
        "the tombstoned id earns no strike"
    );
    assert_eq!(conn.tombstone_count(), 1);
}

#[tokio::test]
async fn crash_mid_call_fails_pending_with_adapter_crashed() {
    let mut fx = Fixture::spawn(json!([{"on": "runtime.prepare", "do": {"crash": {"code": 3}}}]));
    let conn = fx.connect(cfg());
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .unwrap();

    let started = Instant::now();
    let result = conn.call("runtime.prepare", &json!({}), SECS_10).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call did not wait for its deadline"
    );
    let err = match result {
        Err(e @ CallError::Crashed(_)) => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(err.into_error("fx").code(), ErrorCode::AdapterCrashed);
    assert_eq!(conn.closed().await, CloseReason::Eof);
    // a call after the end fails at once, the same way
    assert!(matches!(
        conn.call("ping", &json!({}), SECS_10).await,
        Err(CallError::Crashed(_))
    ));
}

// ---- 24b: supervision ---------------------------------------------------------------------

use std::sync::atomic::{AtomicU32, Ordering};

use common::fixture::{pid_alive, wait_state, Rig};
use oxidepm_adapter_proto::Kind;
use oxidepmd::adapters::entry::{EntryState, FailReason};

static MARKER: AtomicU32 = AtomicU32::new(0);

/// A `sleep` argument no other process on the machine can have.
fn unique_sleep() -> String {
    format!(
        "31.{:05}{:03}",
        std::process::id() % 100_000,
        MARKER.fetch_add(1, Ordering::SeqCst) % 1000
    )
}

/// Kill an orphaned grandchild, but only if it is still the `sleep` this test asked for.
fn reap_grandchild(pid: u32, marker: &str) {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if String::from_utf8_lossy(&cmdline)
        .replace('\0', " ")
        .contains(&format!("sleep {marker}"))
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[tokio::test]
async fn initialize_ok_makes_adapter_ready() {
    let rig = Rig::new("okay", Kind::Service, json!([]), |_| {});
    assert_eq!(rig.entry.state(), EntryState::Stopped);
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await,
        "{:?}",
        rig.entry.view()
    );
    let view = rig.entry.view();
    assert!(view.pid.is_some_and(pid_alive));
    assert_eq!(view.crashes, 0);
    // the adapter was told who it is and what it may do
    let init = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "initialize")
        .expect("initialize was sent");
    assert_eq!(init["frame"]["params"]["adapter"]["name"], "okay");
    assert_eq!(init["frame"]["params"]["grant"]["level"], "read");
    assert_eq!(init["frame"]["params"]["protocols"], json!([1]));
    rig.entry.stop(false).await;
    assert_eq!(rig.entry.state(), EntryState::Stopped);
    assert!(!pid_alive(view.pid.unwrap()));
}

async fn refused(script: serde_json::Value, reason: FailReason) -> Rig {
    let rig = Rig::new("liar", Kind::Service, script, |_| {});
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 15, |s| *s == EntryState::Failed(reason)).await,
        "{:?}",
        rig.entry.view()
    );
    let view = rig.entry.view();
    assert!(view.note.is_some(), "the reason is recorded");
    // killed, and nothing is left in the pid table
    let pid = rig.start_pids()[0];
    assert!(
        wait_until(10, || !pid_alive(pid)).await,
        "the adapter was killed"
    );
    assert!(
        wait_until(10, || std::fs::read_to_string(&rig.pids_path)
            .map(|t| !t.contains("\"liar\""))
            .unwrap_or(true))
        .await
    );
    assert_eq!(rig.starts(), 1, "it is not started again");
    rig
}

#[tokio::test]
async fn reply_with_other_name_is_refused_and_marked_tampered() {
    let rig = refused(
        json!([{"on": "initialize", "do": {"lie_name": {}}}]),
        FailReason::Tampered,
    )
    .await;
    assert!(rig
        .entry
        .view()
        .note
        .unwrap()
        .contains("not the adapter that was imported"));
}

#[tokio::test]
async fn reply_with_other_version_is_refused() {
    refused(
        json!([{"on": "initialize", "do": {"lie_version": {}}}]),
        FailReason::Tampered,
    )
    .await;
}

#[tokio::test]
async fn no_protocol_overlap_marks_incompatible_with_supported_list() {
    // the adapter names a protocol the daemon did not offer
    let rig = refused(
        json!([{"on": "initialize", "do": {"lie_protocol": {"protocol": 2}}}]),
        FailReason::Incompatible,
    )
    .await;
    let note = rig.entry.view().note.unwrap();
    assert!(
        note.contains("protocol 2") && note.contains("[1]"),
        "{note}"
    );

    // the adapter itself says it speaks none of ours (-32022), with what it does speak
    let rig = Rig::new(
        "liar",
        Kind::Service,
        json!([{"on": "initialize", "do": {"error": {"code": -32022, "message": "only protocol 7", "name": "E_ADAPTER_PROTOCOL"}}}]),
        |_| {},
    );
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 15, |s| *s
            == EntryState::Failed(FailReason::Incompatible))
        .await
    );
    let note = rig.entry.view().note.unwrap();
    assert!(
        note.contains("only protocol 7") && note.contains("[1]"),
        "{note}"
    );
}

#[tokio::test]
async fn initialize_timeout_kills_and_backs_off() {
    let rig = Rig::new(
        "slowinit",
        Kind::Service,
        json!([{"on": "initialize", "do": "hang"}]),
        |cfg| {
            cfg.init_timeout = Duration::from_millis(300);
            cfg.restart.restart_delay_ms = 200;
        },
    );
    rig.entry.start();
    assert!(wait_until(15, || rig.starts() >= 1).await);
    let first = rig.start_pids()[0];
    // killed at the timeout
    assert!(
        wait_until(10, || !pid_alive(first)).await,
        "the hung adapter was killed"
    );
    assert!(
        wait_state(&rig.entry, 10, |s| matches!(s, EntryState::Backoff { .. })).await,
        "{:?}",
        rig.entry.view()
    );
    assert_eq!(rig.entry.view().crashes, 1);
    // and started again after the delay
    assert!(
        wait_until(15, || rig.starts() >= 2).await,
        "restarted after the delay"
    );
    assert_ne!(rig.start_pids()[1], first);
    rig.entry.stop(true).await;
    assert_eq!(rig.entry.state(), EntryState::Disabled);
}

#[tokio::test]
async fn adapter_is_group_leader_with_piped_stdin() {
    let rig = Rig::new("grouped", Kind::Service, json!([]), |_| {});
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    let pid = rig.entry.view().pid.unwrap();
    let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid as i32))).unwrap();
    assert_eq!(
        pgid.as_raw() as u32,
        pid,
        "the adapter leads its own process group"
    );
    for fd in 0..=2 {
        let target = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .unwrap()
            .display()
            .to_string();
        assert!(target.starts_with("pipe:"), "fd {fd} is {target}");
    }
    // the process that runs is the file that was hashed
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert_eq!(exe, common::fixture::fixture_bin().canonicalize().unwrap());
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn two_expiries_open_the_circuit_and_later_calls_fail_at_once() {
    let rig = Rig::new(
        "hungrt",
        Kind::Tool,
        json!([{"on": "runtime.prepare", "times": 2, "do": "hang"}]),
        |cfg| {
            cfg.breaker_open = Duration::from_millis(700);
        },
    );
    for _ in 0..2 {
        let err = rig
            .entry
            .call("runtime.prepare", &json!({}), Duration::from_millis(250))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AdapterTimeout);
    }
    assert_eq!(rig.entry.state(), EntryState::Degraded);
    let started = Instant::now();
    let err = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(10))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::AdapterUnavailable);
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "failed at once: {:?}",
        started.elapsed()
    );

    // other calls are not held back by the breaker
    assert!(rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());

    // after the open period one probe goes through; it succeeds, so the circuit closes
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert_eq!(rig.entry.state(), EntryState::Ready);
    assert!(rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert_eq!(
        rig.starts(),
        1,
        "a timeout is not a crash: the adapter was never restarted"
    );
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn failed_tool_is_retried_once_after_five_minutes() {
    // "five minutes", injected as 900 ms
    let rig = Rig::new(
        "flaky",
        Kind::Tool,
        json!([{"on": "initialize", "do": {"crash": {"code": 1}}}]),
        |cfg| {
            cfg.restart.max_restarts = 1;
            cfg.restart.restart_delay_ms = 10;
            cfg.failed_tool_retry = Duration::from_millis(900);
        },
    );
    // the first call gets the adapter started; it crashes twice and the tracker gives up
    let err = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::AdapterUnavailable);
    assert!(
        wait_state(&rig.entry, 10, |s| *s
            == EntryState::Failed(FailReason::CrashLoop))
        .await,
        "{:?}",
        rig.entry.view()
    );
    let spawned = rig.starts();
    assert_eq!(spawned, 2);

    // a failed tool answers at once and starts nothing
    let started = Instant::now();
    let err = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::AdapterUnavailable);
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(rig.starts(), spawned);

    // after the delay the next call is the one retry; it is still crashing, so it fails again
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let err = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::AdapterUnavailable);
    assert_eq!(rig.starts(), spawned + 1, "exactly one more attempt");
    assert!(
        wait_state(&rig.entry, 10, |s| *s
            == EntryState::Failed(FailReason::CrashLoop))
        .await
    );

    // and there is no second retry, however long it waits
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let _ = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(rig.starts(), spawned + 1);

    // a tool whose trouble has passed does come back with its one retry
    let rig = Rig::new(
        "flaky2",
        Kind::Tool,
        json!([{"on": "initialize", "do": {"crash": {"code": 1}}}]),
        |cfg| {
            cfg.restart.max_restarts = 1;
            cfg.restart.restart_delay_ms = 10;
            cfg.failed_tool_retry = Duration::from_millis(900);
        },
    );
    let _ = rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        wait_state(&rig.entry, 10, |s| *s
            == EntryState::Failed(FailReason::CrashLoop))
        .await
    );
    rig.set_script(json!([]));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(rig
        .entry
        .call("runtime.prepare", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert_eq!(rig.entry.state(), EntryState::Ready);
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn call_during_idle_shutdown_respawns() {
    // the adapter takes 800 ms to answer `shutdown`, so the idle exit lasts a while
    let rig = Rig::new(
        "idler",
        Kind::Tool,
        json!([{"on": "shutdown", "do": [{"sleep": {"ms": 800}}, "reply"]}]),
        |cfg| {
            cfg.tool_idle = Duration::from_millis(250);
            cfg.shutdown.stdin_phase = Duration::from_secs(3);
            cfg.shutdown.term_phase = Duration::from_secs(3);
        },
    );
    assert!(rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert!(
        wait_state(&rig.entry, 10, |s| *s == EntryState::Stopping).await,
        "{:?}",
        rig.entry.view()
    );

    // a call now waits for the exit and gets a fresh process: no crash, no error
    let result = rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(rig.starts(), 2);
    assert_eq!(rig.entry.view().crashes, 0, "an idle exit is not a crash");
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn tool_exits_after_idle_without_counting_a_crash() {
    let rig = Rig::new("tooly", Kind::Tool, json!([]), |cfg| {
        cfg.tool_idle = Duration::from_millis(200)
    });
    assert!(rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert_eq!(rig.entry.state(), EntryState::Ready);
    assert!(
        wait_state(&rig.entry, 10, |s| *s == EntryState::Stopped).await,
        "{:?}",
        rig.entry.view()
    );
    let view = rig.entry.view();
    assert_eq!(view.crashes, 0);
    assert!(view.pid.is_none());
    assert!(!pid_alive(rig.start_pids()[0]));
    // and it comes back with the next call
    assert!(rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await
        .is_ok());
    assert_eq!(rig.starts(), 2);
    assert_eq!(rig.entry.view().crashes, 0);
}

#[tokio::test]
async fn kill_walk_reaches_a_setsid_grandchild() {
    let marker = unique_sleep();
    let rig = Rig::new(
        "walker",
        Kind::Service,
        json!([{"on": "initialize", "do": ["reply", {"spawn_setsid_grandchild": {"cmd": format!("sleep {marker}")}}]}]),
        |_| {},
    );
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    let record = loop {
        if let Some(r) = rig.report().into_iter().find(|r| r["what"] == "grandchild") {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let grandchild = record["child_pid"].as_u64().expect("grandchild pid") as u32;
    assert_ne!(
        record["pgid"], record["own_pgid"],
        "it left the process group"
    );
    assert!(pid_alive(grandchild));

    rig.entry.kill_now().await;
    let dead = wait_until(10, || !pid_alive(grandchild)).await;
    reap_grandchild(grandchild, &marker);
    assert!(dead, "the walk found the helper that left the group");
    assert!(!pid_alive(rig.start_pids()[0]));
}

#[tokio::test]
async fn shutdown_ignored_is_followed_by_sigterm_then_sigkill_of_the_group() {
    let marker = unique_sleep();
    let rig = Rig::new(
        "stubborn",
        Kind::Service,
        json!([
            {"on": "initialize", "do": ["reply", "ignore_shutdown", "ignore_sigterm", {"spawn_grandchild": {"cmd": format!("sleep {marker}")}}]},
            {"on": "shutdown", "do": "hang"}
        ]),
        |_| {},
    );
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    let grandchild = loop {
        if let Some(r) = rig.report().into_iter().find(|r| r["what"] == "grandchild") {
            break r["child_pid"].as_u64().unwrap() as u32;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let leader = rig.start_pids()[0];

    let started = Instant::now();
    rig.entry.stop(false).await;
    let took = started.elapsed();
    let gone = !pid_alive(leader) && wait_until(5, || !pid_alive(grandchild)).await;
    reap_grandchild(grandchild, &marker);
    assert!(gone, "the leader and the grandchild in its group are dead");
    // 400 ms for the polite phase, 400 ms for SIGTERM, then the kill: never longer than the sum
    assert!(
        took >= Duration::from_millis(750),
        "the adapter was given its grace: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(rig.entry.state(), EntryState::Stopped);
    assert_eq!(
        rig.entry.view().crashes,
        0,
        "a requested stop is not a crash"
    );
}

#[tokio::test]
async fn adapter_record_does_not_replace_app_record_with_same_number() {
    use oxidepmd::supervisor::{PidRecord, PidTable};
    let rig = Rig::new("pidded", Kind::Service, json!([]), |_| {});
    // an app with the same number as the adapter's slot (1), in the table for apps
    let apps_path = rig.dir.path().join("pids.json");
    let apps = PidTable::new(apps_path.clone());
    let _app = apps.add(PidRecord {
        app_id: 1,
        name: "an-app".into(),
        pid: 4_000_000,
        pgid: 4_000_000,
        start_ticks: 1,
    });

    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);

    let read = |p: &std::path::Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    };
    let app_file = read(&apps_path);
    let adapter_file = read(&rig.pids_path);
    assert_eq!(app_file["apps"][0]["name"], "an-app");
    assert_eq!(adapter_file["apps"][0]["name"], "pidded");
    assert_eq!(adapter_file["apps"][0]["app_id"], 1);
    assert_eq!(
        adapter_file["apps"][0]["pid"].as_u64(),
        rig.entry.view().pid.map(u64::from)
    );

    // stopping the adapter takes its record away and leaves the app's alone
    rig.entry.stop(false).await;
    assert!(read(&rig.pids_path)["apps"].as_array().unwrap().is_empty());
    assert_eq!(read(&apps_path)["apps"][0]["name"], "an-app");
}

#[tokio::test]
async fn ping_misses_kill_a_service() {
    let rig = Rig::new(
        "deafping",
        Kind::Service,
        json!([{"on": "ping", "do": "hang"}]),
        |cfg| {
            cfg.ping_interval = Duration::from_millis(100);
            cfg.ping_timeout = Duration::from_millis(100);
            cfg.restart.restart_delay_ms = 5_000;
        },
    );
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    let pid = rig.entry.view().pid.unwrap();
    // three unanswered pings in a row, then it is killed and counted as a crash
    assert!(
        wait_until(15, || !pid_alive(pid)).await,
        "the service that ignores pings was killed"
    );
    assert!(
        wait_state(&rig.entry, 10, |s| matches!(s, EntryState::Backoff { .. })).await,
        "{:?}",
        rig.entry.view()
    );
    assert_eq!(rig.entry.view().crashes, 1);
    let pings = rig
        .report()
        .iter()
        .filter(|r| r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "ping")
        .count();
    assert!(pings >= 3, "{pings} pings were sent");
    rig.entry.stop(true).await;
}

#[tokio::test]
async fn crash_restarts_with_growing_delay() {
    // the service dies 150 ms after it starts; the delay before each restart doubles from 300 ms
    let rig = Rig::new(
        "dier",
        Kind::Service,
        json!([{"on": "after_ms:150", "do": {"crash": {"code": 1}}}]),
        |cfg| {
            cfg.restart.restart_delay_ms = 300;
            cfg.restart.max_restarts = 20;
        },
    );
    rig.entry.start();

    // Follow the state: the delay each backoff announces, and how long the adapter really stayed
    // down. The announced delays are the policy's; the wait must be at least the announced delay
    // (a loaded machine may add to it, never take away).
    let mut rx = rig.entry.subscribe();
    let mut backoffs: Vec<(u64, Instant)> = Vec::new();
    let mut waited: Vec<Duration> = Vec::new();
    let mut since: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while waited.len() < 3 && Instant::now() < deadline {
        let state = *rx.borrow_and_update();
        match state {
            EntryState::Backoff { delay_ms, .. } => {
                if since.is_none() {
                    since = Some(Instant::now());
                    backoffs.push((delay_ms, Instant::now()));
                }
            }
            EntryState::Starting => {
                if let Some(at) = since.take() {
                    waited.push(at.elapsed());
                }
            }
            _ => {}
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
    }
    assert_eq!(waited.len(), 3, "three restarts were seen: {backoffs:?}");
    let delays: Vec<u64> = backoffs.iter().map(|(d, _)| *d).collect();
    assert_eq!(&delays[..3], &[300, 600, 1200], "the delay doubles");
    for (w, (d, _)) in waited.iter().zip(&backoffs) {
        assert!(
            *w + Duration::from_millis(30) >= Duration::from_millis(*d),
            "restarted after {w:?}, announced {d} ms"
        );
    }
    rig.entry.stop(true).await;
}

#[tokio::test]
async fn crash_loop_marks_failed_and_stops_restarting() {
    let rig = Rig::new(
        "looper",
        Kind::Service,
        json!([{"on": "after_ms:100", "do": {"crash": {"code": 1}}}]),
        |cfg| {
            cfg.restart.restart_delay_ms = 50;
            cfg.restart.max_restarts = 2;
        },
    );
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 30, |s| *s
            == EntryState::Failed(FailReason::CrashLoop))
        .await,
        "{:?}",
        rig.entry.view()
    );
    let starts = rig.starts();
    assert_eq!(starts, 3, "the first run and two restarts");
    // calls fail fast
    let started = Instant::now();
    let err = rig
        .entry
        .call("ping", &json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::AdapterUnavailable);
    assert!(started.elapsed() < Duration::from_millis(50));
    // and nothing is started again
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.starts(), starts);
}

#[tokio::test]
async fn enable_after_failed_resets_the_tracker() {
    let rig = Rig::new(
        "looper",
        Kind::Service,
        json!([{"on": "after_ms:100", "do": {"crash": {"code": 1}}}]),
        |cfg| {
            cfg.restart.restart_delay_ms = 50;
            cfg.restart.max_restarts = 2;
        },
    );
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 30, |s| *s
            == EntryState::Failed(FailReason::CrashLoop))
        .await,
        "{:?}",
        rig.entry.view()
    );
    let starts = rig.starts();
    assert_eq!(starts, 3, "the first run and two restarts");

    // the operator fixes the adapter and enables it again: the crash history starts afresh
    rig.set_script(json!([]));
    rig.entry.enable();
    assert_eq!(rig.entry.state(), EntryState::Stopped);
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await,
        "{:?}",
        rig.entry.view()
    );
    assert_eq!(rig.starts(), starts + 1, "it starts again");
    assert_eq!(
        rig.entry.view().crashes,
        starts as u32,
        "the crashes stay counted for the operator"
    );
    // and it is not held to the old history: a new crash is restarted, not failed at once
    rig.set_script(json!([{"on": "after_ms:100", "do": {"crash": {"code": 1}}}]));
    rig.entry.stop(false).await;
    rig.entry.start();
    assert!(
        wait_until(15, || rig.starts() >= starts + 2).await,
        "{}",
        rig.starts()
    );
    assert_ne!(
        rig.entry.state(),
        EntryState::Failed(FailReason::CrashLoop),
        "one crash is not a loop"
    );
    rig.entry.stop(true).await;
}

// ---- 24c: the daemon -----------------------------------------------------------------------

use common::daemon::{DaemonProc, Signal};
use common::fixture::{adapter_states, install, wait_adapter_state, InstallSpec};
use tempfile::TempDir;

fn spawn_daemon(home: &std::path::Path, installed: &[&common::fixture::Installed]) -> DaemonProc {
    let env: Vec<(String, String)> = installed.iter().flat_map(|i| i.env()).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    DaemonProc::spawn_with_env(home, &env)
}

fn sleeper(workdir: &std::path::Path, name: &str, secs: &str) -> serde_json::Value {
    let mut spec = oxidepm_core::AppSpec::new(
        name.to_string(),
        oxidepm_core::Runtime::CMD,
        "sleep".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec![secs.to_string()];
    json!({ "type": "start", "spec": spec })
}

fn app_pid(daemon: &DaemonProc, name: &str) -> Option<u64> {
    let status = daemon.rpc(json!({ "type": "status" }));
    status["apps"]
        .as_array()?
        .iter()
        .find(|a| a["spec"]["name"] == name)
        .and_then(|a| a["state"]["pid"].as_u64())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_without_adapters_creates_no_adapter_files() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);
    assert!(adapter_states(&daemon).is_empty());
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    for name in ["adapters.db", "adapters", "adapter-pids.json"] {
        assert!(
            !home.join(name).exists(),
            "{name} was created for a home with no adapters"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_adapter_starts_in_the_background_and_stops_with_the_daemon() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fx = install(&home, InstallSpec::service("svc", json!([]))).await;
    let mut daemon = spawn_daemon(&home, &[&fx]);

    assert!(
        wait_adapter_state(&daemon, "svc", "ready", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    let pid = fx.start_pids()[0];
    assert!(pid_alive(pid));
    // it is recorded where a later daemon can find it, apart from the apps
    let recorded = std::fs::read_to_string(home.join("adapter-pids.json")).unwrap();
    assert!(
        recorded.contains("\"svc\"") && recorded.contains(&pid.to_string()),
        "{recorded}"
    );

    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    assert!(
        wait_until(10, || !pid_alive(pid)).await,
        "the service ended with the daemon"
    );
    let after = std::fs::read_to_string(home.join("adapter-pids.json")).unwrap();
    assert!(!after.contains("\"svc\""), "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_boot_does_not_wait_for_a_slow_adapter() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    // the adapter takes 15 s to answer initialize
    let fx = install(
        &home,
        InstallSpec::service(
            "slow",
            json!([{"on": "initialize", "do": [{"sleep": {"ms": 15000}}, "reply"]}]),
        ),
    )
    .await;
    let started = Instant::now();
    let mut daemon = spawn_daemon(&home, &[&fx]);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the socket answered: {:?}",
        started.elapsed()
    );
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");

    // the adapter is still starting while the daemon serves
    assert!(
        wait_adapter_state(&daemon, "slow", "starting", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    assert!(fx.wait_report("start").await.is_some());

    // and a stop does not wait for a handshake that never finishes
    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    assert!(
        signalled.elapsed() < Duration::from_secs(10),
        "{:?}",
        signalled.elapsed()
    );
    assert!(wait_until(10, || !pid_alive(fx.start_pids()[0])).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_apps_are_unaffected_by_an_adapter_crash() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    // a service that dies 100 ms after every start
    let fx = install(
        &home,
        InstallSpec::service(
            "dies",
            json!([{"on": "after_ms:100", "do": {"crash": {"code": 1}}}]),
        ),
    )
    .await;
    let mut daemon = spawn_daemon(&home, &[&fx]);

    let marker = format!("303.{:05}1", std::process::id() % 100_000);
    let started = daemon.rpc(sleeper(&workdir, "steady", &marker));
    assert_eq!(started["type"], "started", "{started}");
    let pid = app_pid(&daemon, "steady").expect("the app runs");

    // the adapter crashes and restarts, more than once
    assert!(
        wait_until(30, || fx.start_pids().len() >= 3).await,
        "{:?}",
        fx.start_pids()
    );
    assert_eq!(
        app_pid(&daemon, "steady"),
        Some(pid),
        "the app was not touched"
    );
    assert!(pid_alive(pid as u32));
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");

    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_gives_services_a_grace_then_kills_groups() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let marker = unique_sleep();
    // it ignores `shutdown` and its stdin closing, and has a grandchild in its group
    let fx = install(
        &home,
        InstallSpec::service(
            "stubborn",
            json!([
                {"on": "initialize", "do": ["reply", "ignore_shutdown", {"spawn_grandchild": {"cmd": format!("sleep {marker}")}}]},
                {"on": "shutdown", "do": "hang"}
            ]),
        ),
    )
    .await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    assert!(wait_adapter_state(&daemon, "stubborn", "ready", 20).await);
    let record = fx.wait_report("grandchild").await.expect("grandchild");
    let grandchild = record["child_pid"].as_u64().unwrap() as u32;
    let leader = fx.start_pids()[0];

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    let took = signalled.elapsed();
    let gone = wait_until(10, || !pid_alive(leader) && !pid_alive(grandchild)).await;
    reap_grandchild(grandchild, &marker);
    assert!(gone, "the adapter and its group are dead");
    // 2 s of grace for the polite request and stdin, then SIGTERM; well inside the bound
    assert!(
        took >= Duration::from_millis(1500),
        "the service was given its grace: {took:?}"
    );
    assert!(took < Duration::from_secs(15), "{took:?}");
}

/// The adapter outlives the daemon's death by SIGKILL (it ignores the closed stdin); the next
/// daemon finds it in adapter-pids.json and kills its group before it starts anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_pids_are_swept_after_a_sigkilled_daemon() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fx = install(
        &home,
        InstallSpec::service(
            "survivor",
            json!([{"on": "initialize", "do": ["reply", "ignore_shutdown"]}]),
        ),
    )
    .await;
    let mut first = spawn_daemon(&home, &[&fx]);
    assert!(wait_adapter_state(&first, "survivor", "ready", 20).await);
    let orphan = fx.start_pids()[0];

    first.sigkill();
    let _ = first.wait_exit();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        pid_alive(orphan),
        "the adapter ignores the end of its stdin, so it outlived the daemon"
    );

    let mut second = spawn_daemon(&home, &[&fx]);
    assert!(
        wait_until(15, || !pid_alive(orphan)).await,
        "the next daemon swept the orphaned adapter"
    );
    assert!(wait_adapter_state(&second, "survivor", "ready", 20).await);
    second.signal(Signal::SIGTERM);
    assert!(second.wait_exit().success());
    // nothing of either is left
    assert!(wait_until(10, || fx.start_pids().iter().all(|p| !pid_alive(*p))).await);
}

/// The daemon loads the adapter table before resurrecting apps at boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_resurrect_of_runtime_app_finds_the_adapter_table() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let fx = install(&home, InstallSpec::tool("rt", json!([]))).await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    let marker = format!("304.{:05}1", std::process::id() % 100_000);
    assert_eq!(
        daemon.rpc(sleeper(&workdir, "comes-back", &marker))["type"],
        "started"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());

    let mut daemon = spawn_daemon(&home, &[&fx]);
    // the first answer already lists the adapter (a tool, so it is stopped, not failed)
    let states = adapter_states(&daemon);
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(
        (states[0].0.as_str(), states[0].1.as_str()),
        ("rt", "stopped"),
        "{states:?}"
    );
    assert!(
        wait_until(20, || app_pid(&daemon, "comes-back").is_some()).await,
        "the saved app came back"
    );
    let status = daemon.rpc(json!({ "type": "status" }));
    let app = status["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["spec"]["name"] == "comes-back")
        .unwrap();
    assert_eq!(app["state"]["status"], "running", "{app}");
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_boots_when_adapters_dir_is_a_file() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("adapters"), b"in the way").unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    let status = daemon.rpc(json!({ "type": "daemon_status" }));
    let warnings = status["warnings"].to_string();
    assert!(
        warnings.contains("adapters") && warnings.contains("not a directory"),
        "{status}"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_executable_is_not_run_and_is_recorded() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::service("changed", json!([]));
    spec.wrong_digest = true;
    let fx = install(&home, spec).await;
    let mut daemon = spawn_daemon(&home, &[&fx]);

    assert!(
        wait_adapter_state(&daemon, "changed", "failed", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    let note = adapter_states(&daemon)[0].2.clone().unwrap();
    assert!(
        note.starts_with("tampered:") && note.contains("digest"),
        "{note}"
    );
    assert!(fx.start_pids().is_empty(), "nothing was executed");

    // the reason is in the row and in the audit file
    let paths = oxidepm_core::Paths::new(home.clone());
    assert!(
        wait_until(10, || {
            std::fs::read_to_string(paths.audit_log())
                .map(|t| t.contains("\"event\":\"tampered\"") && t.contains("\"changed\""))
                .unwrap_or(false)
        })
        .await,
        "the audit file says so"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    let store = oxidepmd::adapters::store::AdapterStore::open_existing(&paths)
        .await
        .unwrap()
        .unwrap();
    let row = store.repo().get("changed").await.unwrap().unwrap();
    assert!(row.state_note.unwrap_or_default().starts_with("tampered:"));
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_files_show_needs_reinstall() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::tool("gone", json!([]));
    spec.no_files = true;
    let fx = install(&home, spec).await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    let states = adapter_states(&daemon);
    assert_eq!(
        (states[0].0.as_str(), states[0].1.as_str()),
        ("gone", "failed"),
        "{states:?}"
    );
    assert!(
        states[0]
            .2
            .as_deref()
            .unwrap_or("")
            .starts_with("needs_reinstall:"),
        "{states:?}"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_adapter_is_listed_and_never_started() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::service("off", json!([]));
    spec.enabled = false;
    let fx = install(&home, spec).await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    assert_eq!(adapter_states(&daemon)[0].1, "disabled");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(fx.start_pids().is_empty());
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

/// A deaf service that also ignores SIGTERM: the shutdown is bounded by the budget plus the
/// fixed shutdown allowances (hook drain 10 s, adapters 5 s, audit 1 s, overhead 5 s).
#[cfg(feature = "slow-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_with_deaf_service_finishes_within_budget_plus_bound() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("daemon.toml"), "shutdown_budget_secs = 10\n").unwrap();
    let fx = install(
        &home,
        InstallSpec::service(
            "deaf",
            json!([{"on": "initialize", "do": ["reply", "ignore_sigterm", "deaf"]}]),
        ),
    )
    .await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    assert!(wait_adapter_state(&daemon, "deaf", "ready", 20).await);
    let pid = fx.start_pids()[0];

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit_for(Duration::from_secs(60));
    assert!(status.success(), "{status}");
    let took = signalled.elapsed();
    assert!(took < Duration::from_secs(10 + 10 + 5 + 1 + 5), "{took:?}");
    assert!(
        took >= Duration::from_secs(4),
        "the service was given its grace first: {took:?}"
    );
    assert!(wait_until(5, || !pid_alive(pid)).await, "killed");
}

/// A second SIGTERM during the shutdown kills the adapters with the apps, without their grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_signal_kills_adapters_at_once() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let marker = unique_sleep();
    // ignores shutdown and SIGTERM, with a grandchild in its group: left alone it takes the
    // whole 5 s grace to be killed
    let fx = install(
        &home,
        InstallSpec::service(
            "holdout",
            json!([
                {"on": "initialize", "do": ["reply", "ignore_shutdown", "ignore_sigterm", {"spawn_grandchild": {"cmd": format!("sleep {marker}")}}]},
                {"on": "shutdown", "do": "hang"}
            ]),
        ),
    )
    .await;
    let mut daemon = spawn_daemon(&home, &[&fx]);
    assert!(wait_adapter_state(&daemon, "holdout", "ready", 20).await);
    let grandchild = fx.wait_report("grandchild").await.expect("grandchild")["child_pid"]
        .as_u64()
        .unwrap() as u32;
    let leader = fx.start_pids()[0];

    daemon.signal(Signal::SIGTERM);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        pid_alive(leader),
        "the first signal is a polite one: the adapter is still being asked"
    );
    let second = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert_eq!(status.code(), Some(130), "{status}");
    let gone = wait_until(5, || !pid_alive(leader) && !pid_alive(grandchild)).await;
    reap_grandchild(grandchild, &marker);
    assert!(gone, "the adapter and its group were killed");
    assert!(
        second.elapsed() < Duration::from_secs(4),
        "{:?}",
        second.elapsed()
    );
}
