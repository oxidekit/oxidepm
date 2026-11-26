//! Events to services: the third event sink, the per-service queues and
//! the pump that writes the events to a running service.
//!
//! The rule that protects the actors is the same as for the hook and notification sinks: `emit`
//! never awaits and a sink must not block. So the sink ([`AdapterEventSink`]) does almost nothing
//! on the actor's task:
//!
//! - With no service subscribed it reads one atomic and returns; it allocates nothing.
//! - Otherwise it clones the event once into an `Arc` and pushes it onto the queue of every
//!   subscribed service ([`ServiceQueue`]): one short mutex, a bounded deque, a wake-up. It cannot
//!   wait for the adapter.
//! - The allow-list copy of the event, the redaction of its text and the JSON encoding happen in
//!   the service's own pump task ([`Pump`]), which is the only thing that talks to the adapter.
//!
//! A queue holds at most 1024 normal and 256 critical events and 4 MiB (the numbers of
//! `Limits::PROTOCOL_1`). When it is full the oldest normal event goes (the oldest critical one
//! when the critical part is full), and the count of what was dropped travels with the next event
//! delivered, as `dropped_before`. A service that is not running does not queue: its events are
//! counted as dropped and reported the same way when it is back. Events that were queued when a
//! service's connection ended are dropped, not delivered to a later process.
//!
//! Two families exist in protocol 1. `lifecycle` events are the core's [`LifecycleEvent`]s and
//! carry its daemon-wide sequence number. `adapter` events (an adapter started, exited, was
//! killed, failed) have no app and no spec: they are a type of their own with their own counter
//! ([`AdapterEvent`]). A service that listens to both sees two sequence spaces, told apart by
//! `family`.
//!
//! What an event may hold is an allow-list written out in [`lifecycle_params`]; it is never made
//! by serialising the spec.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use oxidepm_adapter_proto::messages::{method, EventApp, EventParams};
use oxidepm_adapter_proto::permissions::EVENT_FAMILIES;
use oxidepm_adapter_proto::{HostScope, Limits};
use oxidepm_core::scrub_text;
use oxidepm_core::validate::MAX_ADAPTER_OPTIONS_BYTES;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Map, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::conn::{AdapterConn, CallError};
use crate::supervisor::{Cause, EventSink, LifecycleEvent, LifecycleKind};

/// The family of the core's lifecycle events.
pub const FAMILY_LIFECYCLE: &str = "lifecycle";
/// The family of the adapter system's own events.
pub const FAMILY_ADAPTER: &str = "adapter";

/// The longest `reason` an event carries, in characters.
pub const REASON_MAX_CHARS: usize = 1024;

/// The room the JSON-RPC envelope of an `event` frame takes beyond its params.
const ENVELOPE_BYTES: usize = 64;
/// How long a stop waits for the pump to write what is queued (the adapter shutdown grace is unchanged:
/// this is spent inside the polite stop of a service).
pub const FLUSH_WAIT: Duration = Duration::from_secs(1);
/// The pause before the pump tries again when the connection's outgoing queue is full.
const BUSY_PAUSE: Duration = Duration::from_millis(10);

/// The sizes of one service's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    pub normal: usize,
    pub critical: usize,
    pub bytes: usize,
    /// The longest `event` line sent to the adapter.
    pub event_line: usize,
}

impl QueueLimits {
    pub fn protocol1() -> Self {
        let limits = Limits::PROTOCOL_1;
        Self {
            normal: limits.event_queue as usize,
            critical: limits.event_queue_critical as usize,
            bytes: limits.event_queue_bytes,
            event_line: limits.event_line,
        }
    }
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self::protocol1()
    }
}

// ---------------------------------------------------------------------------------------------
// The adapter family
// ---------------------------------------------------------------------------------------------

