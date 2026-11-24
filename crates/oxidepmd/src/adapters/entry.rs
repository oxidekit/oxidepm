//! One adapter's state machine and its supervision.
//!
//! ```text
//! Disabled -- enable --> Stopped -- start --> Starting -- initialized --> Ready
//! Ready -- unplanned exit, kill for violation --> Backoff(attempt) -- timer --> Starting
//! Ready -- two consecutive deadline expiries on launch-path calls --> Degraded -- one probe succeeds --> Ready
//! Backoff/Ready -- CrashTracker gives up --> Failed{reason}
//! any -- hash differs / no protocol overlap --> Failed{tampered | incompatible}
//! tool: Ready -- idle --> Stopping --> Stopped (a planned exit, not a crash)
//! ```
//!
//! Everything that waits is bounded by a timer owned here, never by the adapter: a call waits for
//! readiness at most `ready_wait`, an `initialize` at most `init_timeout`, and the circuit
//! breaker turns a hung runtime adapter into immediate `E_ADAPTER_UNAVAILABLE` after two expiries.
//! The state lock is a `parking_lot` mutex held for field updates only, never across an await.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oxidepm_adapter_proto::codes::rpc;
use oxidepm_adapter_proto::messages::{
    method, Grant, InitPaths, InitializeParams, InitializeResult, InvokeParams, InvokeResult,
    NameVersion,
};
use oxidepm_adapter_proto::permissions::SpawnPerm;
use oxidepm_adapter_proto::{Kind, Params, Permissions, Verb, SUPPORTED_PROTOCOLS};
use oxidepm_core::restart::{CrashTracker, Decision, ExitCause};
use oxidepm_core::{Error, RestartMode, RestartPolicy};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::conn::{AdapterConn, CallError, CloseReason, ConnConfig, Inbound, Output};
use super::events::{EventHub, ServiceQueue, FLUSH_WAIT};
use super::proc::{dir_usage, AdapterProc, Launch, ProcError, ShutdownTimings};
use crate::audit::{AdapterLine, Audit};
use crate::supervisor::{group_usage, PidTable};

/// How long the connection's end waits for the process to exit by itself.
const END_WAIT: Duration = Duration::from_millis(500);
/// The same for a sandboxed adapter, whose launcher exits at once when it refuses (status 126).
const LAUNCHER_EXIT_WAIT: Duration = Duration::from_secs(3);
/// How long a process that has exited is given for its last stderr lines to be read.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// The calls on the launch path: the ones the circuit breaker watches.
pub fn is_launch_path(method: &str) -> bool {
    matches!(method, method::RUNTIME_PREPARE | method::RUNTIME_PLAN)
}

/// Why an adapter is `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The executable or the handshake does not match what was imported.
    Tampered,
    /// No protocol in common with the daemon.
    Incompatible,
    /// The sandbox could not be set up.
    Sandbox,
    /// The crash tracker gave up.
    CrashLoop,
    /// The installed files are missing.
    NeedsReinstall,
    /// A task of the host panicked or the manifest is no longer valid.
    Internal,
}

impl FailReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailReason::Tampered => "tampered",
            FailReason::Incompatible => "incompatible",
            FailReason::Sandbox => "sandbox",
            FailReason::CrashLoop => "crash_loop",
            FailReason::NeedsReinstall => "needs_reinstall",
            FailReason::Internal => "internal",
        }
    }

    /// A failed tool is tried once more after a while, unless the cause cannot go away by itself.
    fn retryable(&self) -> bool {
        matches!(self, FailReason::CrashLoop)
    }
}

/// Where an adapter stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Disabled,
    Stopped,
    Starting,
    Ready,
    /// Waiting `delay_ms` before start number `attempt` + 1.
    Backoff {
        attempt: u32,
        delay_ms: u64,
    },
    /// Ready, but the circuit breaker is open: calls fail at once.
    Degraded,
    Stopping,
    Failed(FailReason),
}

impl EntryState {
    pub fn name(&self) -> &'static str {
        match self {
            EntryState::Disabled => "disabled",
            EntryState::Stopped => "stopped",
            EntryState::Starting => "starting",
            EntryState::Ready => "ready",
            EntryState::Backoff { .. } => "backoff",
            EntryState::Degraded => "degraded",
            EntryState::Stopping => "stopping",
            EntryState::Failed(_) => "failed",
        }
    }
}

/// Circuit breaker for the launch path. Pure: time is passed in.
///
/// Two consecutive failures open it for `open_for`. Calls are rejected while it is open. After
/// that, one call is let through as a probe; if it succeeds the breaker closes, otherwise it
/// opens again for another `open_for`. Other calls are rejected while the probe is out.
#[derive(Debug, Clone)]
pub struct Breaker {
    threshold: u32,
    open_for: Duration,
    failures: u32,
    open_until: Option<Instant>,
    probing: bool,
}

/// What the breaker says about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Go,
    Probe,
    Reject,
}

impl Breaker {
    pub fn new(threshold: u32, open_for: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            open_for,
            failures: 0,
            open_until: None,
            probing: false,
        }
    }

    pub fn admit(&mut self, now: Instant) -> Admit {
        match self.open_until {
            None => Admit::Go,
            Some(until) if now < until => Admit::Reject,
            Some(_) if self.probing => Admit::Reject,
            Some(_) => {
                self.probing = true;
                Admit::Probe
            }
        }
    }

    pub fn success(&mut self) {
        self.failures = 0;
        self.open_until = None;
        self.probing = false;
    }

    pub fn failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.probing || self.failures >= self.threshold {
            self.open_until = Some(now + self.open_for);
            self.probing = false;
        }
    }

    pub fn is_open(&self, now: Instant) -> bool {
        self.open_until
            .is_some_and(|until| now < until || self.probing)
    }

    /// A probe that was let through but never answered (the call was dropped): allow another.
    pub fn probe_abandoned(&mut self) {
        self.probing = false;
    }
}

/// Adapter supervision settings. [`EntryConfig::production`] holds the production values. Tests inject
/// shorter ones.
#[derive(Debug, Clone)]
pub struct EntryConfig {
    pub conn: ConnConfig,
    pub init_timeout: Duration,
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    pub ping_misses: u32,
    /// A tool with nothing to do for this long is asked to exit.
    pub tool_idle: Duration,
    pub shutdown: ShutdownTimings,
    pub restart: RestartPolicy,
    /// How long the breaker stays open, and how many consecutive failures open it.
    pub breaker_open: Duration,
    pub breaker_threshold: u32,
    /// A failed tool is retried once after this long.
    pub failed_tool_retry: Duration,
    /// A call waits at most this long for the adapter to be ready.
    pub ready_wait: Duration,
    /// How often the adapter's whole process tree is measured (memory and threads).
    pub monitor_interval: Duration,
    /// How often the adapter's state directory is measured.
    pub quota_interval: Duration,
    /// The state directory may hold this many bytes before the adapter is disabled.
    pub state_quota_bytes: u64,
    /// Processes and threads allowed in the adapter's tree before it is killed.
    pub max_threads: u32,
    /// Relayed verbs one adapter runs at the same time; the next one is refused as busy.
    pub invoke_slots: usize,
    /// A `command.invoke` that reports nothing for this long is cut off.
    pub invoke_idle: Duration,
}

impl EntryConfig {
    pub fn production() -> Self {
        Self {
            conn: ConnConfig::protocol1(),
            init_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            ping_timeout: Duration::from_secs(5),
            ping_misses: 3,
            tool_idle: Duration::from_secs(30),
            shutdown: ShutdownTimings::STANDARD,
            restart: RestartPolicy {
                auto_restart: true,
                max_restarts: 5,
                restart_delay_ms: 1000,
                crash_window_secs: 600,
                max_restart_delay_ms: 60_000,
                stable_uptime_secs: 60,
            },
            breaker_open: Duration::from_secs(30),
            breaker_threshold: 2,
            failed_tool_retry: Duration::from_secs(300),
            ready_wait: Duration::from_secs(10),
            monitor_interval: Duration::from_secs(2),
            quota_interval: Duration::from_secs(60),
            state_quota_bytes: STATE_QUOTA_BYTES,
            max_threads: MAX_THREADS,
            invoke_slots: INVOKE_SLOTS,
            invoke_idle: INVOKE_IDLE,
        }
    }
}

