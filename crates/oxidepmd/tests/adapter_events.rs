//! Events to services: the allow-listed event, the per-service queues, the
//! `adapter` family and the daemon's third sink, driven by the fixture adapter.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use common::daemon::{DaemonProc, Signal};
use common::fixture::{install, wait_adapter_state, wait_state, wait_until, InstallSpec, Rig};
use oxidepm_adapter_proto::{HostScope, Kind};
use oxidepm_core::{AppSpec, Runtime};
use oxidepmd::adapters::entry::EntryState;
use oxidepmd::adapters::events::{
    adapter_params, fit_event_line, lifecycle_params, AdapterEvent, AdapterEventSink, EventHub,
    QueueLimits,
};
use oxidepmd::supervisor::{
    AppId, Cause, EventDetail, EventDraft, EventSink, Lifecycle, LifecycleEvent, LifecycleKind,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ---- a counting allocator, per thread ------------------------------------------------------

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn count_one() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

// SAFETY: every method forwards to the system allocator unchanged; the counting touches only
// `const`-initialised thread-local cells that need no allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_one();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_one();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_one();
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// How many allocations `f` makes on this thread.
fn allocations_in(f: impl FnOnce()) -> u64 {
    ALLOCATIONS.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    f();
    COUNTING.with(|on| on.set(false));
    ALLOCATIONS.with(Cell::get)
}

// ---- helpers --------------------------------------------------------------------------------

fn spec(name: &str) -> AppSpec {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "server-binary".to_string(),
        "/srv/app".into(),
    );
    spec.args = vec!["--listen".to_string(), "arg-visible-to-nobody".to_string()];
    spec.tags = vec!["prod".to_string(), "web".to_string()];
    spec.env.insert(
        "DB_DSN".to_string(),
        "postgres://user:planted-secret-dsn@db/app".to_string(),
    );
    spec.env
        .insert("API_KEY".to_string(), "planted-api-key-value".to_string());
    spec.hooks.on_crash = Some("curl -d planted-hook-secret https://example.test".to_string());
    spec
}

fn event(seq: u64, kind: LifecycleKind, spec: &AppSpec) -> LifecycleEvent {
    LifecycleEvent {
        seq,
        at: Utc::now(),
        app_id: 7,
        app: Arc::from(spec.name.as_str()),
        kind,
        cause: Cause::Crash,
        actor: Some(Arc::from("human:cli")),
        detail: EventDetail::new(&Arc::new(spec.clone())),
    }
}

fn draft(kind: LifecycleKind, spec: &Arc<AppSpec>) -> EventDraft {
    let mut detail = EventDetail::new(spec);
    detail.restarts = 1;
    EventDraft {
        app_id: 7 as AppId,
        kind,
        cause: Cause::User,
        actor: None,
        detail,
    }
}

fn names(families: &[&str]) -> Vec<String> {
    families.iter().map(|f| f.to_string()).collect()
}

fn small_limits(normal: usize, critical: usize, bytes: usize) -> QueueLimits {
    QueueLimits {
        normal,
        critical,
        bytes,
        event_line: 16 * 1024,
    }
}