/// Something that happened to an adapter. It has no app and no shared sequence number.
#[derive(Debug, Clone)]
pub struct AdapterEvent {
    /// From the hub's own counter.
    pub seq: u64,
    pub at: DateTime<Utc>,
    /// `started`, `exited`, `killed`, `disabled`, `failed`, `imported` or `rolled_back`.
    pub kind: String,
    /// The adapter it is about.
    pub adapter: String,
    pub version: Option<String>,
    /// For `failed`: the reason's short name (`tampered`, `sandbox`, ...).
    pub reason: Option<String>,
}

/// Does losing this kind of adapter event hurt more than losing a `started`?
fn adapter_event_is_critical(kind: &str) -> bool {
    matches!(kind, "killed" | "failed")
}

// ---------------------------------------------------------------------------------------------
// The queue of one service
// ---------------------------------------------------------------------------------------------

/// What a queue holds.
#[derive(Debug, Clone)]
pub enum Payload {
    Lifecycle(Arc<LifecycleEvent>),
    Adapter(Arc<AdapterEvent>),
}

struct Queued {
    ticket: u64,
    payload: Payload,
    bytes: usize,
}

#[derive(Default)]
struct QueueState {
    /// Is a pump attached? A queue that is not live drops (and counts) what it is offered.
    live: bool,
    normal: VecDeque<Queued>,
    critical: VecDeque<Queued>,
    bytes: usize,
    ticket: u64,
    /// Dropped since the last event taken.
    dropped: u64,
    dropped_total: u64,
    /// Taken by the pump and not yet written.
    in_progress: usize,
}

/// What the pump takes: an event and the number dropped before it.
#[derive(Debug, Clone)]
pub struct Taken {
    pub payload: Payload,
    pub dropped_before: Option<u64>,
}

/// The bounded queue of one subscribed service.
pub struct ServiceQueue {
    name: String,
    lifecycle: bool,
    scope: HostScope,
    adapter: bool,
    limits: QueueLimits,
    state: Mutex<QueueState>,
    wake: Notify,
}

