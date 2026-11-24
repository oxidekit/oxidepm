//! What a flooding, lying or deaf adapter cannot do to the daemon: the
//! connection's limits, strikes and bounds, driven by the fixture adapter over a pipe.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::fixture::{initialize_params, wait_until, Fixture};
use oxidepm_adapter_proto::{ErrorObject, Params};
use oxidepmd::adapters::conn::{
    AdapterConn, CallError, CloseReason, ConnConfig, Inbound, NoHostApi,
};
use serde_json::{json, Value};

const SECS_10: Duration = Duration::from_secs(10);

// The largest single allocation made on the armed thread: the proof that a 100 MiB line is never
// held. Only the thread that armed it is counted, so tests running beside it cannot disturb it
// (the test below runs its whole connection on that one thread).
struct Counting;
static LARGEST: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    if ARMED.try_with(|armed| armed.get()).unwrap_or(false) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

async fn ready(script: Value, cfg: ConnConfig) -> (Fixture, Arc<AdapterConn>) {
    let mut fx = Fixture::spawn(script);
    let conn = fx.connect(cfg);
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .expect("initialize");
    (fx, conn)
}

#[tokio::test]
async fn stdout_flood_is_throttled_and_then_killed() {
    // valid frames as fast as the pipe takes them, after the handshake. The throttle strike is
    // shortened so that the test does not wait the production 10 s per strike.
    let mut cfg = ConnConfig::protocol1();
    cfg.throttle_strike = Duration::from_millis(250);
    let (_fx, conn) = ready(
        json!([{"on": "initialize", "do": ["reply", {"flood_stdout": {"fps": 0, "bytes": 100, "count": 400000}}]}]),
        cfg,
    )
    .await;

    let reason = tokio::time::timeout(Duration::from_secs(30), conn.closed())
        .await
        .expect("the flood ends the connection");
    assert_eq!(reason, CloseReason::Strikes);
    let c = conn.counters();
    assert!(c.throttle_strikes >= 3, "{c:?}");
    // 500 per second sustained plus a burst of 1000, for about a second: far below what was sent
    assert!(c.frames_in < 6000, "the reader was held back: {c:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn giant_line_is_a_strike_not_memory_growth() {
    let limit = ConnConfig::protocol1().limits.max_line;
    let (_fx, conn) = ready(
        json!([{"on": "initialize", "do": ["reply", {"giant_line": {"bytes": 100u64 * 1024 * 1024}}]}]),
        ConnConfig::protocol1(),
    )
    .await;
    LARGEST.store(0, Ordering::SeqCst);
    ARMED.with(|armed| armed.set(true));
    assert!(
        wait_until(60, || conn.counters().overflows == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(conn.closed().await, CloseReason::Desynchronized);
    ARMED.with(|armed| armed.set(false));

    let c = conn.counters();
    assert_eq!(c.strikes, 1);
    assert!(
        c.reader_peak_capacity <= limit,
        "the reader never held more than the limit: {c:?}"
    );
    let largest = LARGEST.load(Ordering::SeqCst);
    assert!(
        largest <= 2 * limit,
        "one allocation of {largest} bytes while a 100 MiB line passed"
    );
}

#[tokio::test]
async fn an_oversize_line_is_counted_before_its_delimiter() {
    use tokio::io::AsyncWriteExt;
    let mut cfg = ConnConfig::protocol1();
    cfg.limits.max_line = 1024;
    let (mut input, adapter_output) = tokio::io::duplex(4096);
    let conn = AdapterConn::start(adapter_output, tokio::io::sink(), cfg, Arc::new(NoHostApi));
    input.write_all(&[b' '; 1025]).await.unwrap();
    assert!(
        wait_until(2, || conn.counters().overflows == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(conn.counters().strikes, 1);
}

#[tokio::test]
async fn invoke_reports_a_full_progress_queue_without_closing_the_connection() {
    use oxidepm_adapter_proto::messages::method;
    let (_fixture, conn) = ready(
        json!([{"on": "command.invoke", "do": [{"invoke_output": {"count": 40, "text": "working"}}, "reply"]}]),
        ConnConfig::protocol1(),
    ).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(oxidepmd::adapters::conn::RELAY_QUEUE_FRAMES);
    let result = conn
        .call_with_progress(method::COMMAND_INVOKE, &json!({}), SECS_10, tx)
        .await;
    let mut messages = 0;
    while rx.try_recv().is_ok() {
        messages += 1;
    }
    assert!(messages <= 32, "queued {messages} progress frames");
    assert_eq!(result, Err(CallError::OutputBackpressure));
    assert!(!conn.is_closed());
}

#[tokio::test]
async fn invalid_utf8_nul_and_non_object_lines_are_strikes() {
    // one fixture per kind of bad line: each is exactly one strike and the connection lives
    for action in [
        json!("invalid_utf8"),
        json!("nul"),
        json!({"garbage": {"text": "42"}}),
        json!({"garbage": {"text": "{ not json"}}),
    ] {
        let (_fx, conn) = ready(
            json!([{"on": "ping", "do": [action.clone(), "reply"]}]),
            ConnConfig::protocol1(),
        )
        .await;
        conn.call("ping", &json!({}), SECS_10)
            .await
            .expect("the ping is answered after the junk");
        assert!(
            wait_until(10, || conn.counters().strikes == 1).await,
            "{action}: {:?}",
            conn.counters()
        );
        assert_eq!(conn.counters().violations, 1, "{action}");
        assert!(!conn.is_closed(), "{action}");
    }
}

#[tokio::test]
async fn array_frame_is_rejected() {
    let (_fx, conn) = ready(
        json!([{"on": "ping", "do": ["array_frame", "reply"]}]),
        ConnConfig::protocol1(),
    )
    .await;
    conn.call("ping", &json!({}), SECS_10)
        .await
        .expect("answered");
    assert!(
        wait_until(10, || conn.counters().violations == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(conn.counters().strikes, 1);
}

#[tokio::test]
async fn three_strikes_in_60s_kill() {
    // three junk lines while a call is pending: the connection ends and the call is a crash
    let (_fx, conn) = ready(
        json!([{"on": "runtime.prepare", "do": [{"garbage": {"text": "a"}}, {"garbage": {"text": "b"}}, {"garbage": {"text": "c"}}]}]),
        ConnConfig::protocol1(),
    )
    .await;
    let result = conn.call("runtime.prepare", &json!({}), SECS_10).await;
    assert!(matches!(result, Err(CallError::Crashed(_))), "{result:?}");
    assert_eq!(conn.closed().await, CloseReason::Strikes);
    assert_eq!(conn.counters().strikes, 3);
}

#[tokio::test]
async fn reply_with_never_issued_id_is_ignored_and_a_strike() {
    let (_fx, conn) = ready(
        json!([{"on": "runtime.prepare", "do": "wrong_id"}]),
        ConnConfig::protocol1(),
    )
    .await;
    let result = conn
        .call("runtime.prepare", &json!({}), Duration::from_millis(400))
        .await;
    assert_eq!(result, Err(CallError::Timeout), "the lie answered nothing");
    assert!(
        wait_until(10, || conn.counters().strikes == 1).await,
        "{:?}",
        conn.counters()
    );
    assert_eq!(conn.in_flight(), 0);
}

#[tokio::test]
async fn duplicate_reply_is_ignored() {
    let (_fx, conn) = ready(
        json!([{"on": "runtime.prepare", "do": "duplicate_reply"}]),
        ConnConfig::protocol1(),
    )
    .await;
    let result = conn.call("runtime.prepare", &json!({}), SECS_10).await;
    assert!(result.is_ok(), "the first reply wins: {result:?}");
    // the second one answers an id that is no longer tracked
    assert!(
        wait_until(10, || conn.counters().strikes == 1).await,
        "{:?}",
        conn.counters()
    );
    assert!(!conn.is_closed());
}

#[tokio::test]
async fn unsolicited_response_is_a_strike() {
    let (_fx, conn) = ready(
        json!([{"on": "ping", "do": [{"unsolicited_response": {"id": 5000}}, "reply"]}]),
        ConnConfig::protocol1(),
    )
    .await;
    conn.call("ping", &json!({}), SECS_10)
        .await
        .expect("answered");
    assert!(
        wait_until(10, || conn.counters().strikes == 1).await,
        "{:?}",
        conn.counters()
    );
}

#[tokio::test]
async fn adapter_that_stops_reading_stdin_is_killed_by_the_write_timeout() {
    let mut cfg = ConnConfig::protocol1();
    cfg.write_timeout = Duration::from_millis(400);
    let (_fx, conn) = ready(json!([{"on": "initialize", "do": ["reply", "deaf"]}]), cfg).await;

    // enough data to fill the pipe (64 KiB) a few times over; no call ever waits for the pipe
    let big = json!({"pad": "x".repeat(200_000)});
    let started = Instant::now();
    for _ in 0..6 {
        let _ = conn.notify("event", &big);
    }
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "senders never wait for the pipe: {:?}",
        started.elapsed()
    );
    let reason = tokio::time::timeout(Duration::from_secs(20), conn.closed())
        .await
        .expect("the write timeout ends it");
    assert_eq!(reason, CloseReason::WriteStalled);
}

#[tokio::test]
async fn in_flight_limit_fails_fast_with_busy() {
    let (_fx, conn) = ready(
        json!([{"on": "runtime.prepare", "do": "hang"}]),
        ConnConfig::protocol1(),
    )
    .await;
    let mut held = Vec::new();
    for _ in 0..32 {
        let conn = Arc::clone(&conn);
        held.push(tokio::spawn(async move {
            conn.call("runtime.prepare", &json!({}), Duration::from_secs(60))
                .await
        }));
    }
    assert!(
        wait_until(10, || conn.in_flight() == 32).await,
        "{}",
        conn.in_flight()
    );

    let started = Instant::now();
    let result = conn.call("runtime.prepare", &json!({}), SECS_10).await;
    assert_eq!(result, Err(CallError::Busy));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the 33rd call returned at once: {:?}",
        started.elapsed()
    );
    assert_eq!(conn.counters().strikes, 0);
    for h in held {
        h.abort();
    }
}

/// 10 000 calls that nobody answers, with injected deadlines on a paused clock: nothing waits
/// real time, and neither the pending table nor the tombstone set grows past its bound.
#[tokio::test(start_paused = true)]
async fn pending_table_does_not_grow_with_unanswered_calls() {
    let (adapter_side, host_side) = tokio::io::duplex(1 << 22);
    let (host_read, host_write) = tokio::io::split(host_side);
    // the adapter reads what it is sent and never answers
    let (mut adapter_read, adapter_write) = tokio::io::split(adapter_side);
    let drain = tokio::spawn(async move {
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut adapter_read, &mut sink).await;
    });
    let conn = AdapterConn::start(
        host_read,
        host_write,
        ConnConfig::protocol1(),
        Arc::new(NoHostApi),
    );
    for i in 0..10_000u32 {
        let result = conn
            .call("runtime.prepare", &json!({"n": i}), Duration::from_secs(10))
            .await;
        assert_eq!(result, Err(CallError::Timeout));
        assert!(conn.in_flight() <= 1);
    }
    assert_eq!(conn.in_flight(), 0);
    assert!(conn.tombstone_count() <= 1024, "{}", conn.tombstone_count());
    assert_eq!(conn.counters().strikes, 0);
    drop(adapter_write);
    drain.abort();
}

#[tokio::test]
async fn truncated_json_then_eof_is_a_crash_not_a_hang() {
    let (_fx, conn) = ready(
        json!([{"on": "runtime.prepare", "do": "partial_line_then_eof"}]),
        ConnConfig::protocol1(),
    )
    .await;
    let started = Instant::now();
    let result = conn.call("runtime.prepare", &json!({}), SECS_10).await;
    assert!(matches!(result, Err(CallError::Crashed(_))), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(conn.closed().await, CloseReason::Truncated);
}

/// Answers `host.request` with far more than a frame holds.
struct Huge;

#[async_trait]
impl Inbound for Huge {
    async fn request(&self, _method: &str, _params: Params) -> Result<Params, ErrorObject> {
        let mut result = Params::new();
        result.insert("blob".into(), Value::String("y".repeat(300 * 1024)));
        Ok(result)
    }
}

#[tokio::test]
async fn oversize_reply_to_an_adapter_is_an_error_not_a_buffer() {
    let mut fx = Fixture::spawn(
        json!([{"on": "initialize", "do": ["reply", {"host_request": {"request": {"type": "status"}}}]}]),
    );
    let conn = fx.connect_with(ConnConfig::protocol1(), Arc::new(Huge));
    conn.call(
        "initialize",
        &initialize_params("fx", "1.0.0", "tool"),
        SECS_10,
    )
    .await
    .unwrap();

    let answer = fx
        .wait_report("host_answer")
        .await
        .expect("the adapter got an answer");
    let frame = &answer["frame"];
    assert!(
        frame.get("result").is_none(),
        "no oversize result was sent: {frame}"
    );
    assert_eq!(
        frame["error"]["data"]["code"], "E_REQUEST_TOO_LARGE",
        "{frame}"
    );
    assert!(!conn.is_closed());
}

#[tokio::test]
async fn stderr_flood_rotates_and_is_bounded() {
    use common::fixture::{wait_state, Rig};
    use oxidepm_adapter_proto::Kind;
    use oxidepmd::adapters::entry::EntryState;

    // 6 MB per second of junk lines on stderr for 3 seconds: 18 MB offered
    let rig = Rig::new(
        "chatty",
        Kind::Service,
        json!([{"on": "initialize", "do": ["reply", {"flood_stderr": {"bps": 6_000_000, "secs": 3}}]}]),
        |_| {},
    );
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    let state_dir = rig.dir.path().join("state").join("chatty");
    let log_dir = rig.dir.path().join("adapter-logs").join("chatty");

    // the flood ends; the adapter and the daemon are unaffected, and ping still answers
    assert!(
        wait_until(30, || rig.report().iter().any(|r| r["what"] == "flood")).await,
        "the flood finished"
    );
    assert!(rig.entry.call("ping", &json!({}), SECS_10).await.is_ok());
    assert_eq!(rig.entry.state(), EntryState::Ready);
    assert_eq!(rig.entry.view().crashes, 0);

    // the log rotated, and what is on disk is bounded: 5 files of 1 MiB
    assert!(!state_dir.join("stderr.log").exists());
    let sizes: Vec<(String, u64)> = std::fs::read_dir(&log_dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("stderr.log"))
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                e.metadata().unwrap().len(),
            )
        })
        .collect();
    let total: u64 = sizes.iter().map(|(_, n)| n).sum();
    assert!(
        sizes.iter().any(|(n, _)| n == "stderr.log.1"),
        "it rotated: {sizes:?}"
    );
    assert!(sizes.len() <= 5, "{sizes:?}");
    assert!(
        total <= 5 * 1024 * 1024 + 64 * 1024,
        "{total} bytes on disk: {sizes:?}"
    );
    // far less than was offered: the rate limit dropped the rest, and counted what it dropped
    assert!(total < 8 * 1024 * 1024, "{total}");
    assert!(
        rig.entry.view().stderr_dropped > 0,
        "{:?}",
        rig.entry.view()
    );
    // and the log is private
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(log_dir.join("stderr.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // the last lines are kept for `adapter info`, 20 at most
    assert!(!rig.entry.stderr_tail().is_empty() && rig.entry.stderr_tail().len() <= 20);
    rig.entry.stop(false).await;
}

/// A service that floods valid frames without end: the daemon keeps answering while the reader of
/// that adapter is held back by the rate limit (the time to the kill is 30 s in production and is
/// tested with injected durations above). A bound on latency, not on CPU.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_flood_keeps_the_daemon_answering() {
    use common::daemon::{DaemonProc, Signal};
    use common::fixture::{adapter_states, install, wait_adapter_state, InstallSpec};

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fx = install(
        &home,
        InstallSpec::service("flooder", json!([{"on": "initialize", "do": ["reply", {"flood_stdout": {"fps": 0, "bytes": 100, "count": 1000000}}]}])),
    )
    .await;
    let env = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &env);
    assert!(
        wait_adapter_state(&daemon, "flooder", "ready", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    // let the flood get going (it never ends by itself: the reader is held to 500 frames a second)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // sample the daemon's answers while the flood goes on
    let mut samples = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let reply = daemon.rpc(json!({ "type": "daemon_status" }));
        samples.push(started.elapsed());
        assert_eq!(reply["type"], "daemon_status");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    assert!(
        median < Duration::from_millis(100),
        "median {median:?}, slowest {:?}",
        samples.last()
    );
    assert!(
        *samples.last().unwrap() < Duration::from_secs(3),
        "{samples:?}"
    );
    // the flooder is still the host's to deal with: it has not taken the daemon down
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}