/// The adapter's state directory may hold this much.
pub const STATE_QUOTA_BYTES: u64 = 64 * 1024 * 1024;
/// Processes and threads in the adapter's tree.
pub const MAX_THREADS: u32 = 64;
/// Relayed verbs one adapter runs at once.
pub const INVOKE_SLOTS: usize = 4;
/// A relayed verb that has said nothing for this long is timed out.
pub const INVOKE_IDLE: Duration = Duration::from_secs(120);

/// What it takes to build the sandbox profile of one call: the permissions the user granted, and
/// the paths the profile is made against. A tool's relayed verb runs in a process of its own with
/// a profile expanded for the call's `$CWD`.
#[derive(Clone)]
pub struct ProfileRecipe {
    pub permissions: Permissions,
    pub daemon_home: std::path::PathBuf,
    pub search_path: Vec<std::path::PathBuf>,
}

/// What a runtime adapter provides and the core's rules for running its words.
#[derive(Clone)]
pub struct RuntimeProvides {
    /// `provides.runtimes`.
    pub runtimes: Vec<String>,
    pub rules: oxidepm_runtime::step::RuntimeRules,
}

/// What the host knows about one installed adapter.
#[derive(Clone)]
pub struct EntrySpec {
    pub name: String,
    pub version: String,
    pub kind: Kind,
    pub launch: Launch,
    pub grant: Grant,
    /// Identifies the adapter in `adapter-pids.json`.
    pub slot: u32,
    /// The verbs the manifest declares.
    pub verbs: Vec<Verb>,
    /// How to make the profile of a call; `None` when nothing is sandboxed.
    pub recipe: Option<ProfileRecipe>,
    /// The runtimes the adapter provides, with the rules for what the core runs on its word.
    pub runtime: Option<RuntimeProvides>,
    /// The manifest says the adapter acts on behalf of an AI.
    pub ai_driven: bool,
}

/// One relayed verb to run, checked and resolved by the host.
#[derive(Debug, Clone)]
pub struct InvokeCall {
    pub params: InvokeParams,
    /// The verb's `max_runtime`.
    pub max_runtime: Duration,
    /// The verb is `safe`: its process may write only to the adapter's state directory.
    pub restrict_writes: bool,
    /// The process-table slot of this call's process (a tool runs in a process of its own).
    pub slot: u32,
}

/// One session to run: a verb that owns the terminal, resolved and checked by the host.
pub struct SessionCall {
    /// The verb, as for a relayed call. Its `max_runtime` is the longest the session may last.
    pub call: InvokeCall,
    /// The terminal the CLI handed over: the adapter's standard input and output.
    pub terminal: (std::os::fd::OwnedFd, std::os::fd::OwnedFd),
    /// Where the adapter connects, the file that names it, and the single-use token it presents.
    pub listener: tokio::net::UnixListener,
    pub socket: std::path::PathBuf,
    pub token: String,
    /// The adapter must have connected by then.
    pub deadline: tokio::time::Instant,
    /// The CLI's terminal changed size: the adapter's process group gets `SIGWINCH`.
    pub winch: mpsc::UnboundedReceiver<()>,
}

/// A snapshot for `DaemonStatus` and tests.
#[derive(Debug, Clone)]
pub struct EntryView {
    pub name: String,
    pub kind: Kind,
    pub state: EntryState,
    pub note: Option<String>,
    pub pid: Option<u32>,
    /// Unplanned exits recorded by the crash tracker.
    pub crashes: u32,
    /// Strikes the current connection has recorded.
    pub strikes: u64,
    /// Stderr lines dropped for exceeding the rate.
    pub stderr_dropped: u64,
}

struct Inner {
    state: EntryState,
    note: Option<String>,
    conn: Option<Arc<super::conn::AdapterConn>>,
    pid: Option<u32>,
    tracker: CrashTracker,
    crashes: u32,
    breaker: Breaker,
    /// Calls that hold the connection right now (a tool is not idle while any does).
    active: usize,
    last_used: Instant,
    stop_tx: Option<watch::Sender<Stop>>,
    task: Option<JoinHandle<()>>,
    /// A failed tool may be tried again at this time, once.
    retry_at: Option<Instant>,
    retry_used: bool,
    stderr_tail: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    stderr_dropped: Option<Arc<std::sync::atomic::AtomicU64>>,
}

/// What the owner asked of a running adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    No,
    /// Request shutdown before terminating the adapter.
    Graceful,
    /// End it now, with no grace.
    Now,
}

/// How one process life ended.
enum Life {
    /// Stopped on request or idle: not a crash.
    Planned,
    /// The state is already `Failed`.
    Failed,
    /// The process ended by itself or was killed for a violation.
    Crashed { ran_for: Duration, cause: ExitCause },
}

/// One adapter: its state, its process and its connection.
pub struct AdapterEntry {
    spec: EntrySpec,
    cfg: EntryConfig,
    pids: Arc<PidTable>,
    inbound: Arc<dyn Inbound>,
    audit: Option<Arc<Audit>>,
    inner: Mutex<Inner>,
    state_tx: watch::Sender<EntryState>,
    /// Where this adapter's events go and come from; unset until the host binds it.
    events: std::sync::OnceLock<EventLink>,
    /// The host replaced or removed this entry: what it does from now on is not written to the
    /// adapter's row.
    detached: std::sync::atomic::AtomicBool,
    /// Ends the processes that relayed verbs started for single calls; renewed after use.
    oneshots: Mutex<CancellationToken>,
    /// Relayed verbs running now, at most `cfg.invoke_slots`.
    slots: Arc<tokio::sync::Semaphore>,
}

/// The event hub, and this adapter's own queue when it is a service that listens.
struct EventLink {
    hub: Arc<EventHub>,
    queue: Option<Arc<ServiceQueue>>,
}

