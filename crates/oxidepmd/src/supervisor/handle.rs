//! What the rest of the daemon holds of an app: a [`ProcessHandle`] (a mailbox, a snapshot
//! receiver and the live numbers), the [`Command`]s an actor takes, and their outcomes.
//!
//! The actor owns the child and all mutable state; a handle only sends it commands in order
//! and reads what it published. Reading a snapshot never messages the actor, so `status` and
//! `show` do not wait for a slow stop.

use chrono::{DateTime, Utc};
use oxidepm_core::{AppSpec, AppStatus, Desired, Error, Result, RunState};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

pub type AppId = u32;

/// Where a command's answer goes.
pub type Reply<T> = oneshot::Sender<Result<T>>;

/// Capacity of an app's mailbox.
pub const MAILBOX: usize = 64;

/// How long a sender waits for room in a full mailbox before it answers `E_BUSY`.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A single app (it owns a child) or a cluster parent (it owns none and fans out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    Process,
    Group,
}

/// Why a command was sent (logged, and copied into the lifecycle event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    User,
    /// The resurrect, starting an app from stored state.
    Resurrect,
    Reload,
    Group,
    Crash,
    Limit,
    Cron,
    Health,
    /// An adapter requested the command.
    #[allow(dead_code)]
    Adapter,
}

tokio::task_local! {
    /// The wire form (`class:label`) of whoever is asking, set by the daemon around the handler
    /// of a request. The supervisor's facade methods build their [`CmdCtx`] from it, so their
    /// signatures stay as they are. Outside a request (the tests that call the supervisor
    /// directly) it is unset and the actor is `None`.
    pub static CURRENT_ACTOR: Arc<str>;
}

/// Who asked and why. Carried by every command for audit and policy.
#[derive(Debug, Clone)]
pub struct CmdCtx {
    pub actor: Option<Arc<str>>,
    pub cause: Cause,
}

impl CmdCtx {
    /// A person (or whoever the request came from) asked: the actor is the one the daemon put in
    /// [`CURRENT_ACTOR`] around the request, if any.
    pub fn user() -> Self {
        Self::with_cause(Cause::User)
    }

    /// The boot resurrect: the daemon's own work.
    pub fn resurrect() -> Self {
        Self {
            actor: Some(Arc::from("system:resurrect")),
            cause: Cause::Resurrect,
        }
    }

    /// `cause`, with the actor of the request in scope.
    pub fn with_cause(cause: Cause) -> Self {
        Self {
            actor: CURRENT_ACTOR.try_with(Arc::clone).ok(),
            cause,
        }
    }

    /// The context for a command a cluster's group sends to one of its instances: the actor of
    /// the command the group got, because the group runs in a task of its own.
    pub fn for_instance(&self) -> Self {
        Self {
            actor: self.actor.clone(),
            cause: Cause::Group,
        }
    }
}

/// Who is starting the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOrigin {
    /// A client.
    User,
    /// The resurrect, starting an app from stored state. It only ever starts an app that is at
    /// rest and still meant to run: a stop that landed after the resurrect chose its
    /// targets wins, and an app that is already up is left alone.
    Resurrect,
    /// The group of a cluster, starting one of its instances.
    Group,
}

/// What kind of limit restarted the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Memory { observed_mb: u64, limit_mb: u64 },
    Uptime { secs: u64 },
}

/// Why an operator restart was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartCause {
    User,
    /// `reload` is a restart; there is no zero-downtime path.
    Reload,
}