impl ServiceQueue {
    fn new(
        name: &str,
        families: &[String],
        scope: HostScope,
        limits: QueueLimits,
    ) -> Arc<ServiceQueue> {
        Arc::new(ServiceQueue {
            name: name.to_string(),
            lifecycle: families.iter().any(|f| f == FAMILY_LIFECYCLE),
            scope,
            adapter: families.iter().any(|f| f == FAMILY_ADAPTER),
            limits,
            state: Mutex::new(QueueState::default()),
            wake: Notify::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Does the service listen to this family?
    pub fn wants(&self, family: &str) -> bool {
        match family {
            FAMILY_LIFECYCLE => self.lifecycle,
            FAMILY_ADAPTER => self.adapter,
            _ => false,
        }
    }

    /// Queue an event. `make` runs only if the queue is live. Never blocks and never waits.
    /// Returns whether the event was queued.
    pub fn offer(&self, critical: bool, bytes: usize, make: impl FnOnce() -> Payload) -> bool {
        {
            let mut state = self.state.lock();
            if !state.live {
                state.dropped += 1;
                state.dropped_total += 1;
                return false;
            }
            let ticket = state.ticket;
            state.ticket += 1;
            let queued = Queued {
                ticket,
                payload: make(),
                bytes,
            };
            state.bytes += bytes;
            if critical {
                state.critical.push_back(queued);
            } else {
                state.normal.push_back(queued);
            }
            // the critical part has its own room
            while state.critical.len() > self.limits.critical {
                if let Some(old) = state.critical.pop_front() {
                    state.bytes -= old.bytes;
                    state.dropped += 1;
                    state.dropped_total += 1;
                }
            }
            // the normal part loses its oldest first; the byte cap takes critical ones only when
            // no normal one is left
            while state.normal.len() > self.limits.normal
                || (state.bytes > self.limits.bytes && !state.normal.is_empty())
            {
                if let Some(old) = state.normal.pop_front() {
                    state.bytes -= old.bytes;
                    state.dropped += 1;
                    state.dropped_total += 1;
                }
            }
            while state.bytes > self.limits.bytes && state.critical.len() > 1 {
                if let Some(old) = state.critical.pop_front() {
                    state.bytes -= old.bytes;
                    state.dropped += 1;
                    state.dropped_total += 1;
                }
            }
        }
        self.wake.notify_one();
        true
    }

    /// Take the oldest event (critical and normal are one sequence in arrival order).
    pub fn take(&self) -> Option<Taken> {
        let mut state = self.state.lock();
        let from_critical = match (state.critical.front(), state.normal.front()) {
            (None, None) => return None,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(c), Some(n)) => c.ticket < n.ticket,
        };
        let queued = if from_critical {
            state.critical.pop_front()
        } else {
            state.normal.pop_front()
        }?;
        state.bytes -= queued.bytes;
        state.in_progress += 1;
        let dropped = std::mem::take(&mut state.dropped);
        Some(Taken {
            payload: queued.payload,
            dropped_before: (dropped > 0).then_some(dropped),
        })
    }

    /// Wait for an event.
    pub async fn next(&self) -> Taken {
        loop {
            if let Some(taken) = self.take() {
                return taken;
            }
            self.wake.notified().await;
        }
    }

    /// The pump has written (or given up on) the event it took.
    pub fn finished(&self) {
        let mut state = self.state.lock();
        state.in_progress = state.in_progress.saturating_sub(1);
    }

    /// An event that was taken could not be sent: it is part of the gap.
    pub fn note_dropped(&self, n: u64) {
        let mut state = self.state.lock();
        state.dropped += n;
        state.dropped_total += n;
    }

    /// Events waiting, and the one the pump is writing.
    pub fn pending(&self) -> usize {
        let state = self.state.lock();
        state.normal.len() + state.critical.len() + state.in_progress
    }

    /// Everything dropped over the life of the queue.
    pub fn dropped_total(&self) -> u64 {
        self.state.lock().dropped_total
    }

    /// Begin queuing. [`ServiceQueue::attach`] does this; a test that drives a queue by hand may too.
    pub fn go_live(&self) {
        self.state.lock().live = true;
    }

    /// Stop queuing; what is queued stays to be written.
    fn stop_accepting(&self) {
        self.state.lock().live = false;
    }

    /// The connection is gone: what was queued is dropped (and counted), not kept for another
    /// process.
    pub fn go_down(&self) {
        let mut state = self.state.lock();
        state.live = false;
        let lost = (state.normal.len() + state.critical.len()) as u64;
        state.normal.clear();
        state.critical.clear();
        state.bytes = 0;
        state.in_progress = 0;
        state.dropped += lost;
        state.dropped_total += lost;
    }

    /// Start writing this queue's events to `conn`. The queue takes events from now until the
    /// returned [`Pump`] ends.
    pub fn attach(self: &Arc<Self>, conn: Arc<AdapterConn>) -> Pump {
        self.go_live();
        let queue = Arc::clone(self);
        let task = tokio::spawn(pump(queue, conn));
        Pump {
            task,
            queue: Arc::clone(self),
        }
    }
}

/// The task that turns queued events into `event` frames. It ends when the connection does.
async fn pump(queue: Arc<ServiceQueue>, conn: Arc<AdapterConn>) {
    loop {
        let taken = tokio::select! {
            taken = queue.next() => taken,
            _ = conn.closed() => return,
        };
        let params = match &taken.payload {
            Payload::Lifecycle(ev) => lifecycle_params(ev, &queue.name, taken.dropped_before),
            Payload::Adapter(ev) => adapter_params(ev, taken.dropped_before),
        };
        let Some(params) = fit_event_line(params, queue.limits.event_line) else {
            queue.note_dropped(1 + taken.dropped_before.unwrap_or(0));
            queue.finished();
            continue;
        };
        loop {
            match conn.notify(method::EVENT, &params) {
                Ok(()) => break,
                // the adapter is slow to read: wait, the queue behind this one is bounded
                Err(CallError::Busy) => {
                    tokio::select! {
                        _ = tokio::time::sleep(BUSY_PAUSE) => {}
                        _ = conn.closed() => return,
                    }
                }
                Err(CallError::TooLarge) => {
                    queue.note_dropped(1 + taken.dropped_before.unwrap_or(0));
                    break;
                }
                Err(_) => return,
            }
        }
        queue.finished();
    }
}

/// The pump of one connection. Dropping it stops the task, makes the queue refuse events and
/// drops what is still queued.
pub struct Pump {
    task: JoinHandle<()>,
    queue: Arc<ServiceQueue>,
}

impl Pump {
    /// The planned end: refuse new events, give the pump up to `wait` to write what is queued,
    /// then stop. The frames go out before the `shutdown` request that follows, in the same
    /// order, because they share the connection's outgoing queue.
    pub async fn finish(self, wait: Duration) {
        self.queue.stop_accepting();
        let queue = Arc::clone(&self.queue);
        let _ = tokio::time::timeout(wait, async {
            while queue.pending() > 0 && !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.task.abort();
        self.queue.go_down();
    }
}

// ---------------------------------------------------------------------------------------------
// Building the frames (in the pump, not on the actor's task)
// ---------------------------------------------------------------------------------------------

/// The wire name of a lifecycle kind.
pub fn kind_name(kind: LifecycleKind) -> &'static str {
    match kind {
        LifecycleKind::Started => "started",
        LifecycleKind::Restarted => "restarted",
        LifecycleKind::Stopped => "stopped",
        LifecycleKind::Crashed => "crashed",
        LifecycleKind::Errored => "errored",
        LifecycleKind::Unhealthy => "unhealthy",
        LifecycleKind::Recovered => "recovered",
        LifecycleKind::BackoffScheduled => "backoff",
        LifecycleKind::CrashLoop => "crash_loop",
        LifecycleKind::CronFired => "cron_fired",
        LifecycleKind::MemoryLimit => "memory_limit",
        LifecycleKind::UptimeLimit => "uptime_limit",
    }
}

fn cause_name(cause: Cause) -> &'static str {
    match cause {
        Cause::User => "user",
        Cause::Resurrect => "resurrect",
        Cause::Reload => "reload",
        Cause::Group => "group",
        Cause::Crash => "crash",
        Cause::Limit => "limit",
        Cause::Cron => "cron",
        Cause::Health => "health",
        Cause::Adapter => "adapter",
    }
}

/// The events a restart-minded service must not lose.
pub fn lifecycle_is_critical(kind: LifecycleKind) -> bool {
    matches!(
        kind,
        LifecycleKind::Crashed
            | LifecycleKind::Errored
            | LifecycleKind::Unhealthy
            | LifecycleKind::CrashLoop
            | LifecycleKind::MemoryLimit
    )
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `text` cut to at most `max` characters.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// The frame params of a lifecycle event for the service `adapter`: only the allow-listed
/// fields, the `reason` scrubbed against the app's environment values, and the service's own
/// `adapters.<adapter>` options (and no other adapter's).
pub fn lifecycle_params(
    ev: &LifecycleEvent,
    adapter: &str,
    dropped_before: Option<u64>,
) -> EventParams {
    let detail = &ev.detail;
    let mut data = Map::new();
    data.insert("cause".into(), json!(cause_name(ev.cause)));
    if let Some(actor) = &ev.actor {
        data.insert("actor".into(), json!(actor.as_ref()));
    }
    if let Some(pid) = detail.pid {
        data.insert("pid".into(), json!(pid));
    }
    if let Some(code) = detail.exit_code {
        data.insert("exit_code".into(), json!(code));
    }
    if let Some(signal) = detail.exit_signal {
        data.insert("exit_signal".into(), json!(signal));
    }
    data.insert("restarts".into(), json!(detail.restarts));
    if let Some((memory_mb, limit_mb)) = detail.memory_mb {
        data.insert("memory_mb".into(), json!(memory_mb));
        data.insert("limit_mb".into(), json!(limit_mb));
    }
    if let Some(reason) = &detail.reason {
        data.insert(
            "reason".into(),
            json!(clip(&scrub_text(reason, &detail.spec), REASON_MAX_CHARS)),
        );
    }
    let options = match detail.spec.adapters.get(adapter) {
        Some(Value::Object(map))
            if Value::Object(map.clone()).to_string().len() <= MAX_ADAPTER_OPTIONS_BYTES =>
        {
            Some(map.clone())
        }
        _ => None,
    };
    EventParams {
        seq: ev.seq,
        kind: kind_name(ev.kind).to_string(),
        family: FAMILY_LIFECYCLE.to_string(),
        at: rfc3339(ev.at),
        app: Some(EventApp {
            id: ev.app_id,
            name: ev.app.to_string(),
            tags: detail.spec.tags.clone(),
        }),
        data,
        options,
        dropped_before,
    }
}

/// The frame params of an `adapter` event.
pub fn adapter_params(ev: &AdapterEvent, dropped_before: Option<u64>) -> EventParams {
    let mut data = Map::new();
    data.insert("adapter".into(), json!(ev.adapter));
    if let Some(version) = &ev.version {
        data.insert("version".into(), json!(version));
    }
    if let Some(reason) = &ev.reason {
        data.insert("reason".into(), json!(clip(reason, REASON_MAX_CHARS)));
    }
    EventParams {
        seq: ev.seq,
        kind: ev.kind.clone(),
        family: FAMILY_ADAPTER.to_string(),
        at: rfc3339(ev.at),
        app: None,
        data,
        options: None,
        dropped_before,
    }
}

/// Make the event fit `limit` bytes as a line: first the options go, then the reason, then the
/// tags. `None` when it still does not fit (it is then dropped and counted).
pub fn fit_event_line(mut params: EventParams, limit: usize) -> Option<EventParams> {
    let size = |p: &EventParams| {
        serde_json::to_vec(p)
            .map(|v| v.len() + ENVELOPE_BYTES)
            .unwrap_or(usize::MAX)
    };
    if size(&params) <= limit {
        return Some(params);
    }
    params.options = None;
    if size(&params) <= limit {
        return Some(params);
    }
    params.data.remove("reason");
    if size(&params) <= limit {
        return Some(params);
    }
    if let Some(app) = params.app.as_mut() {
        app.tags.clear();
    }
    (size(&params) <= limit).then_some(params)
}

/// A rough size of an event in a queue, for the byte cap.
fn lifecycle_bytes(ev: &LifecycleEvent) -> usize {
    256 + ev.app.len()
        + ev.actor.as_ref().map_or(0, |a| a.len())
        + ev.detail.reason.as_ref().map_or(0, |r| r.len())
        + ev.detail.spec.tags.iter().map(String::len).sum::<usize>()
}

fn adapter_bytes(ev: &AdapterEvent) -> usize {
    192 + ev.kind.len()
        + ev.adapter.len()
        + ev.version.as_ref().map_or(0, String::len)
        + ev.reason.as_ref().map_or(0, String::len)
}

// ---------------------------------------------------------------------------------------------
// The hub and the sink
// ---------------------------------------------------------------------------------------------

/// The queues of all subscribed services.
pub struct EventHub {
    queues: RwLock<BTreeMap<String, Arc<ServiceQueue>>>,
    lifecycle_subscribers: AtomicUsize,
    adapter_subscribers: AtomicUsize,
    adapter_seq: AtomicU64,
    limits: QueueLimits,
}

impl EventHub {
    pub fn new() -> Arc<EventHub> {
        Self::with_limits(QueueLimits::protocol1())
    }

    pub fn with_limits(limits: QueueLimits) -> Arc<EventHub> {
        Arc::new(EventHub {
            queues: RwLock::new(BTreeMap::new()),
            lifecycle_subscribers: AtomicUsize::new(0),
            adapter_subscribers: AtomicUsize::new(0),
            adapter_seq: AtomicU64::new(0),
            limits,
        })
    }

    /// Subscribe the service `name` to `families` (names this protocol does not know are
    /// ignored). Returns its queue, or `None` when no known family is left. A second call for
    /// the same name replaces the first.
    pub fn register(
        &self,
        name: &str,
        families: &[String],
        scope: HostScope,
    ) -> Option<Arc<ServiceQueue>> {
        let known: Vec<String> = families
            .iter()
            .filter(|f| EVENT_FAMILIES.contains(&f.as_str()))
            .cloned()
            .collect();
        let mut queues = self.queues.write();
        let queue = if known.is_empty() {
            queues.remove(name);
            None
        } else {
            let queue = ServiceQueue::new(name, &known, scope, self.limits);
            queues.insert(name.to_string(), Arc::clone(&queue));
            Some(queue)
        };
        self.lifecycle_subscribers.store(
            queues.values().filter(|q| q.lifecycle).count(),
            Ordering::Relaxed,
        );
        self.adapter_subscribers.store(
            queues.values().filter(|q| q.adapter).count(),
            Ordering::Relaxed,
        );
        queue
    }

    pub fn queue(&self, name: &str) -> Option<Arc<ServiceQueue>> {
        self.queues.read().get(name).cloned()
    }

    /// Offer a lifecycle event to subscribers whose grants cover the app.
    pub fn publish_lifecycle(&self, ev: &LifecycleEvent) {
        if self.lifecycle_subscribers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let critical = lifecycle_is_critical(ev.kind);
        let bytes = lifecycle_bytes(ev);
        let mut shared: Option<Arc<LifecycleEvent>> = None;
        for queue in self.queues.read().values().filter(|q| q.lifecycle) {
            if queue.scope == HostScope::OptedIn
                && !ev.detail.spec.adapters.contains_key(queue.name())
            {
                continue;
            }
            queue.offer(critical, bytes, || {
                Payload::Lifecycle(Arc::clone(
                    shared.get_or_insert_with(|| Arc::new(ev.clone())),
                ))
            });
        }
    }

    /// Offer an `adapter` event about `about` to every service that listens, except `about`
    /// itself.
    pub fn publish_adapter(
        &self,
        about: &str,
        kind: &str,
        version: Option<&str>,
        reason: Option<&str>,
    ) {
        if self.adapter_subscribers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let queues = self.queues.read();
        if !queues.values().any(|q| q.adapter && q.name != about) {
            return;
        }
        let event = Arc::new(AdapterEvent {
            seq: self.adapter_seq.fetch_add(1, Ordering::SeqCst) + 1,
            at: Utc::now(),
            kind: kind.to_string(),
            adapter: about.to_string(),
            version: version.map(str::to_string),
            reason: reason.map(str::to_string),
        });
        let (critical, bytes) = (adapter_event_is_critical(kind), adapter_bytes(&event));
        for queue in queues.values().filter(|q| q.adapter && q.name != about) {
            queue.offer(critical, bytes, || Payload::Adapter(Arc::clone(&event)));
        }
    }
}

/// The sink the supervisor is given at construction, before the adapter host exists. The daemon
/// binds the hub once the host is loaded; until then (and in a daemon with no service listening)
/// it does nothing.
#[derive(Default)]
pub struct AdapterEventSink {
    hub: std::sync::OnceLock<Arc<EventHub>>,
}

impl AdapterEventSink {
    pub fn new() -> Arc<AdapterEventSink> {
        Arc::new(AdapterEventSink::default())
    }

    /// Connect the sink to the hub. Only the first call counts.
    pub fn bind(&self, hub: Arc<EventHub>) {
        let _ = self.hub.set(hub);
    }
}

impl EventSink for AdapterEventSink {
    fn on_event(&self, ev: &LifecycleEvent) {
        if let Some(hub) = self.hub.get() {
            hub.publish_lifecycle(ev);
        }
    }
}