impl AdapterEntry {
    /// An entry that is not running. `enabled` false starts it `Disabled`.
    pub fn new(
        spec: EntrySpec,
        cfg: EntryConfig,
        pids: Arc<PidTable>,
        inbound: Arc<dyn Inbound>,
        audit: Option<Arc<Audit>>,
        enabled: bool,
    ) -> Arc<AdapterEntry> {
        let state = if enabled {
            EntryState::Stopped
        } else {
            EntryState::Disabled
        };
        let inner = Inner {
            state,
            note: None,
            conn: None,
            pid: None,
            tracker: CrashTracker::new(cfg.restart.clone()),
            crashes: 0,
            breaker: Breaker::new(cfg.breaker_threshold, cfg.breaker_open),
            active: 0,
            last_used: Instant::now(),
            stop_tx: None,
            task: None,
            retry_at: None,
            retry_used: false,
            stderr_tail: None,
            stderr_dropped: None,
        };
        let slots = Arc::new(tokio::sync::Semaphore::new(cfg.invoke_slots.max(1)));
        Arc::new(AdapterEntry {
            spec,
            cfg,
            pids,
            inbound,
            audit,
            inner: Mutex::new(inner),
            state_tx: watch::channel(state).0,
            events: std::sync::OnceLock::new(),
            detached: std::sync::atomic::AtomicBool::new(false),
            oneshots: Mutex::new(CancellationToken::new()),
            slots,
        })
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn kind(&self) -> Kind {
        self.spec.kind
    }

    /// The version this entry runs.
    pub fn version(&self) -> &str {
        &self.spec.version
    }

    /// Mark the entry as replaced or removed: the host stops recording its state in the row.
    pub fn detach(&self) {
        self.detached
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_detached(&self) -> bool {
        self.detached.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// What the user allowed this adapter (its level and scope).
    pub fn grant(&self) -> crate::policy::AdapterGrant {
        crate::policy::AdapterGrant {
            level: self.spec.grant.level,
            scope: self.spec.grant.scope,
        }
    }

    /// The state, with `Degraded` when the breaker is open on a ready adapter.
    pub fn state(&self) -> EntryState {
        let inner = self.inner.lock();
        Self::visible_state(&inner)
    }

    fn visible_state(inner: &Inner) -> EntryState {
        if inner.state == EntryState::Ready && inner.breaker.is_open(Instant::now()) {
            EntryState::Degraded
        } else {
            inner.state
        }
    }

    pub fn view(&self) -> EntryView {
        let inner = self.inner.lock();
        EntryView {
            name: self.spec.name.clone(),
            kind: self.spec.kind,
            state: Self::visible_state(&inner),
            note: inner.note.clone(),
            pid: inner.pid,
            crashes: inner.crashes,
            strikes: inner
                .conn
                .as_ref()
                .map(|c| c.counters().strikes)
                .unwrap_or(0),
            stderr_dropped: inner
                .stderr_dropped
                .as_ref()
                .map(|d| d.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0),
        }
    }

    /// The last lines the adapter wrote to stderr (not yet scrubbed).
    pub fn stderr_tail(&self) -> Vec<String> {
        let tail = self.inner.lock().stderr_tail.clone();
        tail.map(|t| t.lock().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Follow the state.
    pub fn subscribe(&self) -> watch::Receiver<EntryState> {
        self.state_tx.subscribe()
    }

    fn set_state(&self, inner: &mut Inner, state: EntryState) {
        inner.state = state;
        self.state_tx.send_replace(state);
    }

    fn line(&self, event: &str) {
        if let Some(audit) = &self.audit {
            audit.adapter_line(AdapterLine {
                name: self.spec.name.clone(),
                event: event.to_string(),
                version: Some(self.spec.version.clone()),
                sha12: Some(self.spec.launch.exe_sha256.chars().take(12).collect()),
            });
        }
    }

    /// Connect the adapter to the event hub. A service that lists `families` gets a queue there
    /// and receives the events of those families while it runs; every adapter can have `adapter`
    /// events published about it. Only the first call counts.
    pub fn bind_events(&self, hub: &Arc<EventHub>, families: &[String]) {
        let queue = if self.spec.kind == Kind::Service {
            hub.register(&self.spec.name, families, self.spec.grant.scope)
        } else {
            None
        };
        let _ = self.events.set(EventLink {
            hub: Arc::clone(hub),
            queue,
        });
    }

    /// Tell the services that listen to the `adapter` family what happened to this adapter.
    fn announce(&self, kind: &str, reason: Option<&str>) {
        if let Some(link) = self.events.get() {
            link.hub
                .publish_adapter(&self.spec.name, kind, Some(&self.spec.version), reason);
        }
    }

    /// An audit line and the `adapter` event of the same name.
    fn happened(&self, event: &'static str) {
        self.line(event);
        self.announce(event, None);
    }

    /// Enter `Failed`. A failed tool that crashed too often may be tried again once.
    fn fail(&self, reason: FailReason, note: String) {
        warn!(
            "adapter '{}' failed ({}): {}",
            self.spec.name,
            reason.as_str(),
            note
        );
        self.line(reason.as_str());
        self.announce("failed", Some(reason.as_str()));
        let mut inner = self.inner.lock();
        inner.note = Some(note);
        inner.conn = None;
        inner.pid = None;
        inner.stop_tx = None;
        if self.spec.kind == Kind::Tool && reason.retryable() && !inner.retry_used {
            inner.retry_at = Some(Instant::now() + self.cfg.failed_tool_retry);
        } else {
            inner.retry_at = None;
        }
        self.set_state(&mut inner, EntryState::Failed(reason));
    }

    // ---- starting and stopping ---------------------------------------------------------------

    /// Start the supervision if the adapter is stopped. A service calls this at boot and at
    /// enable; a tool is started by its first call.
    pub fn start(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        if inner.state == EntryState::Stopped {
            self.begin(&mut inner);
        }
    }

    /// Make an enabled adapter that was `Disabled` or `Failed` startable again, with a fresh crash
    /// history (an operator `enable`).
    pub fn enable(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        if matches!(inner.state, EntryState::Disabled | EntryState::Failed(_)) {
            inner.tracker.reset();
            inner.retry_at = None;
            inner.retry_used = false;
            inner.note = None;
            inner.breaker.success();
            self.set_state(&mut inner, EntryState::Stopped);
        }
    }

    /// Spawn the supervision task. The state becomes `Starting`.
    fn begin(self: &Arc<Self>, inner: &mut Inner) {
        let (stop_tx, stop_rx) = watch::channel(Stop::No);
        inner.stop_tx = Some(stop_tx);
        self.set_state(inner, EntryState::Starting);
        let entry = Arc::clone(self);
        // The watcher: a panic in the supervision ends this adapter and nothing else. The task's
        // `AdapterProc` is dropped while unwinding, and dropping it kills the process group.
        let handle = tokio::spawn(async move {
            let run = tokio::spawn(Arc::clone(&entry).supervise(stop_rx));
            if let Err(e) = run.await {
                if e.is_panic() {
                    entry.fail(
                        FailReason::Internal,
                        "the supervision of the adapter panicked".to_string(),
                    );
                }
            }
        });
        inner.task = Some(handle);
    }

    /// Ask the adapter to stop and wait for it. This does not count as a crash. The
    /// adapter ends `Stopped`, or `Disabled` when `disable` is set.
    pub async fn stop(&self, disable: bool) {
        self.cancel_oneshots();
        let (stop_tx, task) = {
            let mut inner = self.inner.lock();
            match inner.state {
                EntryState::Disabled => return,
                EntryState::Stopped | EntryState::Failed(_) => {
                    if disable {
                        self.set_state(&mut inner, EntryState::Disabled);
                    }
                    return;
                }
                _ => {}
            }
            (inner.stop_tx.clone(), inner.task.take())
        };
        if let Some(tx) = stop_tx {
            tx.send_if_modified(|s| {
                if *s == Stop::No {
                    *s = Stop::Graceful;
                    true
                } else {
                    false
                }
            });
        }
        if let Some(mut task) = task {
            let bound = self.cfg.shutdown.total()
                + self.cfg.init_timeout.min(Duration::from_secs(5))
                + Duration::from_secs(5);
            if tokio::time::timeout(bound, &mut task).await.is_err() {
                // dropping the process kills its group
                task.abort();
                let mut inner = self.inner.lock();
                inner.conn = None;
                inner.pid = None;
                self.set_state(&mut inner, EntryState::Stopped);
            }
        }
        if disable {
            let mut inner = self.inner.lock();
            if !matches!(inner.state, EntryState::Failed(_)) {
                self.set_state(&mut inner, EntryState::Disabled);
            }
        }
    }

    /// Put the entry in `Failed` from outside (the host found its record unusable at load).
    pub fn mark_failed(&self, reason: FailReason, note: String) {
        self.fail(reason, note);
    }

    /// Kill the process tree at once without waiting for anything: the second signal of a
    /// shutdown. The supervision, if it is running, sees the order and ends.
    pub fn kill_signal(&self) {
        self.cancel_oneshots();
        let (stop_tx, pid) = {
            let inner = self.inner.lock();
            (inner.stop_tx.clone(), inner.pid)
        };
        if let Some(tx) = stop_tx {
            tx.send_replace(Stop::Now);
        }
        if let Some(pid) = pid {
            super::proc::kill_walk(pid as i32, pid as i32);
        }
    }

    /// End the adapter at once, with no grace: the second signal of a shutdown, and tools at
    /// shutdown. Everything under it is killed.
    pub async fn kill_now(&self) {
        self.cancel_oneshots();
        let (stop_tx, task) = {
            let mut inner = self.inner.lock();
            (inner.stop_tx.clone(), inner.task.take())
        };
        if let Some(tx) = stop_tx {
            tx.send_replace(Stop::Now);
        }
        if let Some(task) = task {
            // the supervision sees the order, kills the process and ends
            if tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .is_err()
            {
                warn!(
                    "adapter '{}' did not end after being killed",
                    self.spec.name
                );
            }
        }
    }

    // ---- supervision -------------------------------------------------------------------------

    async fn supervise(self: Arc<Self>, mut stop_rx: watch::Receiver<Stop>) {
        loop {
            match self.one_life(&mut stop_rx).await {
                Life::Planned | Life::Failed => return,
                Life::Crashed { ran_for, cause } => {
                    let decision = {
                        let mut inner = self.inner.lock();
                        inner.crashes = inner.crashes.saturating_add(1);
                        inner.pid = None;
                        inner.conn = None;
                        inner.tracker.record_exit(
                            Instant::now(),
                            ran_for,
                            cause,
                            RestartMode::Always,
                        )
                    };
                    match decision {
                        Decision::Restart { delay, attempt } if self.spec.kind == Kind::Service => {
                            {
                                let mut inner = self.inner.lock();
                                self.set_state(
                                    &mut inner,
                                    EntryState::Backoff {
                                        attempt,
                                        delay_ms: u64::try_from(delay.as_millis())
                                            .unwrap_or(u64::MAX),
                                    },
                                );
                            }
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = wait_stop(&mut stop_rx, false) => {
                                    let mut inner = self.inner.lock();
                                    self.set_state(&mut inner, EntryState::Stopped);
                                    return;
                                }
                            }
                            let mut inner = self.inner.lock();
                            self.set_state(&mut inner, EntryState::Starting);
                        }
                        Decision::Restart { .. } | Decision::NoRestart => {
                            // a tool starts again with its next call
                            let mut inner = self.inner.lock();
                            self.set_state(&mut inner, EntryState::Stopped);
                            return;
                        }
                        Decision::GiveUp { exits_in_window } => {
                            self.fail(FailReason::CrashLoop, format!("the adapter ended {exits_in_window} times in a short while; it will not be started again"));
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Start the process, shake hands, serve until it ends, and say how it ended.
    async fn one_life(self: &Arc<Self>, stop_rx: &mut watch::Receiver<Stop>) -> Life {
        if *stop_rx.borrow() != Stop::No {
            let mut inner = self.inner.lock();
            self.set_state(&mut inner, EntryState::Stopped);
            return Life::Planned;
        }
        let mut proc = match AdapterProc::start(
            &self.spec.launch,
            &self.pids,
            self.spec.slot,
            self.cfg.conn,
            Arc::clone(&self.inbound),
        )
        .await
        {
            Ok(proc) => proc,
            Err(ProcError::Tampered(why)) => {
                self.fail(FailReason::Tampered, why);
                return Life::Failed;
            }
            Err(ProcError::Missing(why)) => {
                self.fail(FailReason::NeedsReinstall, why);
                return Life::Failed;
            }
            Err(ProcError::Sandbox(why)) => {
                self.fail(FailReason::Sandbox, why);
                return Life::Failed;
            }
            Err(ProcError::Spawn(why)) => {
                warn!("adapter '{}' did not start: {}", self.spec.name, why);
                self.inner.lock().note = Some(why);
                return Life::Crashed {
                    ran_for: Duration::ZERO,
                    cause: ExitCause::SpawnFailed,
                };
            }
        };
        {
            let mut inner = self.inner.lock();
            inner.pid = Some(proc.pid());
            inner.stderr_tail = Some(Arc::clone(&proc.stderr_tail));
            inner.stderr_dropped = Some(Arc::clone(&proc.stderr_dropped));
        }
        self.happened("started");

        // the handshake
        let conn = Arc::clone(&proc.conn);
        let outcome = tokio::select! {
            r = self.handshake(&conn) => Some(r),
            _ = wait_stop(stop_rx, false) => None,
        };
        let Some(outcome) = outcome else {
            // it has not said it is ready: there is nothing for it to flush
            proc.kill().await;
            let mut inner = self.inner.lock();
            inner.pid = None;
            self.set_state(&mut inner, EntryState::Stopped);
            return Life::Planned;
        };
        match outcome {
            Ok(()) => {}
            Err(Handshake::Tampered(why)) => {
                proc.kill().await;
                self.fail(FailReason::Tampered, why);
                return Life::Failed;
            }
            Err(Handshake::Incompatible(why)) => {
                proc.kill().await;
                self.fail(FailReason::Incompatible, why);
                return Life::Failed;
            }
            Err(Handshake::Broke(why)) => {
                warn!(
                    "adapter '{}' did not complete the handshake: {}",
                    self.spec.name, why
                );
                // a launcher that refuses exits at once, so a long wait costs nothing there; an
                // adapter that merely failed the handshake is killed after it
                let grace = if self.spec.launch.sandbox.is_some() {
                    LAUNCHER_EXIT_WAIT
                } else {
                    END_WAIT
                };
                let cause = self.end_of(&mut proc, grace).await;
                // the launcher exits 126 when it could not set the sandbox up: a failure
                // of the sandbox, not a crash of the adapter, and the same again on every retry
                if self.spec.launch.sandbox.is_some()
                    && matches!(cause, ExitCause::Crash { code: Some(code), .. } if code == oxidepm_sandbox::EXIT_SANDBOX_FAILED)
                {
                    let reason = self.launcher_reason(&proc).await;
                    self.fail(FailReason::Sandbox, reason);
                    return Life::Failed;
                }
                self.inner.lock().note = Some(why);
                return Life::Crashed {
                    ran_for: proc.started.elapsed(),
                    cause,
                };
            }
        }

        let _ = conn.notify(method::INITIALIZED, &json!({}));
        // a service that listens gets its events from now on, after `initialized` and before
        // anyone can see it ready
        let mut pump = self
            .events
            .get()
            .and_then(|link| link.queue.as_ref())
            .map(|queue| queue.attach(Arc::clone(&conn)));
        {
            let mut inner = self.inner.lock();
            inner.conn = Some(Arc::clone(&conn));
            inner.last_used = Instant::now();
            inner.retry_used = false;
            inner.retry_at = None;
            inner.note = None;
            self.set_state(&mut inner, EntryState::Ready);
        }

        // serve
        let (watch_pid, watch_pgid) = (proc.pid(), proc.pgid());
        let ended = tokio::select! {
            biased;
            _ = wait_stop(stop_rx, false) => Ended::Stop,
            status = proc.wait_exit() => Ended::Exited(status),
            reason = conn.closed() => Ended::ConnClosed(reason),
            _ = self.ping_cycle(&conn), if self.spec.kind == Kind::Service => Ended::PingMissed,
            _ = self.idle_cycle(), if self.spec.kind == Kind::Tool => Ended::Idle,
            over = self.monitor_cycle(watch_pid, watch_pgid) => over,
        };
        match ended {
            Ended::Stop | Ended::Idle => {
                // what the apps' stop just produced goes out before `shutdown` does
                if let Some(pump) = pump.take() {
                    if *stop_rx.borrow() != Stop::Now {
                        pump.finish(FLUSH_WAIT).await;
                    }
                }
                {
                    let mut inner = self.inner.lock();
                    self.set_state(&mut inner, EntryState::Stopping);
                    inner.conn = None;
                }
                self.end_politely(&mut proc, stop_rx).await;
                let mut inner = self.inner.lock();
                inner.pid = None;
                self.set_state(&mut inner, EntryState::Stopped);
                Life::Planned
            }
            Ended::Exited(status) => {
                self.happened("exited");
                let cause = cause_of(status);
                proc.kill().await;
                Life::Crashed {
                    ran_for: proc.started.elapsed(),
                    cause,
                }
            }
            Ended::ConnClosed(reason) => {
                if matches!(reason, CloseReason::Strikes | CloseReason::WriteStalled) {
                    self.happened("killed");
                }
                let cause = self.end_of(&mut proc, END_WAIT).await;
                Life::Crashed {
                    ran_for: proc.started.elapsed(),
                    cause,
                }
            }
            Ended::OverLimit(why) => {
                // a kill and a crash record
                self.happened("killed");
                warn!(
                    "adapter '{}' is over a limit ({}); killing it",
                    self.spec.name, why
                );
                self.inner.lock().note = Some(why);
                proc.kill().await;
                Life::Crashed {
                    ran_for: proc.started.elapsed(),
                    cause: ExitCause::Crash {
                        code: None,
                        signal: Some(9),
                    },
                }
            }
            Ended::OverQuota(why) => {
                // disabled with a warning, the data is kept
                warn!("adapter '{}' is disabled: {}", self.spec.name, why);
                self.happened("disabled");
                {
                    let mut inner = self.inner.lock();
                    self.set_state(&mut inner, EntryState::Stopping);
                    inner.conn = None;
                }
                self.end_politely(&mut proc, stop_rx).await;
                let mut inner = self.inner.lock();
                inner.pid = None;
                inner.note = Some(why);
                self.set_state(&mut inner, EntryState::Disabled);
                Life::Planned
            }
            Ended::PingMissed => {
                self.happened("killed");
                warn!(
                    "adapter '{}' missed {} pings in a row; killing it",
                    self.spec.name, self.cfg.ping_misses
                );
                proc.kill().await;
                Life::Crashed {
                    ran_for: proc.started.elapsed(),
                    cause: ExitCause::Crash {
                        code: None,
                        signal: None,
                    },
                }
            }
        }
    }

    /// What the launcher said when it refused to start the adapter: the last line it wrote to
    /// stderr. `end_of` has already waited for the reader to reach the end of the stream.
    async fn launcher_reason(&self, proc: &AdapterProc) -> String {
        let line = proc.stderr_tail.lock().back().cloned();
        line.unwrap_or_else(|| {
            "the sandbox launcher exited with status 126 and said nothing".to_string()
        })
    }

    /// The planned end: the polite sequence, cut short by an order to kill now.
    async fn end_politely(&self, proc: &mut AdapterProc, stop_rx: &mut watch::Receiver<Stop>) {
        if *stop_rx.borrow() == Stop::Now {
            proc.kill().await;
            return;
        }
        tokio::select! {
            _ = proc.shutdown(self.cfg.shutdown) => {}
            _ = wait_stop(stop_rx, true) => {
                proc.kill().await;
            }
        }
    }

    /// The connection ended: give the process a moment to exit by itself, then kill what is left,
    /// and say how it ended.
    async fn end_of(&self, proc: &mut AdapterProc, grace: Duration) -> ExitCause {
        let status = tokio::time::timeout(grace, proc.wait_exit())
            .await
            .ok()
            .flatten();
        if status.is_some() {
            // it has exited: let the reader take in what it wrote last before the reader is stopped
            proc.drain_stderr(STDERR_DRAIN).await;
        }
        proc.kill().await;
        cause_of(status)
    }

    async fn handshake(&self, conn: &Arc<super::conn::AdapterConn>) -> Result<(), Handshake> {
        let params = InitializeParams {
            protocols: SUPPORTED_PROTOCOLS.to_vec(),
            host: NameVersion {
                name: "oxidepmd".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            adapter: NameVersion {
                name: self.spec.name.clone(),
                version: self.spec.version.clone(),
            },
            kind: self.spec.kind,
            grant: self.spec.grant,
            paths: InitPaths {
                state: self.spec.launch.state_dir.display().to_string(),
            },
            limits: self.cfg.conn.limits.for_initialize(),
        };
        let reply: Result<InitializeResult, CallError> = conn
            .call_typed(method::INITIALIZE, &params, self.cfg.init_timeout)
            .await;
        match reply {
            Ok(r) => {
                if r.name != self.spec.name {
                    return Err(Handshake::Tampered(format!(
                        "the adapter answered as '{}'; the record says '{}'. This is not the adapter that was imported",
                        r.name, self.spec.name
                    )));
                }
                if r.version != self.spec.version {
                    return Err(Handshake::Tampered(format!(
                        "the adapter answered as version {}; the record says {}. This is not the adapter that was imported",
                        r.version, self.spec.version
                    )));
                }
                if !SUPPORTED_PROTOCOLS.contains(&r.protocol) {
                    return Err(Handshake::Incompatible(format!(
                        "the adapter chose protocol {}; this daemon offered {:?}",
                        r.protocol, SUPPORTED_PROTOCOLS
                    )));
                }
                conn.accept_handshake();
                Ok(())
            }
            Err(CallError::Remote(e)) if e.code == rpc::UNSUPPORTED_PROTOCOL => {
                let supported = e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("supported"))
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                Err(Handshake::Incompatible(format!(
                    "the adapter supports no protocol this daemon speaks (daemon: {:?}; adapter: {}): {}",
                    SUPPORTED_PROTOCOLS,
                    if supported.is_empty() { "unknown".to_string() } else { supported },
                    e.message
                )))
            }
            Err(CallError::Timeout) => Err(Handshake::Broke(
                "no answer to initialize in time".to_string(),
            )),
            Err(other) => Err(Handshake::Broke(other.to_string())),
        }
    }

    /// Ping the adapter every interval; returns when `ping_misses` in a row went unanswered. Never
    /// returns otherwise.
    async fn ping_cycle(&self, conn: &Arc<super::conn::AdapterConn>) {
        let mut misses = 0;
        loop {
            tokio::time::sleep(self.cfg.ping_interval).await;
            match conn
                .call(method::PING, &json!({}), self.cfg.ping_timeout)
                .await
            {
                Ok(_) => misses = 0,
                Err(CallError::Crashed(_)) => return,
                Err(_) => {
                    misses += 1;
                    if misses >= self.cfg.ping_misses {
                        return;
                    }
                }
            }
        }
    }

    /// Measure the adapter's tree every `monitor_interval` and its state directory every
    /// `quota_interval`; returns when a resource limit is exceeded. Never returns otherwise. The
    /// measuring reads `/proc` and the directory, so it runs on a blocking thread.
    async fn monitor_cycle(&self, pid: u32, pgid: i32) -> Ended {
        let memory_limit = u64::from(self.spec.launch.memory_mb) * 1024 * 1024;
        let max_threads = u64::from(self.cfg.max_threads);
        let state_dir = self.spec.launch.state_dir.clone();
        let mut last_quota = Instant::now();
        loop {
            tokio::time::sleep(self.cfg.monitor_interval).await;
            let usage = tokio::task::spawn_blocking(move || group_usage(pid, pgid))
                .await
                .unwrap_or_default();
            if memory_limit > 0 && usage.rss_bytes > memory_limit {
                return Ended::OverLimit(format!(
                    "its processes use {} MiB of memory; the limit is {} MiB",
                    usage.rss_bytes / (1024 * 1024),
                    self.spec.launch.memory_mb
                ));
            }
            if usage.threads > max_threads {
                return Ended::OverLimit(format!(
                    "its processes run {} threads; the limit is {}",
                    usage.threads, self.cfg.max_threads
                ));
            }
            if last_quota.elapsed() >= self.cfg.quota_interval {
                last_quota = Instant::now();
                let dir = state_dir.clone();
                let bytes = tokio::task::spawn_blocking(move || dir_usage(&dir, DIR_WALK_MAX))
                    .await
                    .unwrap_or(0);
                if bytes > self.cfg.state_quota_bytes {
                    return Ended::OverQuota(format!(
                        "its state directory holds {} MiB; the limit is {} MiB. The data is kept; enable the adapter again after clearing it",
                        bytes / (1024 * 1024),
                        self.cfg.state_quota_bytes / (1024 * 1024)
                    ));
                }
            }
        }
    }

    /// Returns when a tool has had nothing to do for `tool_idle`; at that moment (under the lock)
    /// it is `Stopping`, so a call that arrives later waits for the exit instead of being
    /// written to a closing pipe.
    async fn idle_cycle(&self) {
        loop {
            let wait = {
                let mut inner = self.inner.lock();
                if inner.state != EntryState::Ready || inner.active > 0 {
                    self.cfg.tool_idle
                } else {
                    let idle_for = inner.last_used.elapsed();
                    if idle_for >= self.cfg.tool_idle {
                        inner.conn = None;
                        self.set_state(&mut inner, EntryState::Stopping);
                        return;
                    }
                    self.cfg.tool_idle - idle_for
                }
            };
            tokio::time::sleep(wait.max(Duration::from_millis(5))).await;
        }
    }

    // ---- relayed verbs -----------------------------------------------------------------------

    pub fn verbs(&self) -> &[Verb] {
        &self.spec.verbs
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.lock().state != EntryState::Disabled
    }

    /// The rules for the runtime `name`, when this adapter provides it.
    pub fn runtime_rules(&self, name: &str) -> Option<&oxidepm_runtime::step::RuntimeRules> {
        self.spec
            .runtime
            .as_ref()
            .filter(|r| r.runtimes.iter().any(|n| n == name))
            .map(|r| &r.rules)
    }

    /// The runtimes this adapter provides.
    pub fn runtimes(&self) -> &[String] {
        self.spec
            .runtime
            .as_ref()
            .map(|r| r.runtimes.as_slice())
            .unwrap_or(&[])
    }

    /// The sha256 of the executable this entry runs (recorded when the adapter was imported).
    pub fn exe_sha256(&self) -> &str {
        &self.spec.launch.exe_sha256
    }

    /// A look at the entry that waits for nothing: can a call be made now? A tool that is
    /// stopped can be started, so it is available; a failed one is not (unless its one retry is
    /// due), and neither is an entry whose circuit is open.
    pub fn available(&self) -> Result<(), Error> {
        let now = Instant::now();
        let inner = self.inner.lock();
        match inner.state {
            EntryState::Disabled => {
                return Err(Error::AdapterUnavailable(format!(
                    "adapter '{}' is disabled",
                    self.spec.name
                )))
            }
            EntryState::Failed(_) => {
                let retry_due =
                    self.spec.kind == Kind::Tool && inner.retry_at.is_some_and(|at| now >= at);
                if !retry_due {
                    return Err(Error::AdapterUnavailable(format!(
                        "adapter '{}' has failed: {}",
                        self.spec.name,
                        inner.note.clone().unwrap_or_default()
                    )));
                }
            }
            _ => {}
        }
        if inner.breaker.is_open(now) {
            return Err(Error::AdapterUnavailable(format!(
                "adapter '{}' did not answer in time and is being left alone for a moment; try again shortly",
                self.spec.name
            )));
        }
        Ok(())
    }

    pub fn kind_of(&self) -> Kind {
        self.spec.kind
    }

    /// The manifest marks the adapter `ai_driven`.
    pub fn ai_driven(&self) -> bool {
        self.spec.ai_driven
    }

    /// End every process that a relayed verb started for a single call.
    fn cancel_oneshots(&self) {
        let old = std::mem::replace(&mut *self.oneshots.lock(), CancellationToken::new());
        old.cancel();
    }

    /// Run a relayed verb and report its progress to `out`. A service gets `command.invoke`
    /// on its running connection; a tool is started for this call alone, under a profile expanded
    /// for the call's `$CWD` (a safe verb, or a rehearsal, writes only to the state directory; a
    /// rehearsal also spawns nothing) and ended when the call is over. `cancel` ends the call: a
    /// `cancel` goes to the service, the tool is killed.
    pub async fn invoke(
        self: &Arc<Self>,
        call: InvokeCall,
        out: mpsc::Sender<Output>,
        cancel: CancellationToken,
    ) -> Result<InvokeResult, Error> {
        let _slot = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return Err(Error::AdapterBusy(format!(
                    "adapter '{}' is running {} calls already; try again when one has finished",
                    self.spec.name, self.cfg.invoke_slots
                )))
            }
        };
        match self.spec.kind {
            Kind::Service => {
                let conn = self.acquire().await?;
                let _release = Release { entry: self };
                self.drive(
                    &conn,
                    &call,
                    Some(&out),
                    &cancel,
                    Some(self.cfg.invoke_idle),
                )
                .await
            }
            Kind::Tool => self.invoke_alone(call, out, cancel).await,
        }
    }

    /// The profile for one call of a tool: the granted permissions, with writes cut back to the
    /// state directory for a safe verb or a rehearsal and no spawning for a rehearsal.
    fn call_launch(&self, call: &InvokeCall) -> super::proc::Launch {
        self.call_launch_with(call, &[])
    }

    /// [`call_launch`](Self::call_launch) with more paths the process may write (and read): the
    /// socket of a session.
    fn call_launch_with(
        &self,
        call: &InvokeCall,
        extra_write: &[std::path::PathBuf],
    ) -> super::proc::Launch {
        let mut launch = self.spec.launch.clone();
        if call.params.dry_run {
            launch.connect_env_socket.clear();
        }
        let (Some(sandbox), Some(recipe)) = (launch.sandbox.as_mut(), self.spec.recipe.as_ref())
        else {
            return launch;
        };
        let mut permissions = recipe.permissions.clone();
        if call.restrict_writes || call.params.dry_run {
            permissions.fs.write = vec!["$STATE".to_string()];
        }
        if call.params.dry_run {
            permissions.spawn = SpawnPerm::default();
        }
        let cwd = std::path::PathBuf::from(&call.params.cwd);
        let input = oxidepm_sandbox::ProfileInput {
            state_dir: &self.spec.launch.state_dir,
            daemon_home: &recipe.daemon_home,
            cwd: Some(&cwd),
            search_path: &recipe.search_path,
        };
        let mut profile = oxidepm_sandbox::Profile::from_permissions(&permissions, &input);
        for path in extra_write {
            if !profile.write.contains(path) {
                profile.write.push(path.clone());
            }
        }
        sandbox.profile_json = profile.to_json();
        launch
    }

    /// Is the adapter in a state where a process may be started for a call?
    fn ensure_callable(&self) -> Result<(), Error> {
        let inner = self.inner.lock();
        match inner.state {
            EntryState::Disabled => Err(Error::AdapterUnavailable(format!(
                "adapter '{}' is disabled",
                self.spec.name
            ))),
            EntryState::Failed(reason) => {
                let note = inner.note.clone().unwrap_or_default();
                Err(match reason {
                    FailReason::NeedsReinstall => {
                        Error::AdapterMissing(format!("adapter '{}': {note}", self.spec.name))
                    }
                    FailReason::Tampered => {
                        Error::AdapterTampered(format!("adapter '{}': {note}", self.spec.name))
                    }
                    _ => Error::AdapterUnavailable(format!(
                        "adapter '{}' failed ({}): {note}",
                        self.spec.name,
                        reason.as_str()
                    )),
                })
            }
            _ => Ok(()),
        }
    }

    /// The error for a process that could not be started, recording the failure where it sticks.
    fn start_error(&self, e: ProcError) -> Error {
        match e {
            ProcError::Tampered(why) => {
                self.fail(FailReason::Tampered, why.clone());
                Error::AdapterTampered(format!("adapter '{}': {why}", self.spec.name))
            }
            ProcError::Missing(why) => {
                self.fail(FailReason::NeedsReinstall, why.clone());
                Error::AdapterMissing(format!("adapter '{}': {why}", self.spec.name))
            }
            ProcError::Sandbox(why) | ProcError::Spawn(why) => Error::AdapterUnavailable(format!(
                "adapter '{}' did not start: {why}",
                self.spec.name
            )),
        }
    }

    /// The error for a handshake that failed, recording the failure where it sticks.
    fn handshake_error(&self, h: Handshake) -> Error {
        match h {
            Handshake::Tampered(why) => {
                self.fail(FailReason::Tampered, why.clone());
                Error::AdapterTampered(format!("adapter '{}': {why}", self.spec.name))
            }
            Handshake::Incompatible(why) => {
                self.fail(FailReason::Incompatible, why.clone());
                Error::AdapterProtocol(format!("adapter '{}': {why}", self.spec.name))
            }
            Handshake::Broke(why) => {
                Error::AdapterCrashed(format!("adapter '{}' did not start: {why}", self.spec.name))
            }
        }
    }

    /// A tool's relayed verb: start the executable for this call, shake hands, invoke, end it.
    async fn invoke_alone(
        self: &Arc<Self>,
        call: InvokeCall,
        out: mpsc::Sender<Output>,
        cancel: CancellationToken,
    ) -> Result<InvokeResult, Error> {
        self.ensure_callable()?;
        // the host's order to end every call (a stop, a disable, the shutdown) joins the caller's
        let ended = self.oneshots.lock().child_token();
        let launch = self.call_launch(&call);
        let mut proc = match AdapterProc::start_named(
            &launch,
            &self.pids,
            call.slot,
            self.cfg.conn,
            Arc::clone(&self.inbound),
            "stderr-invoke.log",
        )
        .await
        {
            Ok(proc) => proc,
            Err(e) => return Err(self.start_error(e)),
        };
        self.inner.lock().stderr_tail = Some(Arc::clone(&proc.stderr_tail));
        let conn = Arc::clone(&proc.conn);

        let shaken = tokio::select! {
            r = self.handshake(&conn) => r,
            _ = cancel.cancelled() => { proc.kill().await; return Err(Error::Cancelled("the call was cancelled".to_string())) }
            _ = ended.cancelled() => { proc.kill().await; return Err(Error::Cancelled("the adapter was stopped".to_string())) }
        };
        if let Err(h) = shaken {
            proc.kill().await;
            return Err(self.handshake_error(h));
        }
        let _ = conn.notify(method::INITIALIZED, &json!({}));

        let (pid, pgid) = (proc.pid(), proc.pgid());
        let result = tokio::select! {
            r = self.drive(&conn, &call, Some(&out), &cancel, Some(self.cfg.invoke_idle)) => r,
            _ = ended.cancelled() => Err(Error::Cancelled("the adapter was stopped".to_string())),
            over = self.monitor_cycle(pid, pgid) => {
                let why = match over { Ended::OverLimit(w) | Ended::OverQuota(w) => w, _ => "a limit was exceeded".to_string() };
                Err(Error::AdapterCrashed(format!("adapter '{}' was killed: {why}", self.spec.name)))
            }
        };
        match &result {
            // finished: the polite end, which a tool answers at once
            Ok(_) => {
                proc.shutdown(self.cfg.shutdown).await;
            }
            Err(_) => proc.kill().await,
        }
        result
    }

    /// Run a session verb: start the executable on the terminal the CLI handed over, wait for it
    /// to connect to its session socket, shake hands, invoke, end it. The profile is the one of
    /// any call, with the session socket added. `cancel` ends the session (the CLI went away);
    /// the host's order to end every call (a disable, the shutdown) does too.
    pub async fn run_session(
        self: &Arc<Self>,
        mut session: SessionCall,
        cancel: CancellationToken,
    ) -> Result<InvokeResult, Error> {
        self.ensure_callable()?;
        let ended = self.oneshots.lock().child_token();
        let launch = self.call_launch_with(&session.call, &[session.socket.clone()]);
        let (host_end, bridge_end) = tokio::net::UnixStream::pair().map_err(Error::IoError)?;
        let inbound = self
            .inbound
            .for_session()
            .unwrap_or_else(|| Arc::clone(&self.inbound));
        let launch_session = super::proc::SessionLaunch {
            terminal: session.terminal,
            env: vec![
                (
                    oxidepm_ipc::session::ENV_SOCKET.to_string(),
                    session.socket.display().to_string(),
                ),
                (
                    oxidepm_ipc::session::ENV_TOKEN.to_string(),
                    session.token.clone(),
                ),
            ],
            channel: host_end,
        };
        let mut proc = match AdapterProc::start_session(
            &launch,
            &self.pids,
            session.call.slot,
            self.cfg.conn,
            inbound,
            "stderr-session.log",
            launch_session,
        )
        .await
        {
            Ok(proc) => proc,
            Err(e) => return Err(self.start_error(e)),
        };
        self.inner.lock().stderr_tail = Some(Arc::clone(&proc.stderr_tail));
        let conn = Arc::clone(&proc.conn);

        // the adapter connects to its socket and presents its token
        let connected = tokio::select! {
            r = super::session::accept_adapter(&session.listener, &session.token, session.deadline) => r,
            status = proc.wait_exit() => Err(Error::AdapterCrashed(format!(
                "adapter '{}' ended before it connected to its session ({})",
                self.spec.name,
                status.map_or("no exit status".to_string(), |s| s.to_string())
            ))),
            _ = cancel.cancelled() => Err(Error::Cancelled("the session was ended before it started".to_string())),
            _ = ended.cancelled() => Err(Error::Cancelled("the adapter was stopped".to_string())),
        };
        let bridge = match connected {
            Ok(stream) => {
                // both sides are connected: nothing else may present a token, so the socket goes
                drop(session.listener);
                let _ = std::fs::remove_file(&session.socket);
                super::session::bridge(stream, bridge_end)
            }
            Err(e) => {
                proc.kill().await;
                return Err(e);
            }
        };

        let shaken = tokio::select! {
            r = self.handshake(&conn) => r,
            _ = cancel.cancelled() => { proc.kill().await; bridge.abort(); return Err(Error::Cancelled("the session was ended before it started".to_string())) }
            _ = ended.cancelled() => { proc.kill().await; bridge.abort(); return Err(Error::Cancelled("the adapter was stopped".to_string())) }
        };
        if let Err(h) = shaken {
            proc.kill().await;
            bridge.abort();
            return Err(self.handshake_error(h));
        }
        let _ = conn.notify(method::INITIALIZED, &json!({}));

        let (pid, pgid) = (proc.pid(), proc.pgid());
        // what a session verb reports as output goes nowhere: it has the terminal
        let drive = self.drive(&conn, &session.call, None, &cancel, None);
        let monitor = self.monitor_cycle(pid, pgid);
        tokio::pin!(drive, monitor);
        let result = loop {
            tokio::select! {
                r = &mut drive => break r,
                Some(()) = session.winch.recv() => {
                    let _ = oxidepm_runtime::kill_group(pgid, nix::sys::signal::Signal::SIGWINCH);
                }
                _ = ended.cancelled() => break Err(Error::Cancelled("the adapter was stopped".to_string())),
                over = &mut monitor => {
                    let why = match over { Ended::OverLimit(w) | Ended::OverQuota(w) => w, _ => "a limit was exceeded".to_string() };
                    break Err(Error::AdapterCrashed(format!("adapter '{}' was killed: {why}", self.spec.name)));
                }
            }
        };
        match &result {
            Ok(_) => {
                proc.shutdown(self.cfg.shutdown).await;
            }
            Err(_) => proc.kill().await,
        }
        bridge.abort();
        result
    }

    /// Send `command.invoke` on `conn` and relay what comes back until the answer, the caller's
    /// cancel, the deadline of the verb, or `invoke_idle` without a frame.
    async fn drive(
        &self,
        conn: &Arc<AdapterConn>,
        call: &InvokeCall,
        out: Option<&mpsc::Sender<Output>>,
        cancel: &CancellationToken,
        idle: Option<Duration>,
    ) -> Result<InvokeResult, Error> {
        let (tx, mut rx) = mpsc::channel::<Output>(super::conn::RELAY_QUEUE_FRAMES);
        let reply =
            conn.call_with_progress(method::COMMAND_INVOKE, &call.params, call.max_runtime, tx);
        tokio::pin!(reply);
        let mut last = tokio::time::Instant::now();
        let until = last + call.max_runtime;
        let forward = |frame| async move {
            let Some(out) = out else { return Ok(()) };
            tokio::select! {
                _ = cancel.cancelled() => Err(Error::Cancelled("the client went away".into())),
                sent = tokio::time::timeout_at(until, out.send(frame)) => match sent {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(Error::Cancelled("the client went away".into())),
                    Err(_) => Err(CallError::OutputBackpressure.into_error(&self.spec.name)),
                },
            }
        };
        let answer = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // dropping the pending call tells the adapter to stop
                    return Err(Error::Cancelled("the client went away".to_string()));
                }
                Some(frame) = rx.recv() => {
                    last = tokio::time::Instant::now();
                    forward(frame).await?;
                }
                r = &mut reply => break r,
                // a session has no idle limit: a person at a terminal may think for a long time
                _ = async {
                    match idle {
                        Some(idle) => tokio::time::sleep_until(last + idle).await,
                        None => std::future::pending().await,
                    }
                } => {
                    return Err(Error::AdapterTimeout(format!(
                        "adapter '{}' sent nothing for {} s while running '{}'; the call was ended",
                        self.spec.name,
                        idle.unwrap_or_default().as_secs_f64(),
                        call.params.verb
                    )));
                }
            }
        };
        // what the adapter reported before it answered
        while let Ok(frame) = rx.try_recv() {
            forward(frame).await?;
        }
        let params = answer.map_err(|e| match e {
            CallError::Remote(e) if e.code == rpc::DRY_RUN_UNSUPPORTED => {
                Error::DryRunUnsupported(format!(
                    "adapter '{}' has no dry run for '{}': {}",
                    self.spec.name, call.params.verb, e.message
                ))
            }
            CallError::Remote(e) if e.code == rpc::PERMISSION_DENIED => {
                Error::PolicyDenied(format!(
                    "adapter '{}' refused '{}': {}",
                    self.spec.name, call.params.verb, e.message
                ))
            }
            other => other.into_error(&self.spec.name),
        })?;
        match oxidepm_adapter_proto::from_params::<InvokeResult>(&params) {
            Ok(result) => Ok(result),
            Err(e) => {
                conn.strike("a reply of the wrong shape");
                Err(Error::AdapterProtocol(format!(
                    "adapter '{}': the reply to command.invoke has the wrong shape: {e}",
                    self.spec.name
                )))
            }
        }
    }

    // ---- calls -------------------------------------------------------------------------------

    /// Call the adapter and wait for the answer, at most `deadline`. A tool is started if it is
    /// stopped. Fails at once when the adapter is failed or the breaker is open; otherwise waits
    /// at most `ready_wait` for it to be ready.
    pub async fn call<P: serde::Serialize>(
        self: &Arc<Self>,
        method: &str,
        params: &P,
        deadline: Duration,
    ) -> Result<Params, Error> {
        let tracked = is_launch_path(method);
        let probe = if tracked {
            let mut inner = self.inner.lock();
            match inner.breaker.admit(Instant::now()) {
                Admit::Reject => {
                    return Err(Error::AdapterUnavailable(format!(
                        "adapter '{}' did not answer in time and is being left alone for a moment; try again shortly",
                        self.spec.name
                    )))
                }
                Admit::Probe => true,
                Admit::Go => false,
            }
        } else {
            false
        };
        let conn = match self.acquire().await {
            Ok(conn) => conn,
            Err(e) => {
                if probe {
                    self.inner.lock().breaker.probe_abandoned();
                }
                return Err(e);
            }
        };
        let _release = Release { entry: self };
        let result = conn.call(method, params, deadline).await;
        if tracked {
            let mut inner = self.inner.lock();
            match &result {
                Ok(_) | Err(CallError::Remote(_)) => inner.breaker.success(),
                Err(CallError::Timeout) | Err(CallError::Crashed(_)) => {
                    inner.breaker.failure(Instant::now())
                }
                Err(_) => inner.breaker.probe_abandoned(),
            }
        }
        result.map_err(|e| e.into_error(&self.spec.name))
    }

    /// Get the ready connection, starting the adapter or waiting for it as the state requires.
    /// Counts the caller as active until the guard is dropped.
    async fn acquire(self: &Arc<Self>) -> Result<Arc<super::conn::AdapterConn>, Error> {
        let give_up = Instant::now() + self.cfg.ready_wait;
        let mut state_rx = self.state_tx.subscribe();
        loop {
            {
                let mut inner = self.inner.lock();
                match inner.state {
                    EntryState::Ready => {
                        if let Some(conn) = inner.conn.clone() {
                            inner.active += 1;
                            return Ok(conn);
                        }
                    }
                    EntryState::Disabled => {
                        return Err(Error::AdapterUnavailable(format!(
                            "adapter '{}' is disabled",
                            self.spec.name
                        )));
                    }
                    EntryState::Failed(reason) => {
                        let due = inner.retry_at.is_some_and(|at| Instant::now() >= at);
                        if self.spec.kind == Kind::Tool && due {
                            // the one retry of a failed tool
                            inner.retry_at = None;
                            inner.retry_used = true;
                            self.set_state(&mut inner, EntryState::Stopped);
                            self.begin(&mut inner);
                        } else {
                            let note = inner.note.clone().unwrap_or_default();
                            return Err(match reason {
                                FailReason::NeedsReinstall => Error::AdapterMissing(format!(
                                    "adapter '{}': {note}",
                                    self.spec.name
                                )),
                                FailReason::Tampered => Error::AdapterTampered(format!(
                                    "adapter '{}': {note}",
                                    self.spec.name
                                )),
                                _ => Error::AdapterUnavailable(format!(
                                    "adapter '{}' failed ({}): {note}",
                                    self.spec.name,
                                    reason.as_str()
                                )),
                            });
                        }
                    }
                    EntryState::Stopped => self.begin(&mut inner),
                    // being started, backing off, or on its way out: wait
                    EntryState::Starting
                    | EntryState::Backoff { .. }
                    | EntryState::Stopping
                    | EntryState::Degraded => {}
                }
            }
            let remaining = give_up.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::AdapterUnavailable(format!(
                    "adapter '{}' did not become ready in time",
                    self.spec.name
                )));
            }
            // wake on the next state change, or re-check soon: a change between the check above
            // and this wait would otherwise be missed
            let _ = tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                state_rx.changed(),
            )
            .await;
        }
    }
}

/// Wait until the owner asks for an end (any end, or only the immediate one).
async fn wait_stop(rx: &mut watch::Receiver<Stop>, only_now: bool) {
    let _ = rx
        .wait_for(|stop| {
            if only_now {
                *stop == Stop::Now
            } else {
                *stop != Stop::No
            }
        })
        .await;
}

/// A call is no longer using the connection.
struct Release<'a> {
    entry: &'a AdapterEntry,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut inner = self.entry.inner.lock();
        inner.active = inner.active.saturating_sub(1);
        inner.last_used = Instant::now();
    }
}