impl RestartCause {
    pub fn cause(&self) -> Cause {
        match self {
            RestartCause::User => Cause::User,
            RestartCause::Reload => Cause::Reload,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartOutcome {
    pub id: AppId,
    pub name: String,
    pub status: AppStatus,
    pub child_ids: Vec<AppId>,
    /// False when nothing was started: a resurrect found the app up, or no longer meant to run.
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    /// The app was active and is now stopped.
    pub changed: bool,
    /// The desired state could not be persisted (the stop happened anyway).
    pub warning: Option<String>,
}

/// `reload` and `restart` both end in a restart; the field exists so the wire says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartKind {
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartOutcome {
    pub kind: RestartKind,
    pub status: AppStatus,
}

/// What an actor takes from its mailbox.
pub enum Command {
    // from the daemon
    Start {
        spec: Arc<AppSpec>,
        origin: StartOrigin,
        /// This call inserted the app's row: a failed first launch takes it out again.
        created: bool,
        ctx: CmdCtx,
        reply: Reply<StartOutcome>,
    },
    Stop {
        ctx: CmdCtx,
        reply: Reply<StopOutcome>,
    },
    Restart {
        cause: RestartCause,
        ctx: CmdCtx,
        reply: Reply<RestartOutcome>,
    },
    Delete {
        /// The row is already gone (a group removed it in its transaction).
        row_removed: bool,
        ctx: CmdCtx,
        reply: Reply<StopOutcome>,
    },
    /// Empty the app's active log files (a truncate, not a rotate).
    FlushLogs {
        reply: Reply<()>,
    },
    Shutdown {
        kill_cap: Duration,
        /// Answered with whether there was something to stop.
        reply: Reply<bool>,
    },
    // from tasks the actor owns (they carry the generation or epoch they were started for)
    /// A probe passed: the first time, or again after the app was reported unhealthy.
    HealthOk {
        gen: u64,
    },
    /// `retries` probes in a row failed.
    HealthFailed {
        gen: u64,
        failures: u32,
        message: String,
    },
    CronRestart {
        cron_epoch: u64,
    },
    /// The sampler saw the child of generation `gen` over its memory limit. The actor ignores it
    /// unless that child is still the one it runs, so a late post never touches a stopped app
    /// or a newer child. (The uptime limit is an arm of the actor's own loop, not a post.)
    LimitExceeded {
        gen: u64,
        kind: LimitKind,
    },
    /// Test hook: the actor panics.
    #[cfg(feature = "test-hooks")]
    TestPanic,
}

/// Published by the actor after every transition.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub spec: Arc<AppSpec>,
    /// Status and the data fields that go with it. `uptime_secs`, `cpu_percent` and
    /// `memory_bytes` are filled in when it is read ([`ProcessHandle::run_state`]).
    pub state: RunState,
    pub desired: Desired,
    pub parent_id: Option<AppId>,
    /// The instances of a cluster parent.
    pub children: Vec<AppId>,
    /// Bumps for every spawned child.
    #[allow(dead_code)] // read by the tests
    pub generation: u64,
}

impl Snapshot {
    pub fn new(spec: Arc<AppSpec>, desired: Desired, parent_id: Option<AppId>) -> Self {
        let state = RunState::new(spec.id);
        Self {
            spec,
            state,
            desired,
            parent_id,
            children: Vec::new(),
            generation: 0,
        }
    }
}

/// Pid, process group and generation of the live child. They change together, so they are one
/// value behind one lock, never separate atomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveProc {
    pub pid: u32,
    pub pgid: i32,
    pub gen: u64,
    /// Start time of the leader in clock ticks since boot (`/proc/<pid>/stat`, field 22), read
    /// at launch; 0 if it could not be read. The sampler measures the pid only while it matches.
    pub start_ticks: u64,
}

/// Numbers that tasks other than the actor write and `status` reads.
#[derive(Debug, Default)]
pub struct Live {
    proc: Mutex<Option<LiveProc>>,
    cpu_milli_pct: AtomicU32,
    mem_bytes: AtomicU64,
    health_failures: AtomicU32,
    last_health_unix_ms: AtomicI64,
    /// Set by the sampler when it posted a limit restart; cleared at launch.
    limit_pending: AtomicBool,
}

impl Live {
    pub fn proc(&self) -> Option<LiveProc> {
        *self.proc.lock()
    }