/// The `params` of every `event` frame the fixture received, in order.
fn event_frames(report: &[Value]) -> Vec<Value> {
    report
        .iter()
        .filter(|r| r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "event")
        .map(|r| r["frame"]["params"].clone())
        .collect()
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

// ---- the event itself ----------------------------------------------------------------------

#[test]
fn events_have_only_allow_listed_fields() {
    // These event fields are part of the protocol.
    const TOP: &[&str] = &[
        "app",
        "at",
        "data",
        "dropped_before",
        "family",
        "kind",
        "options",
        "seq",
    ];
    const APP: &[&str] = &["id", "name", "tags"];
    const DATA: &[&str] = &[
        "actor",
        "cause",
        "exit_code",
        "exit_signal",
        "limit_mb",
        "memory_mb",
        "pid",
        "reason",
        "restarts",
    ];

    let mut spec = spec("web");
    spec.adapters
        .insert("svc".to_string(), json!({"chat": "ops"}));
    let mut full = event(9, LifecycleKind::MemoryLimit, &spec);
    full.detail.pid = Some(4242);
    full.detail.exit_code = Some(1);
    full.detail.exit_signal = Some(9);
    full.detail.restarts = 3;
    full.detail.reason = Some("over the limit".to_string());
    full.detail.memory_mb = Some((900, 512));

    let params = serde_json::to_value(lifecycle_params(&full, "svc", Some(2))).unwrap();
    assert_eq!(keys(&params), TOP, "{params}");
    assert_eq!(keys(&params["app"]), APP);
    assert_eq!(
        keys(&params["data"]),
        DATA,
        "every allowed field shows up when the event has it"
    );
    assert_eq!(params["family"], "lifecycle");
    assert_eq!(params["kind"], "memory_limit");
    assert_eq!(params["data"]["cause"], "crash");
    assert_eq!(
        params["app"],
        json!({"id": 7, "name": "web", "tags": ["prod", "web"]})
    );

    // nothing of the spec that is not on the list: not the command, the arguments, the working
    // directory, the environment keys or values, the hooks
    let text = params.to_string();
    for absent in [
        "server-binary",
        "arg-visible-to-nobody",
        "/srv/app",
        "DB_DSN",
        "API_KEY",
        "planted",
        "hooks",
        "on_crash",
    ] {
        assert!(!text.contains(absent), "{absent} is in the event: {text}");
    }

    // a bare event has fewer keys, never other ones
    let bare = serde_json::to_value(lifecycle_params(
        &event(1, LifecycleKind::Started, &spec),
        "nobody",
        None,
    ))
    .unwrap();
    assert!(
        keys(&bare["data"])
            .iter()
            .all(|k| DATA.contains(&k.as_str())),
        "{bare}"
    );
    assert_eq!(keys(&bare["data"]), ["actor", "cause", "restarts"]);
    assert!(
        bare.get("options").is_none() && bare.get("dropped_before").is_none(),
        "{bare}"
    );

    // the adapter family has its own keys, and no app
    let adapter = AdapterEvent {
        seq: 1,
        at: Utc::now(),
        kind: "failed".to_string(),
        adapter: "notifier".to_string(),
        version: Some("1.0.0".to_string()),
        reason: Some("tampered".to_string()),
    };
    let params = serde_json::to_value(adapter_params(&adapter, None)).unwrap();
    assert_eq!(
        keys(&params),
        ["at", "data", "family", "kind", "seq"],
        "{params}"
    );
    assert_eq!(keys(&params["data"]), ["adapter", "reason", "version"]);
}

#[test]
fn event_data_has_no_env_values() {
    let mut spec = spec("web");
    spec.env.insert("TINY".to_string(), "on".to_string());
    let mut ev = event(1, LifecycleKind::Errored, &spec);
    ev.detail.reason = Some(
        "could not connect to postgres://user:planted-secret-dsn@db/app with planted-api-key-value"
            .to_string(),
    );
    let params = lifecycle_params(&ev, "svc", None);
    let text = serde_json::to_string(&params).unwrap();
    for secret in [
        "planted-secret-dsn",
        "planted-api-key-value",
        "postgres://user",
    ] {
        assert!(!text.contains(secret), "{secret} is in the event: {text}");
    }
    let reason = params.data["reason"].as_str().unwrap();
    assert!(
        reason.contains("<redacted>") && reason.contains("could not connect to"),
        "{reason}"
    );
    // a value too short to be told from a word (under 4 bytes) is not hunted for
    ev.detail.reason = Some("turned on".to_string());
    assert_eq!(
        lifecycle_params(&ev, "svc", None).data["reason"],
        "turned on"
    );

    // a long reason is cut
    ev.detail.reason = Some("x".repeat(5000));
    assert_eq!(
        lifecycle_params(&ev, "svc", None).data["reason"]
            .as_str()
            .unwrap()
            .len(),
        1024
    );
}

#[test]
fn events_carry_only_the_adapters_own_options() {
    let mut spec = spec("web");
    spec.adapters
        .insert("alpha".to_string(), json!({"chat": "ops-room"}));
    spec.adapters
        .insert("beta".to_string(), json!({"token": "beta-token-value"}));
    let ev = event(1, LifecycleKind::Started, &spec);

    let alpha = serde_json::to_string(&lifecycle_params(&ev, "alpha", None)).unwrap();
    assert!(
        alpha.contains("ops-room") && alpha.contains("\"options\":{\"chat\""),
        "{alpha}"
    );
    assert!(
        !alpha.contains("beta-token-value") && !alpha.contains("beta"),
        "{alpha}"
    );

    let beta = serde_json::to_string(&lifecycle_params(&ev, "beta", None)).unwrap();
    assert!(beta.contains("beta-token-value"), "{beta}");
    assert!(
        !beta.contains("ops-room") && !beta.contains("alpha"),
        "{beta}"
    );

    assert!(
        lifecycle_params(&ev, "gamma", None).options.is_none(),
        "an adapter with no options gets none"
    );

    // options over 4 KiB are never sent (validation keeps them out; an old row might hold them)
    spec.adapters
        .insert("big".to_string(), json!({"blob": "x".repeat(5000)}));
    assert!(
        lifecycle_params(&event(1, LifecycleKind::Started, &spec), "big", None)
            .options
            .is_none()
    );

    // and an event that would not fit a line loses its options first, then its reason
    spec.adapters
        .insert("fat".to_string(), json!({"a": "y".repeat(4000)}));
    let mut ev = event(1, LifecycleKind::Errored, &spec);
    ev.detail.reason = Some("r".repeat(900));
    let params = lifecycle_params(&ev, "fat", None);
    assert!(params.options.is_some());
    let fitted = fit_event_line(params.clone(), 3000).expect("fits without the options");
    assert!(fitted.options.is_none() && fitted.data.contains_key("reason"));
    let fitted = fit_event_line(params, 600).expect("fits without the reason");
    assert!(fitted.options.is_none() && !fitted.data.contains_key("reason"));
    let tiny = fit_event_line(lifecycle_params(&ev, "fat", None), 50);
    assert!(
        tiny.is_none(),
        "an event that cannot fit is dropped, not truncated"
    );
}

#[test]
fn empty_adapter_options_table_is_still_delivered_to_its_owner() {
    let mut spec = spec("web");
    spec.adapters.insert("watch".to_string(), json!({}));
    let started = event(1, LifecycleKind::Started, &spec);
    assert_eq!(
        lifecycle_params(&started, "watch", None).options,
        Some(serde_json::Map::new())
    );
    assert_eq!(lifecycle_params(&started, "other", None).options, None);
}

// ---- the queue ------------------------------------------------------------------------------

/// A hub with one live service queue that nobody drains.
fn held_queue(
    limits: QueueLimits,
) -> (Arc<EventHub>, Arc<oxidepmd::adapters::events::ServiceQueue>) {
    let hub = EventHub::with_limits(limits);
    let queue = hub
        .register("svc", &names(&["lifecycle"]), HostScope::All)
        .expect("a queue");
    queue.go_live();
    (hub, queue)
}

fn seqs(queue: &oxidepmd::adapters::events::ServiceQueue) -> Vec<(u64, Option<u64>)> {
    let mut out = Vec::new();
    while let Some(taken) = queue.take() {
        let seq = match taken.payload {
            oxidepmd::adapters::events::Payload::Lifecycle(ev) => ev.seq,
            oxidepmd::adapters::events::Payload::Adapter(ev) => ev.seq,
        };
        out.push((seq, taken.dropped_before));
        queue.finished();
    }
    out
}

#[test]
fn queue_overflow_drops_oldest_and_reports_dropped_before() {
    let spec = spec("web");
    let (hub, queue) = held_queue(small_limits(5, 2, usize::MAX));
    for seq in 1..=8 {
        hub.publish_lifecycle(&event(seq, LifecycleKind::Started, &spec));
    }
    assert_eq!(queue.dropped_total(), 3);
    assert_eq!(queue.pending(), 5);
    // the oldest three went; the first event delivered says so, the others do not
    assert_eq!(
        seqs(&queue),
        [(4, Some(3)), (5, None), (6, None), (7, None), (8, None)]
    );
    assert_eq!(queue.pending(), 0);

    // the count starts again from zero
    hub.publish_lifecycle(&event(9, LifecycleKind::Started, &spec));
    assert_eq!(seqs(&queue), [(9, None)]);

    // the byte cap drops the oldest normal events too (an event counts for a few hundred bytes)
    let (hub, queue) = held_queue(small_limits(1000, 100, 1000));
    for seq in 1..=20 {
        hub.publish_lifecycle(&event(seq, LifecycleKind::Started, &spec));
    }
    let taken = seqs(&queue);
    assert!(taken.len() < 8 && taken.len() >= 2, "{taken:?}");
    assert_eq!(taken.last().unwrap().0, 20, "the newest is kept");
    assert_eq!(taken[0].1, Some(20 - taken.len() as u64), "{taken:?}");
}

#[test]
fn critical_events_survive_normal_overflow() {
    let spec = spec("web");
    let (hub, queue) = held_queue(small_limits(3, 2, usize::MAX));
    let mut seq = 0;
    let mut next = |kind| {
        seq += 1;
        event(seq, kind, &spec)
    };
    hub.publish_lifecycle(&next(LifecycleKind::Started)); // 1
    hub.publish_lifecycle(&next(LifecycleKind::Crashed)); // 2, critical
    for _ in 0..10 {
        hub.publish_lifecycle(&next(LifecycleKind::Started)); // 3..=12
    }
    hub.publish_lifecycle(&next(LifecycleKind::CrashLoop)); // 13, critical
    for _ in 0..5 {
        hub.publish_lifecycle(&next(LifecycleKind::Started)); // 14..=18
    }
    let taken: Vec<u64> = seqs(&queue).into_iter().map(|(s, _)| s).collect();
    assert!(
        taken.contains(&2) && taken.contains(&13),
        "the critical events were lost: {taken:?}"
    );
    assert_eq!(
        taken,
        [2, 13, 16, 17, 18],
        "critical ones stay in arrival order among the three newest normal ones"
    );

    // the critical part has its limit as well: the oldest critical goes first
    let (hub, queue) = held_queue(small_limits(3, 2, usize::MAX));
    for seq in 1..=4 {
        hub.publish_lifecycle(&event(seq, LifecycleKind::Crashed, &spec));
    }
    let taken = seqs(&queue);
    assert_eq!(taken, [(3, Some(2)), (4, None)]);
}

#[test]
fn a_service_that_went_down_does_not_keep_old_events_for_the_next_process() {
    let spec = spec("web");
    let (hub, queue) = held_queue(small_limits(10, 5, usize::MAX));
    for seq in 1..=3 {
        hub.publish_lifecycle(&event(seq, LifecycleKind::Started, &spec));
    }
    queue.go_down();
    assert_eq!(queue.pending(), 0);
    assert!(queue.take().is_none());
    // while it is down, events are counted; when it is back the first one says how many were lost
    hub.publish_lifecycle(&event(4, LifecycleKind::Started, &spec));
    queue.go_live();
    hub.publish_lifecycle(&event(5, LifecycleKind::Started, &spec));
    assert_eq!(seqs(&queue), [(5, Some(4))]);
}

#[test]
fn events_are_not_delivered_to_an_adapter_without_the_family() {
    let spec = spec("web");
    let hub = EventHub::new();
    assert!(
        hub.register("none", &[], HostScope::All).is_none(),
        "no families, no queue"
    );
    assert!(
        hub.register("unknown", &names(&["logs", "gate"]), HostScope::All)
            .is_none(),
        "families the protocol does not have are ignored"
    );
    let adapter_only = hub
        .register("watcher", &names(&["adapter"]), HostScope::All)
        .unwrap();
    adapter_only.go_live();
    let lifecycle_only = hub
        .register("notifier", &names(&["lifecycle", "bogus"]), HostScope::All)
        .unwrap();
    lifecycle_only.go_live();

    hub.publish_lifecycle(&event(1, LifecycleKind::Crashed, &spec));
    assert_eq!(adapter_only.pending(), 0);
    assert_eq!(
        adapter_only.dropped_total(),
        0,
        "an event of another family is not even a drop"
    );
    assert_eq!(lifecycle_only.pending(), 1);

    hub.publish_adapter("other", "started", Some("1.0.0"), None);
    assert_eq!(adapter_only.pending(), 1);
    assert_eq!(
        lifecycle_only.pending(),
        1,
        "the lifecycle service does not receive adapter events"
    );

    // an adapter does not get events about itself
    hub.publish_adapter("watcher", "started", None, None);
    assert_eq!(adapter_only.pending(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_service_without_the_family_gets_no_event_frame() {
    let rig = Rig::new("quiet", Kind::Service, json!([]), |_| {});
    let hub = EventHub::new();
    rig.entry.bind_events(&hub, &names(&["adapter"]));
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);

    let spec = Arc::new(spec("web"));
    let sink = AdapterEventSink::new();
    sink.bind(Arc::clone(&hub));
    let lifecycle = Lifecycle::new(vec![sink as Arc<dyn EventSink>]);
    for _ in 0..5 {
        lifecycle.emit(draft(LifecycleKind::Crashed, &spec));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(event_frames(&rig.report()).is_empty(), "{:?}", rig.report());
    rig.entry.kill_now().await;
}

#[test]
fn emit_with_no_adapter_subscribers_allocates_nothing() {
    let spec = spec("web");
    let ev = event(1, LifecycleKind::Crashed, &spec);

    // the sink before the host exists
    let sink = AdapterEventSink::new();
    assert_eq!(
        allocations_in(|| sink.on_event(&ev)),
        0,
        "an unbound sink allocated"
    );

    // bound, with nobody listening to the family
    let hub = EventHub::new();
    sink.bind(Arc::clone(&hub));
    assert_eq!(
        allocations_in(|| sink.on_event(&ev)),
        0,
        "a sink with no subscriber allocated"
    );
    let watcher = hub
        .register("watcher", &names(&["adapter"]), HostScope::All)
        .unwrap();
    watcher.go_live();
    assert_eq!(
        allocations_in(|| sink.on_event(&ev)),
        0,
        "a subscriber of another family made the sink allocate"
    );

    // the counter can see allocations: with a live subscriber the event is cloned once
    let notifier = hub
        .register("notifier", &names(&["lifecycle"]), HostScope::All)
        .unwrap();
    notifier.go_live();
    assert!(
        allocations_in(|| sink.on_event(&ev)) > 0,
        "the counting allocator counts nothing"
    );
    assert_eq!(notifier.pending(), 1);

    // a subscriber that is down costs nothing either: the event is only counted as dropped
    notifier.go_down();
    assert_eq!(
        allocations_in(|| sink.on_event(&ev)),
        0,
        "a down subscriber made the sink allocate"
    );
    assert_eq!(
        notifier.dropped_total(),
        2,
        "the event queued before it went down, and this one"
    );
}

// ---- with a running service ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_never_blocks_with_a_deaf_adapter() {
    // the fixture stops reading its input at the first event
    let rig = Rig::new(
        "deaf",
        Kind::Service,
        json!([{"on": "event", "do": "deaf"}]),
        |_| {},
    );
    let hub = EventHub::new();
    rig.entry.bind_events(&hub, &names(&["lifecycle"]));
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);

    let spec = Arc::new(spec("web"));
    let sink = AdapterEventSink::new();
    sink.bind(Arc::clone(&hub));
    let lifecycle = Lifecycle::new(vec![sink as Arc<dyn EventSink>]);

    // the first event makes it deaf; give it the moment it needs
    lifecycle.emit(draft(LifecycleKind::Started, &spec));
    assert!(rig.wait_for_report("deaf").await);

    let started = Instant::now();
    for _ in 0..10_000 {
        lifecycle.emit(draft(LifecycleKind::Crashed, &spec));
    }
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "10 000 emits took {took:?}");

    // the adapter's queue is bounded, and what did not fit was counted
    let queue = hub.queue("deaf").unwrap();
    assert!(queue.pending() <= 256 + 1, "pending {}", queue.pending());
    assert!(
        queue.dropped_total() > 5000,
        "dropped {}",
        queue.dropped_total()
    );
    rig.entry.kill_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seq_gaps_are_visible_to_the_adapter() {
    let rig = Rig::new("gaps", Kind::Service, json!([]), |_| {});
    let hub = EventHub::new();
    rig.entry.bind_events(&hub, &names(&["lifecycle"]));
    let spec = Arc::new(spec("web"));
    let sink = AdapterEventSink::new();
    sink.bind(Arc::clone(&hub));
    let lifecycle = Lifecycle::new(vec![sink as Arc<dyn EventSink>]);

    // three events while the service is not running: counted, not queued
    for _ in 0..3 {
        lifecycle.emit(draft(LifecycleKind::Started, &spec));
    }
    assert_eq!(hub.queue("gaps").unwrap().dropped_total(), 3);

    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);
    lifecycle.emit(draft(LifecycleKind::Crashed, &spec));
    lifecycle.emit(draft(LifecycleKind::Stopped, &spec));
    assert!(
        wait_until(15, || event_frames(&rig.report()).len() == 2).await,
        "{:?}",
        rig.report()
    );

    let frames = event_frames(&rig.report());
    assert_eq!(
        frames[0]["seq"], 4,
        "the sequence number is the daemon's: 1 to 3 were never sent"
    );
    assert_eq!(frames[0]["dropped_before"], 3);
    assert_eq!(frames[0]["kind"], "crashed");
    assert_eq!(frames[1]["seq"], 5);
    assert!(frames[1].get("dropped_before").is_none(), "{}", frames[1]);
    assert_eq!(frames[0]["family"], "lifecycle");
    assert_eq!(frames[0]["app"]["name"], "web");
    rig.entry.kill_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_family_event_reaches_a_subscribed_service() {
    let hub = EventHub::new();
    let watcher = Rig::new("watcher", Kind::Service, json!([]), |_| {});
    watcher.entry.bind_events(&hub, &names(&["adapter"]));
    watcher.entry.start();
    assert!(wait_state(&watcher.entry, 15, |s| *s == EntryState::Ready).await);

    // another adapter starts and then breaks the protocol three times, so it is cut off; the
    // watcher is told, with the adapter family's own sequence numbers and no app
    let garbage =
        json!([{"garbage": {"text": "a"}}, {"garbage": {"text": "b"}}, {"garbage": {"text": "c"}}]);
    let other = Rig::new(
        "other",
        Kind::Service,
        json!([{"on": "after_ms:300", "do": garbage}]),
        |_| {},
    );
    other.entry.bind_events(&hub, &[]);
    other.entry.start();

    assert!(
        wait_until(15, || event_frames(&watcher.report()).len() >= 2).await,
        "{:?}",
        watcher.report()
    );
    let frames = event_frames(&watcher.report());
    assert_eq!(frames[0]["family"], "adapter");
    assert_eq!(frames[0]["kind"], "started");
    assert_eq!(frames[0]["seq"], 1);
    assert_eq!(
        frames[0]["data"],
        json!({"adapter": "other", "version": "1.0.0"})
    );
    assert!(frames[0].get("app").is_none(), "{}", frames[0]);
    assert_eq!(frames[1]["kind"], "killed");
    assert_eq!(frames[1]["seq"], 2);
    assert!(
        frames.iter().all(|f| f["data"]["adapter"] == "other"),
        "the watcher is not told about itself"
    );
    other.entry.kill_now().await;
    watcher.entry.kill_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_queued_at_a_stop_are_written_before_shutdown() {
    let rig = Rig::new("flush", Kind::Service, json!([]), |_| {});
    let hub = EventHub::new();
    rig.entry.bind_events(&hub, &names(&["lifecycle"]));
    rig.entry.start();
    assert!(wait_state(&rig.entry, 15, |s| *s == EntryState::Ready).await);

    let spec = Arc::new(spec("web"));
    let sink = AdapterEventSink::new();
    sink.bind(Arc::clone(&hub));
    let lifecycle = Lifecycle::new(vec![sink as Arc<dyn EventSink>]);
    for _ in 0..50 {
        lifecycle.emit(draft(LifecycleKind::Stopped, &spec));
    }
    // the polite stop follows at once, as the daemon's shutdown does after the apps stopped
    rig.entry.stop(false).await;

    let report = rig.report();
    let frame_methods: Vec<String> = report
        .iter()
        .filter(|r| r["what"] == "frame" && r["dir"] == "in")
        .map(|r| r["frame"]["method"].as_str().unwrap_or("").to_string())
        .collect();
    let last_event = frame_methods
        .iter()
        .rposition(|m| m == "event")
        .expect("events arrived");
    let shutdown = frame_methods
        .iter()
        .position(|m| m == "shutdown")
        .expect("shutdown arrived");
    assert!(last_event < shutdown, "{frame_methods:?}");
    assert_eq!(
        event_frames(&report).len(),
        50,
        "all 50 were written before shutdown"
    );
}

// ---- the options, and the whole daemon ---------------------------------------------------------

fn sleeper(workdir: &Path, name: &str, secs: &str) -> AppSpec {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec![secs.to_string()];
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opted_in_service_receives_only_events_for_apps_that_list_it() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fixture = install(
        &home,
        InstallSpec {
            perms: Some(
                json!({"host": {"level": "read", "scope": "opted-in"}, "events": ["lifecycle"]}),
            ),
            ..InstallSpec::service("observer", json!([]))
        },
    )
    .await;
    let env = fixture.env();
    let refs: Vec<_> = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &refs);
    assert!(wait_adapter_state(&daemon, "observer", "ready", 20).await);
    let private = sleeper(tmp.path(), "private", "300.1");
    let mut shared = sleeper(tmp.path(), "shared", "300.2");
    shared.adapters.insert("observer".into(), json!({}));
    for spec in [private, shared] {
        let started = daemon.rpc(json!({"type": "start", "spec": spec}));
        assert_eq!(started["type"], "started", "{started}");
    }
    assert!(
        wait_until(10, || event_frames(&fixture.report())
            .iter()
            .any(|event| event["app"]["name"] == "shared"))
        .await
    );
    let events = event_frames(&fixture.report());
    assert!(
        events.iter().all(|event| event["app"]["name"] == "shared"),
        "{events:?}"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn options_values_appear_only_in_the_owners_event() {
    // the events: each adapter sees its own values and nothing of the other's
    let mut spec = spec("web");
    spec.adapters.insert(
        "owner".to_string(),
        json!({"chat": "value-of-owner-option"}),
    );
    spec.adapters.insert(
        "other".to_string(),
        json!({"token": "value-of-other-option"}),
    );
    let ev = event(1, LifecycleKind::Started, &spec);
    let for_owner = serde_json::to_string(&lifecycle_params(&ev, "owner", None)).unwrap();
    let for_other = serde_json::to_string(&lifecycle_params(&ev, "other", None)).unwrap();
    assert!(
        for_owner.contains("value-of-owner-option") && !for_owner.contains("value-of-other-option")
    );
    assert!(
        for_other.contains("value-of-other-option") && !for_other.contains("value-of-owner-option")
    );

    // the daemon: Status and Show show the names of the options, and only a human with reveal
    // sees the values
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    for name in ["owner", "other"] {
        install(&home, InstallSpec::tool(name, json!([]))).await;
    }
    let daemon = DaemonProc::spawn(&home);
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut app = sleeper(&workdir, "opts-app", "302.5");
    app.adapters.insert(
        "owner".to_string(),
        json!({"chat": "value-of-owner-option"}),
    );
    app.adapters.insert(
        "other".to_string(),
        json!({"token": "value-of-other-option"}),
    );

    let started = daemon.rpc(json!({ "type": "start", "spec": app }));
    let selector = json!({ "by": "name", "value": "opts-app" });
    let status = daemon.rpc(json!({ "type": "status" }));
    let show = daemon.rpc(json!({ "type": "show", "selector": selector }));
    let revealed = daemon.rpc_with(json!({ "type": "show", "selector": selector }), true);
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert_eq!(started["type"], "started", "{started}");
    for (what, reply) in [("status", &status), ("show", &show)] {
        let raw = reply.to_string();
        assert_ne!(reply["type"], "error", "{what}: {raw}");
        for value in ["value-of-owner-option", "value-of-other-option"] {
            assert!(!raw.contains(value), "{what} leaked {value}: {raw}");
        }
        for name in ["owner", "chat", "other", "token"] {
            assert!(
                raw.contains(name),
                "{what} lost the option name {name}: {raw}"
            );
        }
    }
    let raw = revealed.to_string();
    assert!(
        raw.contains("value-of-owner-option") && raw.contains("value-of-other-option"),
        "a human with reveal sees the values: {raw}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_delivers_lifecycle_events_to_an_installed_service() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fx = install(
        &home,
        InstallSpec {
            perms: Some(json!({ "events": ["lifecycle"] })),
            ..InstallSpec::service("svc", json!([]))
        },
    )
    .await;
    install(&home, InstallSpec::tool("someone-else", json!([]))).await;
    let env: Vec<(String, String)> = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &env);
    assert!(wait_adapter_state(&daemon, "svc", "ready", 20).await);

    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut app = sleeper(&workdir, "evented", "303.5");
    app.env.insert(
        "DB_DSN".to_string(),
        "postgres://user:planted-secret-dsn@db/app".to_string(),
    );
    app.adapters
        .insert("svc".to_string(), json!({"chat": "ops-room"}));
    app.adapters.insert(
        "someone-else".to_string(),
        json!({"token": "planted-other-token"}),
    );
    let started = daemon.rpc(json!({ "type": "start", "spec": app }));
    assert_eq!(started["type"], "started", "{started}");
    let stopped =
        daemon.rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "evented" } }));
    assert_ne!(stopped["type"], "error", "{stopped}");

    let both = wait_until(20, || {
        let kinds: Vec<Value> = event_frames(&fx.report())
            .iter()
            .map(|f| f["kind"].clone())
            .collect();
        kinds.contains(&json!("started")) && kinds.contains(&json!("stopped"))
    })
    .await;
    let frames = event_frames(&fx.report());
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());

    assert!(both, "{frames:?}");
    let first = frames.iter().find(|f| f["kind"] == "started").unwrap();
    assert_eq!(first["family"], "lifecycle");
    assert_eq!(first["app"]["name"], "evented");
    assert_eq!(first["options"], json!({"chat": "ops-room"}));
    let raw = serde_json::to_string(&frames).unwrap();
    for secret in ["planted-secret-dsn", "planted-other-token", "DB_DSN"] {
        assert!(!raw.contains(secret), "{secret} reached the adapter: {raw}");
    }
}