enum Handshake {
    Tampered(String),
    Incompatible(String),
    /// The handshake did not complete: a crash.
    Broke(String),
}

enum Ended {
    Stop,
    Idle,
    Exited(Option<std::process::ExitStatus>),
    ConnClosed(CloseReason),
    PingMissed,
    /// Memory or threads over the limit: killed, counted as a crash.
    OverLimit(String),
    /// The state directory is over its quota: disabled.
    OverQuota(String),
}

/// At most this many entries of the state directory are visited when its size is measured.
const DIR_WALK_MAX: usize = 10_000;

fn cause_of(status: Option<std::process::ExitStatus>) -> ExitCause {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(s) if s.success() => ExitCause::CleanExit,
        Some(s) => ExitCause::Crash {
            code: s.code(),
            signal: s.signal(),
        },
        None => ExitCause::Crash {
            code: None,
            signal: None,
        },
    }
}

impl Drop for AdapterEntry {
    fn drop(&mut self) {
        info!("adapter '{}' dropped", self.spec.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn breaker_opens_after_two_failures_and_lets_one_probe_through_after_30s() {
        let now = t0();
        let mut b = Breaker::new(2, Duration::from_secs(30));
        assert_eq!(b.admit(now), Admit::Go);
        b.failure(now);
        assert_eq!(b.admit(now), Admit::Go, "one failure does not open it");
        b.failure(now);
        assert!(b.is_open(now));
        assert_eq!(b.admit(now + Duration::from_secs(1)), Admit::Reject);
        assert_eq!(b.admit(now + Duration::from_secs(29)), Admit::Reject);
        // after 30 s one probe, and only one
        let later = now + Duration::from_secs(31);
        assert_eq!(b.admit(later), Admit::Probe);
        assert_eq!(b.admit(later), Admit::Reject, "the probe is out");
        // the probe fails: open for another 30 s
        b.failure(later);
        assert_eq!(b.admit(later + Duration::from_secs(29)), Admit::Reject);
        let again = later + Duration::from_secs(31);
        assert_eq!(b.admit(again), Admit::Probe);
        b.success();
        assert_eq!(b.admit(again), Admit::Go);
        assert!(!b.is_open(again));
    }

    #[test]
    fn a_success_between_failures_keeps_the_breaker_closed() {
        let now = t0();
        let mut b = Breaker::new(2, Duration::from_secs(30));
        b.failure(now);
        b.success();
        b.failure(now);
        assert_eq!(b.admit(now), Admit::Go, "the failures were not consecutive");
    }

    #[test]
    fn an_abandoned_probe_allows_the_next_call_to_probe() {
        let now = t0();
        let mut b = Breaker::new(1, Duration::from_secs(30));
        b.failure(now);
        let later = now + Duration::from_secs(31);
        assert_eq!(b.admit(later), Admit::Probe);
        b.probe_abandoned();
        assert_eq!(b.admit(later), Admit::Probe);
    }
}