    pub fn set_proc(&self, proc: Option<LiveProc>) {
        *self.proc.lock() = proc;
        if proc.is_none() {
            self.cpu_milli_pct.store(0, Ordering::Relaxed);
            self.mem_bytes.store(0, Ordering::Relaxed);
        }
    }

    pub fn set_metrics(&self, cpu_percent: f32, mem_bytes: u64) {
        self.cpu_milli_pct
            .store((cpu_percent.max(0.0) * 1000.0) as u32, Ordering::Relaxed);
        self.mem_bytes.store(mem_bytes, Ordering::Relaxed);
    }

    pub fn cpu_percent(&self) -> f32 {
        self.cpu_milli_pct.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn mem_bytes(&self) -> u64 {
        self.mem_bytes.load(Ordering::Relaxed)
    }

    pub fn record_health(&self, failures: u32) {
        self.health_failures.store(failures, Ordering::Relaxed);
        self.last_health_unix_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    pub fn clear_health(&self) {
        self.health_failures.store(0, Ordering::Relaxed);
        self.last_health_unix_ms.store(0, Ordering::Relaxed);
    }

    pub fn health_failures(&self) -> u32 {
        self.health_failures.load(Ordering::Relaxed)
    }

    pub fn last_health(&self) -> Option<DateTime<Utc>> {
        match self.last_health_unix_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => DateTime::from_timestamp_millis(ms),
        }
    }

    /// Mark a limit restart as posted. False if one is already pending.
    pub fn begin_limit_restart(&self) -> bool {
        !self.limit_pending.swap(true, Ordering::AcqRel)
    }

    pub fn clear_limit_pending(&self) {
        self.limit_pending.store(false, Ordering::Release);
    }
}

/// The two policy markers of an app, read by the gate on every request and changed by
/// `SetPolicy` without a restart. They are atomics on the handle (and two columns of the
/// database): the single source of truth. The copy in the actor's `AppSpec` is only what the app
/// was started with.
#[derive(Debug, Default)]
pub struct Markers {
    protected: AtomicBool,
    ai_control: AtomicBool,
}

impl Markers {
    pub fn from_spec(spec: &AppSpec) -> Self {
        Self {
            protected: AtomicBool::new(spec.protected),
            ai_control: AtomicBool::new(spec.ai_control),
        }
    }

    pub fn protected(&self) -> bool {
        self.protected.load(Ordering::SeqCst)
    }

    pub fn ai_control(&self) -> bool {
        self.ai_control.load(Ordering::SeqCst)
    }

    pub fn set(&self, protected: bool, ai_control: bool) {
        self.protected.store(protected, Ordering::SeqCst);
        self.ai_control.store(ai_control, Ordering::SeqCst);
    }
}

/// Everything the daemon needs to talk to one app. Cheap to clone; stored in the registry.
#[derive(Clone)]
pub struct ProcessHandle {
    pub id: AppId,
    pub kind: HandleKind,
    tx: mpsc::Sender<Command>,
    snap: watch::Receiver<Arc<Snapshot>>,
    live: Arc<Live>,
    markers: Arc<Markers>,
    kill: CancellationToken,
    closed: Arc<AtomicBool>,
    abort: Option<AbortHandle>,
}

/// The pieces an actor is built from; the other halves are in its handle.
pub struct HandleParts {
    pub rx: mpsc::Receiver<Command>,
    /// For tasks the actor owns: it does not keep the mailbox open, so the actor ends when the
    /// last handle is dropped.
    pub weak_tx: mpsc::WeakSender<Command>,
    pub snap_tx: watch::Sender<Arc<Snapshot>>,
    pub live: Arc<Live>,
    pub kill: CancellationToken,
    pub closed: Arc<AtomicBool>,
}

impl ProcessHandle {
    /// Create a handle and the actor's half of it. [`attach_task`](Self::attach_task) records
    /// the task once it is spawned.
    pub fn channel(id: AppId, kind: HandleKind, initial: Snapshot) -> (Self, HandleParts) {
        let (tx, rx) = mpsc::channel(MAILBOX);
        let markers = Arc::new(Markers::from_spec(&initial.spec));
        let (snap_tx, snap_rx) = watch::channel(Arc::new(initial));
        let live = Arc::new(Live::default());
        let kill = CancellationToken::new();
        let closed = Arc::new(AtomicBool::new(false));
        let weak_tx = tx.downgrade();
        let handle = Self {
            id,
            kind,
            tx,
            snap: snap_rx,
            live: Arc::clone(&live),
            markers,
            kill: kill.clone(),
            closed: Arc::clone(&closed),
            abort: None,
        };
        let parts = HandleParts {
            weak_tx,
            rx,
            snap_tx,
            live,
            kill,
            closed,
        };
        (handle, parts)
    }

    pub fn attach_task(&mut self, abort: AbortHandle) {
        self.abort = Some(abort);
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snap.borrow())
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snap.clone()
    }

    pub fn live(&self) -> &Arc<Live> {
        &self.live
    }

    /// The policy markers (`protected`, `ai_control`) of this app.
    pub fn markers(&self) -> &Markers {
        &self.markers
    }

    /// The snapshot's state with the numbers other tasks keep (`uptime_secs`, cpu, memory,
    /// health) filled in.
    pub fn run_state(&self) -> RunState {
        let snap = self.snapshot();
        let mut state = snap.state.clone();
        if state.pid.is_some() {
            if let Some(started) = state.started_at {
                state.uptime_secs = (Utc::now() - started).num_seconds().max(0) as u64;
            }
            state.cpu_percent = self.live.cpu_percent();
            state.memory_bytes = self.live.mem_bytes();
            state.health_check_failures = self.live.health_failures();
            state.last_health_check = self.live.last_health();
        }
        state
    }

    /// Hard stop: the actor SIGKILLs its group and exits. The second-signal escalation and the
    /// shutdown fallback use this, so a pid is never signalled from outside the actor.
    pub fn kill_now(&self) {
        self.kill.cancel();
    }

    /// Give up on the actor task (the last resort after `kill_now`).
    pub fn abort(&self) {
        if let Some(abort) = &self.abort {
            abort.abort();
        }
    }

    /// Mark the handle closed: [`request`](Self::request) refuses from now on.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send a command and wait for its answer. Refuses with `ShuttingDown` once the handle is
    /// closed, `Busy` when the mailbox stays full, `Internal` when the actor died.
    pub async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        if self.is_closed() {
            return Err(Error::ShuttingDown);
        }
        self.request_unchecked(make).await
    }

    /// [`request`](Self::request) without the closed check (the shutdown's own command).
    pub async fn request_unchecked<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.send(make(reply)).await?;
        match answer.await {
            Ok(result) => result,
            Err(_) => Err(Error::Internal("the app's task died".to_string())),
        }
    }

    async fn send(&self, command: Command) -> Result<()> {
        match self.tx.send_timeout(command, SEND_TIMEOUT).await {
            Ok(()) => Ok(()),
            Err(mpsc::error::SendTimeoutError::Timeout(_)) => Err(Error::Busy(format!(
                "app {} is not taking commands",
                self.id
            ))),
            Err(mpsc::error::SendTimeoutError::Closed(_)) => {
                Err(Error::Internal("the app's task is gone".to_string()))
            }
        }
    }

    /// Post a command from a task that cannot wait. False if the mailbox is full or gone.
    pub fn try_post(&self, command: Command) -> bool {
        self.tx.try_send(command).is_ok()
    }
}

impl std::fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessHandle")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish()
    }
}

/// Post a command from a task that does not own the mailbox. False if the mailbox is full or
/// the app is gone.
pub fn post(tx: &mpsc::WeakSender<Command>, command: Command) -> bool {
    match tx.upgrade() {
        Some(tx) => tx.try_send(command).is_ok(),
        None => false,
    }
}
