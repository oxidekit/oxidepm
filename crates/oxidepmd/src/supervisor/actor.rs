//! The process actor: one task per app that exclusively owns the child.
//!
//! It takes commands from its mailbox in order, observes the child's exit with `child.wait()`,
//! runs the launch pipeline (dependency gate, delay, build) as a future it can drop, and
//! publishes a [`Snapshot`] after every loop turn. Nothing else touches the child, so a stop
//! always wins against a pending build, a backoff timer or a crash restart.
//!
//! Automatic restarts follow the pure [`CrashTracker`] of `oxidepm-core`: a doubling delay, a
//! rolling window for the crash loop breaker, and a stable run starts the delay over.

use super::deps::{self, Verdict};
use super::events::{EventDetail, EventDraft, Lifecycle, LifecycleKind};
use super::handle::{
    post, AppId, Cause, CmdCtx, Command, HandleKind, HandleParts, LimitKind, Live, LiveProc,
    ProcessHandle, RestartCause, RestartKind, RestartOutcome, Snapshot, StartOrigin, StartOutcome,
    StopOutcome,
};
use super::health_task::{self, HealthTask};
use super::pids::{self, PidGuard, PidRecord, PidTable};
use super::registry::Registry;
use super::stats::{Counted, Kind, Stats};
use super::store::AppStore;
use crate::adapters::runtime::{spawn_runtime, uses_pinned_binary};
use chrono::{DateTime, Utc};
use nix::sys::signal::Signal;
use oxidepm_core::restart::{CrashTracker, Decision, ExitCause};
use oxidepm_core::validate::{MAX_APP_TIMER_MS, MAX_APP_TIMER_SECS};
use oxidepm_core::{AppSpec, AppStatus, Desired, Error, HealthAction, Paths, Result, RunState};
use oxidepm_health::ScriptContext;
use oxidepm_logs::{LogCapture, LogControl, RotationConfig};
use oxidepm_runtime::{
    build_env, group_exists, kill_group, script_env, spawn_group, terminate_group, BuiltinRuntimes,
    GroupChild, LaunchPlan, Origin, Runner, RuntimeSource, Terminated,
};
#[cfg(feature = "test-hooks")]
use parking_lot::Mutex;
use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// How long the capture tasks get to drain after the group is gone.
const CAPTURE_JOIN: Duration = Duration::from_secs(2);

/// Grace given to members of the group that outlived an exited leader, before they are killed.
const ORPHAN_GRACE: Duration = Duration::from_secs(1);

/// Where the pipeline gets the runner for a runtime (tests replace it with a fake that counts
/// `prepare` calls).
pub type RuntimeSourceRef = Arc<dyn RuntimeSource>;

/// What every actor shares.
pub struct ActorCtx {
    pub store: AppStore,
    pub paths: Arc<Paths>,
    pub lifecycle: Arc<Lifecycle>,
    /// Weak: the registry holds the handles, and a handle must be able to end the actor.
    pub registry: Weak<Registry>,
    pub stats: Arc<Stats>,
    /// The live processes, mirrored to `pids.json` for the orphan sweep.
    pub pids: Arc<PidTable>,
    pub runtimes: RuntimeSourceRef,
    /// Bounds the apps that are in their build-and-spawn phase at once. A permit is taken only
    /// after the dependency gate has passed and given back when the launch is over, so an app
    /// that waits for its dependencies never holds one (otherwise dependents could starve the
    /// dependency they wait for).
    pub launch_slots: Arc<Semaphore>,
    /// Test hook: holds a launch between the spawn of its child and the moment it is recorded.
    #[cfg(feature = "test-hooks")]
    pub spawn_pause: Mutex<Option<SpawnPause>>,
}

/// The two ends of the pause set by `Supervisor::pause_starts_after_spawn`.
#[cfg(feature = "test-hooks")]
#[derive(Clone)]
pub struct SpawnPause {
    /// Notified (one permit) by a launch that has spawned its child and now waits.
    pub reached: Arc<Notify>,
    /// Notify it (one permit) to let the waiting launch go on.
    pub release: Arc<Notify>,
}

impl ActorCtx {
    pub fn registry(&self) -> Option<Arc<Registry>> {
        self.registry.upgrade()
    }

    pub fn default_runtimes() -> RuntimeSourceRef {
        Arc::new(BuiltinRuntimes)
    }
}

/// A task that is aborted when this is dropped.
pub struct AbortOnDrop(JoinHandle<()>, #[allow(dead_code)] Option<Counted>);

impl AbortOnDrop {
    pub fn new(handle: JoinHandle<()>) -> Self {
        Self(handle, None)
    }

    /// The task is counted for as long as this value lives (so the counter drops with the
    /// owner, not whenever the runtime gets round to dropping the aborted future).
    pub fn counted(handle: JoinHandle<()>, counted: Counted) -> Self {
        Self(handle, Some(counted))
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How an idle actor starts out.
pub enum Initial {
    Idle,
    /// After a panic: errored with this message.
    Failed(String),
    /// The stored spec cannot be read (4.1): errored with this message, `Start` and `Restart`
    /// are refused, `Delete` works.
    Broken(String),
}

/// What it takes to create an actor.
pub struct ActorInit {
    pub spec: Arc<AppSpec>,
    pub desired: Desired,
    pub parent_id: Option<AppId>,
    /// The row was inserted by the start that is about to be sent (stopped); a failed first
    /// launch takes it out again.
    pub created_pending: bool,
    pub initial: Initial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Stopped,
    Waiting,
    Building,
    /// Spawned, and the first probe has not passed yet (only for an app with a health check).
    Starting {
        since: Instant,
    },
    Running {
        since: Instant,
    },
    /// Alive, but the health check failed `retries` times in a row.
    Unhealthy {
        since: Instant,
    },
    Backoff {
        until: Instant,
    },
    Stopping,
    Failed,
}

impl State {
    fn has_child(&self) -> bool {
        matches!(
            self,
            State::Starting { .. }
                | State::Running { .. }
                | State::Unhealthy { .. }
                | State::Stopping
        )
    }

    fn is_active(&self) -> bool {
        !matches!(self, State::Stopped | State::Failed)
    }

    fn is_terminal(&self) -> bool {
        matches!(self, State::Stopped | State::Failed)
    }
}

/// How the launch in flight was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// An operator start (from stopped or errored): the dependency gate applies, the event is
    /// `Started`.
    First,
    /// An operator restart of an app that was up: no gate, the event is `Restarted`.
    Restart(Cause),
    /// A launch the actor decided on (crash restart, cron): the cached plan, no gate.
    Auto(Cause),
}

impl Launch {
    fn cause(&self) -> Cause {
        match self {
            Launch::First => Cause::User,
            Launch::Restart(c) | Launch::Auto(c) => *c,
        }
    }
}

enum PipelineMode {
    /// Operator launch: gate (when `gate`), startup delay, then `prepare` and `plan`.
    Operator { gate: bool },
    /// Automatic launch: the cached plan.
    Cached,
}

#[derive(Clone)]
struct PreparedLaunch {
    plan: LaunchPlan,
    invocation: Option<PathBuf>,
    pin_owner: Option<Arc<dyn Runner>>,
}

enum PipelineResult {
    /// The plan to launch, and the launch slot to hold until the launch is over (an operator
    /// launch; an automatic one needs no build and takes none).
    Ready(PreparedLaunch, Option<OwnedSemaphorePermit>, Vec<String>),
    Failed(Error),
}

struct Pipeline {
    fut: Pin<Box<dyn Future<Output = PipelineResult> + Send>>,
    building: Arc<AtomicBool>,
    _count: Counted,
}

enum Pending {
    Start(oneshot::Sender<Result<StartOutcome>>),
    Restart(oneshot::Sender<Result<RestartOutcome>>),
}

/// The capture tasks of a launch, and the handle to the writers they own.
struct Capture {
    handles: Vec<JoinHandle<()>>,
    control: LogControl,
    _counts: Vec<Counted>,
}

enum Flow {
    Continue,
    Exit,
}

pub struct ProcessActor {
    id: AppId,
    spec: Arc<AppSpec>,
    state: State,
    desired: Desired,
    parent_id: Option<AppId>,
    rx: mpsc::Receiver<Command>,
    weak_tx: mpsc::WeakSender<Command>,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    live: Arc<Live>,
    kill: CancellationToken,
    closed: Arc<AtomicBool>,
    ctx: Arc<ActorCtx>,
    child: Option<GroupChild>,
    capture: Option<Capture>,
    plan: Option<PreparedLaunch>,
    /// Owns the verified executable for as long as the child may reopen it.
    running_pin: Option<Arc<dyn Runner>>,
    /// Cancelled when the child goes away; the health task is tied to it.
    launch_token: Option<CancellationToken>,
    health: Option<AbortOnDrop>,
    gen: u64,
    cron_epoch: u64,
    cron: Option<AbortOnDrop>,
    /// When the running child reaches `max_uptime_secs`; an arm of the loop, nothing to cancel.
    uptime_deadline: Option<Instant>,
    pipeline: Option<Pipeline>,
    /// The `pids.json` record of the live child; dropped with it.
    pid_record: Option<PidGuard>,
    /// A placeholder for a stored spec that does not parse: it cannot be started.
    broken: bool,
    /// Tells the loop that the pipeline moved from waiting to building.
    phase: Arc<Notify>,
    launch_kind: Launch,
    /// Who asked for the launch in flight (an adapter may use a stored plan only for a
    /// resurrect or an automatic launch).
    launch_origin: Origin,
    /// What the pipeline of the launch in flight has to say, told with the `Started` event.
    launch_warnings: Vec<String>,
    pending: Vec<Pending>,
    created_pending: bool,
    restarts: u32,
    /// The restart policy's memory: recent exits and the attempt count.
    tracker: CrashTracker,
    backoff_attempt: u32,
    /// Why the pending automatic restart was scheduled.
    backoff_cause: Cause,
    started_at: Option<DateTime<Utc>>,
    /// Pid of the last child (still named in the events about its end).
    last_pid: Option<u32>,
    /// Who sent the command being handled (copied into the events it causes).
    cmd_actor: Option<Arc<str>>,
    last_exit_code: Option<i32>,
    last_exit_signal: Option<i32>,
    last_error: Option<String>,
    _alive: Counted,
}

/// Create an actor, spawn its task and return its handle.
pub fn spawn_process_actor(ctx: &Arc<ActorCtx>, init: ActorInit) -> ProcessHandle {
    let ActorInit {
        spec,
        desired,
        parent_id,
        created_pending,
        initial,
    } = init;
    let id = spec.id;
    let snapshot = Snapshot::new(Arc::clone(&spec), desired, parent_id);
    let (mut handle, parts) = ProcessHandle::channel(id, HandleKind::Process, snapshot);
    let HandleParts {
        rx,
        weak_tx,
        snap_tx,
        live,
        kill,
        closed,
    } = parts;

    let tracker = CrashTracker::new(spec.restart_policy.clone());
    let mut actor = ProcessActor {
        id,
        spec,
        state: State::Stopped,
        desired,
        parent_id,
        rx,
        weak_tx,
        snap_tx,
        live,
        kill,
        closed,
        ctx: Arc::clone(ctx),
        child: None,
        capture: None,
        plan: None,
        running_pin: None,
        launch_token: None,
        health: None,
        gen: 0,
        cron_epoch: 0,
        cron: None,
        uptime_deadline: None,
        pipeline: None,
        pid_record: None,
        broken: false,
        phase: Arc::new(Notify::new()),
        launch_kind: Launch::First,
        launch_origin: Origin::Start,
        launch_warnings: Vec::new(),
        pending: Vec::new(),
        created_pending,
        restarts: 0,
        tracker,
        backoff_attempt: 0,
        backoff_cause: Cause::Crash,
        started_at: None,
        last_pid: None,
        cmd_actor: None,
        last_exit_code: None,
        last_exit_signal: None,
        last_error: None,
        _alive: Counted::new(&ctx.stats, Kind::Actor),
    };
    match initial {
        Initial::Idle => {}
        Initial::Failed(message) => {
            actor.state = State::Failed;
            actor.last_error = Some(message);
        }
        Initial::Broken(message) => {
            actor.state = State::Failed;
            actor.last_error = Some(message);
            actor.broken = true;
        }
    }
    actor.publish();

    let join = tokio::spawn(actor.run());
    handle.attach_task(join.abort_handle());

    // The panic watcher: a panic left the group killed by `GroupChild::drop`; put an
    // errored, idle actor in the same registry slot so the registry never keeps a dead mailbox.
    let ctx = Arc::clone(ctx);
    let watched = handle.clone();
    tokio::spawn(async move {
        if let Err(e) = join.await {
            if e.is_panic() {
                error!("The task of app {} panicked", watched.id);
                respawn_after_panic(&ctx, &watched);
            }
        }
    });
    handle
}

fn respawn_after_panic(ctx: &Arc<ActorCtx>, dead: &ProcessHandle) {
    let snap = dead.snapshot();
    let init = ActorInit {
        spec: Arc::clone(&snap.spec),
        desired: snap.desired,
        parent_id: snap.parent_id,
        created_pending: false,
        initial: Initial::Failed("internal error: actor panicked".to_string()),
    };
    let fresh = spawn_process_actor(ctx, init);
    // the markers are the handle's, not the spec's: the new handle keeps what the dead one had
    fresh
        .markers()
        .set(dead.markers().protected(), dead.markers().ai_control());
    if let Some(registry) = ctx.registry() {
        registry.replace(fresh);
    }
}

fn exit_parts(status: &ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), status.signal())
}

impl ProcessActor {
    async fn run(mut self) {
        loop {
            let backoff = self.backoff_until();
            let uptime = self.uptime_deadline;
            let flow = tokio::select! {
                biased;
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => Flow::Exit,
                },
                status = wait_child(&mut self.child), if self.child.is_some() => {
                    self.on_exit(status).await
                }
                res = poll_pipeline(&mut self.pipeline), if self.pipeline.is_some() => {
                    self.on_pipeline(res).await
                }
                _ = sleep_until_opt(backoff), if backoff.is_some() => {
                    self.on_backoff_elapsed().await
                }
                _ = sleep_until_opt(uptime), if uptime.is_some() => {
                    self.on_uptime_limit().await;
                    Flow::Continue
                }
                _ = self.phase.notified() => {
                    self.on_phase();
                    Flow::Continue
                }
                _ = self.kill.cancelled() => {
                    self.hard_kill();
                    Flow::Exit
                }
            };
            self.publish();
            if let Flow::Exit = flow {
                break;
            }
        }
        self.finish();
    }

    /// The end of the actor: queued commands are answered, nothing is left running.
    fn finish(&mut self) {
        self.rx.close();
        let shutting_down = self.closed.load(Ordering::SeqCst);
        while let Ok(cmd) = self.rx.try_recv() {
            let err = || {
                if shutting_down {
                    Error::ShuttingDown
                } else {
                    Error::AppNotFound(self.spec.name.clone())
                }
            };
            match cmd {
                Command::Start { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Stop { reply, .. } | Command::Delete { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Restart { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Shutdown { reply, .. } => {
                    let _ = reply.send(Ok(false));
                }
                Command::FlushLogs { reply } => {
                    let _ = reply.send(Err(err()));
                }
                _ => {}
            }
        }
        // `GroupChild::drop` kills the group if the child is still armed (a panic, an abort).
        self.child = None;
    }

    fn backoff_until(&self) -> Option<Instant> {
        match self.state {
            State::Backoff { until } => Some(until),
            _ => None,
        }
    }

    // ---- publishing --------------------------------------------------------------------

    fn run_state(&self) -> RunState {
        let status = match self.state {
            State::Stopped => AppStatus::Stopped,
            State::Waiting => AppStatus::Waiting,
            State::Building => AppStatus::Building,
            State::Starting { .. } => AppStatus::Starting,
            State::Running { .. } => AppStatus::Running,
            State::Unhealthy { .. } => AppStatus::Unhealthy,
            State::Backoff { .. } => AppStatus::Backoff,
            State::Stopping => AppStatus::Stopping,
            State::Failed => AppStatus::Errored,
        };
        let pid = if self.state.has_child() {
            self.live.proc().map(|p| p.pid)
        } else {
            None
        };
        let backoff_until = match self.state {
            State::Backoff { until } => {
                let left = until.saturating_duration_since(Instant::now());
                Some(Utc::now() + chrono::Duration::from_std(left).unwrap_or_default())
            }
            _ => None,
        };
        RunState {
            app_id: self.id,
            pid,
            status,
            restarts: self.restarts,
            uptime_secs: 0,
            cpu_percent: 0.0,
            memory_bytes: 0,
            last_exit_code: self.last_exit_code,
            started_at: self.started_at,
            healthy: matches!(self.state, State::Running { .. }),
            last_health_check: None,
            health_check_failures: 0,
            port: self.spec.port,
            instance_id: self.spec.instance_id,
            backoff_attempt: if backoff_until.is_some() {
                self.backoff_attempt
            } else {
                0
            },
            backoff_until,
            last_error: self.last_error.clone(),
            last_exit_signal: self.last_exit_signal,
        }
    }

    fn publish(&mut self) {
        let snapshot = Snapshot {
            spec: Arc::clone(&self.spec),
            state: self.run_state(),
            desired: self.desired,
            parent_id: self.parent_id,
            children: Vec::new(),
            generation: self.gen,
        };
        self.snap_tx.send_replace(Arc::new(snapshot));
    }

    fn detail(&self) -> EventDetail {
        let mut detail = EventDetail::new(&self.spec);
        detail.pid = self.last_pid;
        detail.exit_code = self.last_exit_code;
        detail.exit_signal = self.last_exit_signal;
        detail.restarts = self.restarts;
        detail
    }

    fn emit(&self, kind: LifecycleKind, cause: Cause, detail: EventDetail) {
        self.ctx.lifecycle.emit(EventDraft {
            app_id: self.id,
            kind,
            cause,
            actor: self.event_actor(cause),
            detail,
        });
    }

    /// Who an event is credited to: the actor of the last command for what a command caused,
    /// and the daemon itself (`system:<cause>`) for what the actor decided on its own (crash and
    /// limit restarts, cron, health), so a later crash is never blamed on whoever started the app.
    fn event_actor(&self, cause: Cause) -> Option<Arc<str>> {
        match cause {
            Cause::User | Cause::Resurrect | Cause::Reload | Cause::Group | Cause::Adapter => {
                self.cmd_actor.clone()
            }
            Cause::Crash => Some(Arc::from("system:crash")),
            Cause::Limit => Some(Arc::from("system:limit")),
            Cause::Cron => Some(Arc::from("system:cron")),
            Cause::Health => Some(Arc::from("system:health")),
        }
    }

    /// Note who is asking, for the log and for the events that follow.
    fn note(&mut self, what: &str, ctx: &CmdCtx) {
        debug!(
            "{} of {} by {:?} ({:?})",
            what, self.spec.name, ctx.actor, ctx.cause
        );
        self.cmd_actor = ctx.actor.clone();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    // ---- commands ----------------------------------------------------------------------

    async fn handle(&mut self, cmd: Command) -> Flow {
        match cmd {
            Command::Start {
                spec,
                origin,
                created,
                ctx,
                reply,
            } => {
                self.on_start(spec, origin, created, ctx, reply).await;
                Flow::Continue
            }
            Command::Stop { ctx, reply } => {
                self.on_stop(ctx, reply).await;
                Flow::Continue
            }
            Command::Restart { cause, ctx, reply } => {
                self.on_restart(cause, ctx, reply).await;
                Flow::Continue
            }
            Command::Delete {
                row_removed,
                ctx,
                reply,
            } => self.on_delete(row_removed, ctx, reply).await,
            Command::FlushLogs { reply } => {
                let result = self.flush_logs().await;
                let _ = reply.send(result);
                Flow::Continue
            }
            Command::Shutdown { kill_cap, reply } => self.on_shutdown(kill_cap, reply).await,
            Command::HealthOk { gen } => {
                self.on_health_ok(gen);
                Flow::Continue
            }
            Command::HealthFailed {
                gen,
                failures,
                message,
            } => {
                self.on_health_failed(gen, failures, message).await;
                Flow::Continue
            }
            Command::CronRestart { cron_epoch } => {
                self.on_cron(cron_epoch).await;
                Flow::Continue
            }
            Command::LimitExceeded { gen, kind } => {
                self.on_limit(gen, kind).await;
                Flow::Continue
            }
            #[cfg(feature = "test-hooks")]
            Command::TestPanic => panic!("test panic requested"),
        }
    }

    /// Empty the log files. While a child runs its capture owns the writers, so they do it
    /// themselves (and agree with the files afterwards); with no live capture no writer exists
    /// to disagree and the files are truncated directly.
    async fn flush_logs(&mut self) -> Result<()> {
        match &self.capture {
            Some(capture) => {
                let control = capture.control.clone();
                tokio::task::spawn_blocking(move || control.truncate())
                    .await
                    .map_err(|e| Error::Internal(format!("log flush task: {}", e)))?
            }
            None => truncate_log_files(&self.ctx.paths, &self.spec.name),
        }?;
        info!("Flushed logs for {}", self.spec.name);
        Ok(())
    }

    async fn on_start(
        &mut self,
        spec: Arc<AppSpec>,
        origin: StartOrigin,
        created: bool,
        ctx: CmdCtx,
        reply: oneshot::Sender<Result<StartOutcome>>,
    ) {
        self.note("start", &ctx);
        self.launch_origin = if origin == StartOrigin::Resurrect {
            Origin::Resurrect
        } else {
            Origin::Start
        };
        if self.is_closed() {
            let _ = reply.send(Err(Error::ShuttingDown));
            return;
        }
        if self.broken {
            let _ = reply.send(Err(self.broken_error()));
            return;
        }
        if !self.state.is_terminal() {
            // a resurrect that finds the app up has nothing to do
            let result = if origin == StartOrigin::Resurrect {
                Ok(self.outcome(false))
            } else {
                Err(Error::AppAlreadyExists(self.spec.name.clone()))
            };
            let _ = reply.send(result);
            return;
        }
        // the desired state is read here, at accept time, so a stop that landed after
        // the resurrect chose its targets is never overridden
        if origin == StartOrigin::Resurrect && self.desired != Desired::Running {
            let _ = reply.send(Ok(self.outcome(false)));
            return;
        }
        // an actor made for a fresh row knows it, whoever sends the command
        let created = created || self.created_pending;

        let mut spec = (*spec).clone();
        spec.id = self.id;
        if !created {
            // a start does not change when the app was created
            spec.created_at = self.spec.created_at;
            if let Err(e) = self.ctx.store.update_spec(self.id, &spec).await {
                let _ = reply.send(Err(e));
                return;
            }
            if let Err(e) = self.ctx.store.set_desired(self.id, Desired::Running).await {
                let _ = reply.send(Err(e));
                return;
            }
            self.desired = Desired::Running;
        }
        self.spec = Arc::new(spec);
        self.created_pending = created;
        self.reset_for_operator_launch();
        self.arm_cron();
        if self.deps_not_ready_yet() {
            // dependencies that are not running yet: the caller is told at once and the start
            // finishes (or fails closed) in the background (2.3)
            self.begin_pipeline(PipelineMode::Operator { gate: true }, Launch::First);
            self.accept_as_waiting().await;
            self.publish();
            let _ = reply.send(Ok(self.outcome(true)));
            return;
        }
        self.pending.push(Pending::Start(reply));
        self.begin_pipeline(PipelineMode::Operator { gate: true }, Launch::First);
    }

    /// Do the dependencies say "wait" right now? (An errored or vanished one is not a wait: the
    /// pipeline fails at its first poll and the caller hears of it.)
    fn deps_not_ready_yet(&self) -> bool {
        if self.spec.depends_on.is_empty() {
            return false;
        }
        match self.ctx.registry() {
            Some(registry) => matches!(deps::check_now(&registry, &self.spec), Verdict::Wait(_)),
            None => false,
        }
    }

    /// A start that was accepted while its dependencies are not running is as good as launched
    /// for the row: the app is meant to run (a crash of the daemon resurrects it), and a later
    /// failure of the gate does not take the row away.
    async fn accept_as_waiting(&mut self) {
        if self.created_pending {
            match self.ctx.store.set_desired(self.id, Desired::Running).await {
                Ok(()) => self.desired = Desired::Running,
                Err(e) => warn!(
                    "Could not persist the desired state of {}: {}",
                    self.spec.name, e
                ),
            }
            self.created_pending = false;
        }
    }

    /// The answer to a `Start` that is not waiting for a launch.
    fn outcome(&self, changed: bool) -> StartOutcome {
        StartOutcome {
            id: self.id,
            name: self.spec.name.clone(),
            status: self.run_state().status,
            child_ids: Vec::new(),
            changed,
        }
    }

    /// What a `Start` or `Restart` of a placeholder gets.
    fn broken_error(&self) -> Error {
        Error::StateIncompatible(
            self.last_error
                .clone()
                .unwrap_or_else(|| "the stored spec is invalid".to_string()),
        )
    }

    /// Counters and observed data start afresh when an operator starts an app that was at rest.
    fn reset_for_operator_launch(&mut self) {
        self.restarts = 0;
        // the policy may have changed with the spec
        self.tracker = CrashTracker::new(self.spec.restart_policy.clone());
        self.backoff_attempt = 0;
        self.last_error = None;
        self.last_exit_code = None;
        self.last_exit_signal = None;
    }

    async fn on_stop(&mut self, ctx: CmdCtx, reply: oneshot::Sender<Result<StopOutcome>>) {
        self.note("stop", &ctx);
        let warning = self.persist_desired(Desired::Stopped).await;
        let changed = match self.state {
            State::Stopped => false,
            State::Failed => {
                self.last_error = None;
                self.state = State::Stopped;
                false
            }
            State::Waiting | State::Building => {
                self.drop_pipeline();
                self.created_pending = false;
                self.cron = None;
                self.state = State::Stopped;
                // after the state is set: the callers hear of it with the new state published
                self.reject_pending(|| {
                    Error::Cancelled("the start was cancelled by a stop".into())
                });
                true
            }
            State::Backoff { .. } => {
                self.cron = None;
                self.state = State::Stopped;
                true
            }
            State::Starting { .. }
            | State::Running { .. }
            | State::Unhealthy { .. }
            | State::Stopping => {
                self.cron = None;
                self.terminate(None).await;
                self.state = State::Stopped;
                let detail = self.detail();
                self.emit(LifecycleKind::Stopped, Cause::User, detail);
                true
            }
        };
        // the snapshot is published before the answer, so a caller that reads right after
        // the reply sees the new state
        self.publish();
        let _ = reply.send(Ok(StopOutcome { changed, warning }));
    }

    /// Persist the desired state; the message of a failure (the caller goes on regardless).
    async fn persist_desired(&mut self, desired: Desired) -> Option<String> {
        self.desired = desired;
        match self.ctx.store.set_desired(self.id, desired).await {
            Ok(()) => None,
            Err(e) => {
                warn!(
                    "Could not persist the desired state of {}: {}",
                    self.spec.name, e
                );
                Some(format!("could not save the desired state: {}", e))
            }
        }
    }

    async fn on_restart(
        &mut self,
        cause: RestartCause,
        ctx: CmdCtx,
        reply: oneshot::Sender<Result<RestartOutcome>>,
    ) {
        self.note("restart", &ctx);
        self.launch_origin = Origin::Restart;
        if self.is_closed() {
            let _ = reply.send(Err(Error::ShuttingDown));
            return;
        }
        if self.broken {
            let _ = reply.send(Err(self.broken_error()));
            return;
        }
        match self.state {
            State::Stopped | State::Failed => {
                if self.desired != Desired::Running {
                    if let Err(e) = self.ctx.store.set_desired(self.id, Desired::Running).await {
                        let _ = reply.send(Err(e));
                        return;
                    }
                    self.desired = Desired::Running;
                }
                self.reset_for_operator_launch();
                self.arm_cron();
                if self.deps_not_ready_yet() {
                    self.begin_pipeline(PipelineMode::Operator { gate: true }, Launch::First);
                    self.publish();
                    let status = self.run_state().status;
                    let _ = reply.send(Ok(RestartOutcome {
                        kind: RestartKind::Restart,
                        status,
                    }));
                    return;
                }
                self.pending.push(Pending::Restart(reply));
                self.begin_pipeline(PipelineMode::Operator { gate: true }, Launch::First);
            }
            // the pipeline starts over; earlier replies wait for the new one
            State::Waiting | State::Building => {
                self.drop_pipeline();
                self.tracker.reset();
                self.backoff_attempt = 0;
                self.pending.push(Pending::Restart(reply));
                self.begin_pipeline(PipelineMode::Operator { gate: true }, Launch::First);
            }
            State::Backoff { .. } => {
                // the operator's restart replaces the scheduled one and starts a fresh budget
                self.tracker.reset();
                self.backoff_attempt = 0;
                self.pending.push(Pending::Restart(reply));
                self.begin_pipeline(
                    PipelineMode::Operator { gate: false },
                    Launch::Restart(cause.cause()),
                );
            }
            State::Starting { .. }
            | State::Running { .. }
            | State::Unhealthy { .. }
            | State::Stopping => {
                self.terminate(None).await;
                self.restarts += 1;
                self.pending.push(Pending::Restart(reply));
                self.begin_pipeline(
                    PipelineMode::Operator { gate: false },
                    Launch::Restart(cause.cause()),
                );
            }
        }
    }

    async fn on_delete(
        &mut self,
        row_removed: bool,
        ctx: CmdCtx,
        reply: oneshot::Sender<Result<StopOutcome>>,
    ) -> Flow {
        self.note("delete", &ctx);
        // the row goes first; if that fails the actor stays
        if !row_removed {
            if let Err(e) = self.ctx.store.delete(self.id).await {
                let _ = reply.send(Err(e));
                return Flow::Continue;
            }
        }
        let was_active = self.state.is_active();
        let had_child = self.state.has_child();
        self.drop_pipeline();
        self.created_pending = false;
        self.reject_pending(|| Error::Cancelled("the app was deleted".into()));
        self.cron = None;
        if had_child {
            self.terminate(None).await;
            let detail = self.detail();
            self.emit(LifecycleKind::Stopped, Cause::User, detail);
        }
        self.state = State::Stopped;
        if let Some(registry) = self.ctx.registry() {
            registry.remove(self.id);
        }
        // the plan the runtime adapter made for it goes too: an app created later under this
        // name must not find it
        self.ctx.runtimes.forget(&self.spec.name).await;
        info!("Deleted app {} (id: {})", self.spec.name, self.id);
        self.publish();
        let _ = reply.send(Ok(StopOutcome {
            changed: was_active,
            warning: None,
        }));
        Flow::Exit
    }

    /// Discard a newly created app whose first start was never accepted.
    async fn discard_pending_start(&mut self) -> bool {
        if !std::mem::take(&mut self.created_pending) {
            return false;
        }
        if let Err(e) = self.ctx.store.delete(self.id).await {
            warn!(
                "Could not remove the row of an app that did not start (id {}): {}",
                self.id, e
            );
        }
        if let Some(registry) = self.ctx.registry() {
            registry.remove(self.id);
        }
        self.ctx.runtimes.forget(&self.spec.name).await;
        true
    }

    async fn on_shutdown(
        &mut self,
        kill_cap: Duration,
        reply: oneshot::Sender<Result<bool>>,
    ) -> Flow {
        // `desired` is left as it is, so the app comes back on resurrect
        let was_active = self.state.is_active();
        let had_child = self.state.has_child();
        self.closed.store(true, Ordering::SeqCst);
        self.drop_pipeline();
        self.discard_pending_start().await;
        self.reject_pending(|| Error::ShuttingDown);
        self.cron = None;
        if had_child {
            self.terminate(Some(kill_cap)).await;
            let detail = self.detail();
            self.emit(LifecycleKind::Stopped, Cause::User, detail);
        }
        self.state = State::Stopped;
        self.publish();
        let _ = reply.send(Ok(was_active));
        Flow::Exit
    }

    /// a probe passed for the first time, or again after a streak.
    fn on_health_ok(&mut self, gen: u64) {
        if gen != self.gen {
            debug!(
                "Ignoring a stale health report for {} (gen {})",
                self.spec.name, gen
            );
            return;
        }
        match self.state {
            State::Starting { since } => {
                info!("App {} is healthy", self.spec.name);
                self.state = State::Running { since };
            }
            State::Unhealthy { since } => {
                info!("App {} recovered", self.spec.name);
                self.state = State::Running { since };
                let detail = self.detail();
                self.emit(LifecycleKind::Recovered, Cause::Health, detail);
            }
            _ => debug!(
                "Ignoring a health report for {} in {:?}",
                self.spec.name, self.state
            ),
        }
    }

    /// `retries` probes in a row failed.
    async fn on_health_failed(&mut self, gen: u64, failures: u32, message: String) {
        let since = match self.state {
            State::Starting { since } | State::Running { since } if gen == self.gen => since,
            // already reported, or the child is gone
            _ => {
                debug!(
                    "Ignoring a health report for {} (gen {})",
                    self.spec.name, gen
                );
                return;
            }
        };
        warn!(
            "App {} marked as unhealthy after {} failed checks: {}",
            self.spec.name, failures, message
        );
        let mut detail = self.detail();
        detail.reason = Some(message);
        self.emit(LifecycleKind::Unhealthy, Cause::Health, detail);

        match self.spec.health_check.as_ref().map(|hc| hc.action) {
            Some(HealthAction::Restart) => {
                // the child is replaced like after a crash, whatever the restart setting says
                let ran_for = since.elapsed();
                self.terminate(None).await;
                let next = self.decide(ExitCause::HealthFailed, ran_for);
                self.after_exit(Cause::Health, next);
            }
            _ => {
                self.state = State::Unhealthy { since };
            }
        }
    }

    async fn on_cron(&mut self, epoch: u64) {
        if epoch != self.cron_epoch
            || !matches!(
                self.state,
                State::Starting { .. } | State::Running { .. } | State::Unhealthy { .. }
            )
        {
            debug!("Ignoring a stale cron tick for {}", self.spec.name);
            return;
        }
        info!("Cron restart triggered for {}", self.spec.name);
        let detail = self.detail();
        self.emit(LifecycleKind::CronFired, Cause::Cron, detail);
        self.terminate(None).await;
        self.restarts += 1;
        self.begin_pipeline(PipelineMode::Cached, Launch::Auto(Cause::Cron));
    }

    /// the child of generation `gen` is over its memory limit, or has run for
    /// `max_uptime_secs`. It is replaced by the cached plan after the tracker's delay. The
    /// tracker is told it was a limit restart, so it never ends a periodic restart in a crash
    /// loop, while a limit that fires within seconds of every start still does.
    async fn on_limit(&mut self, gen: u64, kind: LimitKind) {
        let since = match self.state {
            State::Starting { since } | State::Running { since } | State::Unhealthy { since }
                if gen == self.gen && !self.is_closed() =>
            {
                since
            }
            // a stop, a restart or a newer child got there first
            _ => {
                debug!(
                    "Ignoring a stale limit report for {} (gen {})",
                    self.spec.name, gen
                );
                return;
            }
        };
        let mut detail = self.detail();
        let event = match kind {
            LimitKind::Memory {
                observed_mb,
                limit_mb,
            } => {
                warn!(
                    "Process {} (id: {}) exceeded its memory limit: {}MB > {}MB, restarting",
                    self.spec.name, self.id, observed_mb, limit_mb
                );
                detail.memory_mb = Some((observed_mb, limit_mb));
                LifecycleKind::MemoryLimit
            }
            LimitKind::Uptime { secs } => {
                warn!(
                    "Process {} (id: {}) reached its maximum uptime of {}s, restarting",
                    self.spec.name, self.id, secs
                );
                LifecycleKind::UptimeLimit
            }
        };
        self.emit(event, Cause::Limit, detail);
        let ran_for = since.elapsed();
        self.terminate(None).await;
        let next = self.decide(ExitCause::LimitRestart, ran_for);
        self.after_exit(Cause::Limit, next);
    }

    /// The uptime deadline of the running child passed.
    async fn on_uptime_limit(&mut self) {
        self.uptime_deadline = None;
        if let Some(secs) = self.spec.max_uptime_secs {
            self.on_limit(self.gen, LimitKind::Uptime { secs }).await;
        }
    }

    // ---- pipeline ----------------------------------------------------------------------

    fn begin_pipeline(&mut self, mode: PipelineMode, launch: Launch) {
        self.launch_kind = launch;
        let building = Arc::new(AtomicBool::new(false));
        let waits = match &mode {
            PipelineMode::Operator { gate } => {
                (*gate && !self.spec.depends_on.is_empty())
                    || (*gate && self.spec.startup_delay_ms.unwrap_or(0) > 0)
            }
            PipelineMode::Cached => false,
        };
        self.state = if waits {
            State::Waiting
        } else {
            State::Building
        };
        building.store(!waits, Ordering::SeqCst);
        let origin = if matches!(mode, PipelineMode::Cached) {
            Origin::Auto
        } else {
            self.launch_origin
        };
        let fut = run_pipeline(
            Arc::clone(&self.ctx),
            Arc::clone(&self.spec),
            mode,
            origin,
            self.plan.clone(),
            Arc::clone(&building),
            Arc::clone(&self.phase),
        );
        self.pipeline = Some(Pipeline {
            fut: Box::pin(fut),
            building,
            _count: Counted::new(&self.ctx.stats, Kind::Pipeline),
        });
    }

    fn drop_pipeline(&mut self) {
        self.pipeline = None;
    }

    fn on_phase(&mut self) {
        if let Some(pipeline) = &self.pipeline {
            let building = pipeline.building.load(Ordering::SeqCst);
            match self.state {
                State::Waiting if building => self.state = State::Building,
                // the final check found a dependency down again: back to waiting
                State::Building if !building => self.state = State::Waiting,
                _ => {}
            }
        }
    }

    async fn on_pipeline(&mut self, result: PipelineResult) -> Flow {
        self.pipeline = None;
        match result {
            PipelineResult::Ready(plan, slot, warnings) => {
                self.plan = plan.pin_owner.is_none().then(|| plan.clone());
                self.launch_warnings = warnings;
                let launched = self.launch(&plan).await;
                // the slot is free as soon as the launch is over, whatever came of it
                drop(slot);
                match launched {
                    Ok(()) => {
                        self.on_launched().await;
                        Flow::Continue
                    }
                    Err(e) => self.on_launch_failed(e).await,
                }
            }
            PipelineResult::Failed(e) => self.on_launch_failed(e).await,
        }
    }

    async fn on_launched(&mut self) {
        let launch = self.launch_kind;
        if self.created_pending {
            // the first launch worked: the app is now meant to be running
            match self.ctx.store.set_desired(self.id, Desired::Running).await {
                Ok(()) => self.desired = Desired::Running,
                Err(e) => warn!(
                    "Could not persist the desired state of {}: {}",
                    self.spec.name, e
                ),
            }
            self.created_pending = false;
        }
        // a launch that worked ends the explanation of an earlier failed one
        self.last_error = None;
        let kind = match launch {
            Launch::First => LifecycleKind::Started,
            Launch::Restart(_) | Launch::Auto(_) => LifecycleKind::Restarted,
        };
        let mut detail = self.detail();
        if !self.launch_warnings.is_empty() {
            let text = self.launch_warnings.join("; ");
            warn!("{}: {}", self.spec.name, text);
            detail.reason = Some(text);
            self.launch_warnings.clear();
        }
        self.emit(kind, launch.cause(), detail);
        self.resolve_pending();
    }

    /// the launch did not work.
    async fn on_launch_failed(&mut self, e: Error) -> Flow {
        let shutting_down = matches!(e, Error::ShuttingDown);
        let message = e.to_string();
        error!("Launch of {} failed: {}", self.spec.name, message);
        if let (Launch::Auto(cause), false) = (self.launch_kind, shutting_down) {
            // an automatic relaunch that fails is one more attempt, not the end
            return self.on_auto_launch_failed(cause, message);
        }
        self.cron = None;
        self.state = if shutting_down {
            State::Stopped
        } else {
            State::Failed
        };
        if !shutting_down {
            self.last_error = Some(message.clone());
            let mut detail = self.detail();
            detail.reason = Some(message);
            self.emit(LifecycleKind::Errored, self.launch_kind.cause(), detail);
        }
        // An app that never ran is not registered: the row and the registry entry go before
        // the callers hear of the failure, in this loop turn, so nothing can be accepted in
        // between and a caller that looks right after the error finds nothing.
        let rolled_back = self.discard_pending_start().await;
        self.publish();
        for pending in self.pending.drain(..) {
            let err = clone_error(&e);
            match pending {
                Pending::Start(reply) => {
                    let _ = reply.send(Err(err));
                }
                Pending::Restart(reply) => {
                    let _ = reply.send(Err(err));
                }
            }
        }
        if rolled_back {
            Flow::Exit
        } else {
            Flow::Continue
        }
    }

    /// the cached plan could not be spawned again. The tracker counts it like a crash
    /// (longer delay, closer to the breaker); the operator's first launch is never retried.
    fn on_auto_launch_failed(&mut self, cause: Cause, message: String) -> Flow {
        self.last_exit_code = None;
        self.last_exit_signal = None;
        let next = self
            .decide(ExitCause::SpawnFailed, Duration::ZERO)
            .map_err(|reason| format!("{}; the last launch failed: {}", reason, message));
        if next.is_ok() {
            self.last_error = Some(format!("restart failed: {}", message));
        }
        self.after_exit(cause, next);
        Flow::Continue
    }

    fn resolve_pending(&mut self) {
        self.publish();
        let status = self.run_state().status;
        for pending in self.pending.drain(..) {
            match pending {
                Pending::Start(reply) => {
                    let _ = reply.send(Ok(StartOutcome {
                        id: self.id,
                        name: self.spec.name.clone(),
                        status,
                        child_ids: Vec::new(),
                        changed: true,
                    }));
                }
                Pending::Restart(reply) => {
                    let _ = reply.send(Ok(RestartOutcome {
                        kind: RestartKind::Restart,
                        status,
                    }));
                }
            }
        }
    }

    fn reject_pending(&mut self, err: impl Fn() -> Error) {
        self.publish();
        for pending in self.pending.drain(..) {
            match pending {
                Pending::Start(reply) => {
                    let _ = reply.send(Err(err()));
                }
                Pending::Restart(reply) => {
                    let _ = reply.send(Err(err()));
                }
            }
        }
    }

    // ---- launch ------------------------------------------------------------------------

    /// The one place an app process is created: the log files are opened before the
    /// spawn, so a log failure leaves no process, and the pipes are handed to the capture tasks
    /// before the child can fill them.
    async fn launch(&mut self, launch: &PreparedLaunch) -> Result<()> {
        let plan = &launch.plan;
        if self.is_closed() {
            return Err(Error::ShuttingDown);
        }
        std::fs::create_dir_all(self.ctx.paths.logs_dir())?;
        let capture = LogCapture::with_paths(
            self.ctx.paths.log_path(&self.spec.name, "out"),
            self.ctx.paths.log_path(&self.spec.name, "err"),
            RotationConfig::from_spec(&self.spec),
        )?;
        let env = build_env(&self.spec, &plan.extra_env);
        debug!(
            "Launching {}: {} {:?}",
            self.spec.name,
            plan.program.display(),
            plan.args
        );
        let mut child = match &launch.invocation {
            Some(invocation) => spawn_runtime(plan, invocation, &env)?,
            None => spawn_group(plan, &env)?,
        };
        let pid = child.pid();
        let pgid = child.pgid();
        // recorded with no await since the spawn: a SIGKILL of the daemon from here on leaves
        // a record for the next daemon's sweep
        let start_ticks = pids::start_ticks(pid);
        self.pid_record = Some(self.ctx.pids.add(PidRecord {
            app_id: self.id,
            name: self.spec.name.clone(),
            pid,
            pgid,
            start_ticks,
        }));
        let (stdout, stderr) = (child.take_stdout(), child.take_stderr());
        let tasks = capture.spawn_controlled_capture(stdout, stderr);

        let mut handles = Vec::new();
        let mut counts = Vec::new();
        for handle in [tasks.stdout, tasks.stderr].into_iter().flatten() {
            handles.push(handle);
            counts.push(Counted::new(&self.ctx.stats, Kind::Capture));
        }
        // from here the actor owns the child; a drop kills its group
        self.child = Some(child);
        self.running_pin = launch.pin_owner.clone();
        self.capture = Some(Capture {
            handles,
            control: tasks.control,
            _counts: counts,
        });

        #[cfg(feature = "test-hooks")]
        {
            let pause = self.ctx.spawn_pause.lock().clone();
            if let Some(pause) = pause {
                pause.reached.notify_one();
                // released by the test, or by the gate closing (a shutdown does not wait for it)
                loop {
                    tokio::select! {
                        _ = pause.release.notified() => break,
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {
                            if self.is_closed() {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // The gate closed while this launch was spawning: nothing may outlive the shutdown.
        if self.is_closed() {
            if let Some(mut child) = self.child.take() {
                child.kill_and_wait().await;
            }
            self.running_pin = None;
            self.pid_record = None;
            self.join_capture().await;
            return Err(Error::ShuttingDown);
        }

        self.gen += 1;
        self.live.set_proc(Some(LiveProc {
            pid,
            pgid,
            gen: self.gen,
            start_ticks,
        }));
        self.live.clear_limit_pending();
        self.live.clear_health();
        let token = CancellationToken::new();
        self.launch_token = Some(token.clone());
        self.started_at = Some(Utc::now());
        self.last_pid = Some(pid);
        let since = Instant::now();
        self.uptime_deadline = self
            .spec
            .max_uptime_secs
            .map(|secs| since + Duration::from_secs(secs.min(MAX_APP_TIMER_SECS)));
        info!("Started {} with PID {}", self.spec.name, pid);
        if self.spec.health_check.is_some() {
            // not running until the first probe has passed
            self.state = State::Starting { since };
            self.health = Some(health_task::spawn(HealthTask {
                spec: Arc::clone(&self.spec),
                live: Arc::clone(&self.live),
                tx: self.weak_tx.clone(),
                gen: self.gen,
                token,
                counted: Counted::new(&self.ctx.stats, Kind::Health),
            }));
        } else {
            self.state = State::Running { since };
        }
        Ok(())
    }

    /// (Re)arm the cron timer: one per activation, with its own epoch.
    fn arm_cron(&mut self) {
        self.cron = None;
        let Some(expr) = self.spec.restart_cron.clone() else {
            return;
        };
        let schedule = match oxidepm_core::cron::normalize_cron(&expr) {
            Ok(schedule) => schedule,
            Err(e) => {
                error!(
                    "Invalid cron expression '{}' for app {}: {}",
                    expr, self.spec.name, e
                );
                return;
            }
        };
        self.cron_epoch += 1;
        let epoch = self.cron_epoch;
        let tx = self.weak_tx.clone();
        let counted = Counted::new(&self.ctx.stats, Kind::Cron);
        let name = self.spec.name.clone();
        info!("Cron restart schedule active for app {}: {}", name, expr);
        self.cron = Some(AbortOnDrop::counted(
            tokio::spawn(async move {
                loop {
                    let Some(next) = schedule.upcoming(Utc).next() else {
                        warn!("No upcoming cron times for app {}", name);
                        return;
                    };
                    let until = (next - Utc::now())
                        .to_std()
                        .unwrap_or(Duration::from_millis(1));
                    debug!(
                        "App {} next cron restart in {:?} (at {})",
                        name, until, next
                    );
                    tokio::time::sleep(until).await;
                    if !post(&tx, Command::CronRestart { cron_epoch: epoch })
                        && tx.upgrade().is_none()
                    {
                        return;
                    }
                }
            }),
            counted,
        ));
    }

    // ---- exit and termination ---------------------------------------------------------

    /// the child exited by itself.
    async fn on_exit(&mut self, status: std::io::Result<ExitStatus>) -> Flow {
        let ran_for = match self.state {
            State::Starting { since } | State::Running { since } | State::Unhealthy { since } => {
                since.elapsed()
            }
            _ => Duration::ZERO,
        };
        let (code, signal) = match &status {
            Ok(status) => exit_parts(status),
            Err(e) => {
                warn!("Error waiting for {}: {}", self.spec.name, e);
                (None, None)
            }
        };
        warn!(
            "Process {} exited unexpectedly (code {:?}, signal {:?}, after {:?})",
            self.spec.name, code, signal, ran_for
        );
        self.live.set_proc(None);
        self.uptime_deadline = None;
        if let Some(token) = self.launch_token.take() {
            token.cancel();
        }
        self.health = None;
        // members the leader left behind would hold the port of the restart
        if let Some(child) = &mut self.child {
            let pgid = child.pgid();
            reap_group_members(
                pgid,
                Duration::from_millis(self.spec.kill_timeout_ms).min(ORPHAN_GRACE),
            )
            .await;
            child.disarm();
        }
        self.child = None;
        self.running_pin = None;
        self.pid_record = None;
        self.join_capture().await;
        self.last_exit_code = code;
        self.last_exit_signal = signal;

        let abnormal = code != Some(0);
        if abnormal {
            let detail = self.detail();
            self.emit(LifecycleKind::Crashed, Cause::Crash, detail);
        }

        let cause = match (code, signal) {
            (Some(0), _) => ExitCause::CleanExit,
            _ => ExitCause::Crash { code, signal },
        };
        let next = self.decide(cause, ran_for);
        self.after_exit(Cause::Crash, next);
        Flow::Continue
    }

    /// Ask the tracker what follows an ended launch: the delay and attempt of the next one, or
    /// the reason the app ends errored (the policy says no, or the breaker tripped).
    fn decide(
        &mut self,
        cause: ExitCause,
        ran_for: Duration,
    ) -> std::result::Result<(Duration, u32), String> {
        let mode = self.spec.restart_mode;
        match self
            .tracker
            .record_exit(Instant::now(), ran_for, cause, mode)
        {
            Decision::Restart { delay, attempt } => {
                if cause != ExitCause::SpawnFailed {
                    // a failed relaunch is another attempt, not another restart
                    self.restarts += 1;
                }
                Ok((delay, attempt))
            }
            Decision::NoRestart => {
                let exit = describe_exit(self.last_exit_code, self.last_exit_signal);
                Err(if self.spec.restart_policy.auto_restart {
                    format!("exited with {}; not restarted (restart={})", exit, mode)
                } else {
                    format!("exited with {}; not restarted (auto restart is off)", exit)
                })
            }
            Decision::GiveUp { exits_in_window } => {
                let window = self.spec.restart_policy.crash_window_secs;
                warn!(
                    "Process {} exited {} times within {} s, marking as errored",
                    self.spec.name, exits_in_window, window
                );
                let message = format!("crash loop: {} exits in {} s", exits_in_window, window);
                let mut detail = self.detail();
                detail.reason = Some(message.clone());
                self.emit(LifecycleKind::CrashLoop, Cause::Crash, detail);
                Err(message)
            }
        }
    }

    /// The child is gone (it exited, or was stopped to be replaced, or its relaunch failed):
    /// errored with the reason, or backoff until the automatic restart.
    fn after_exit(&mut self, cause: Cause, next: std::result::Result<(Duration, u32), String>) {
        match next {
            Err(reason) => {
                self.cron = None;
                self.state = State::Failed;
                self.last_error = Some(reason.clone());
                let mut detail = self.detail();
                detail.reason = Some(reason);
                self.emit(LifecycleKind::Errored, cause, detail);
            }
            Ok((delay, attempt)) => {
                self.backoff_attempt = attempt;
                self.backoff_cause = cause;
                self.state = State::Backoff {
                    until: Instant::now() + delay,
                };
                let detail = self.detail();
                self.emit(LifecycleKind::BackoffScheduled, cause, detail);
            }
        }
    }

    /// the backoff is over; the automatic launch reuses the cached plan.
    async fn on_backoff_elapsed(&mut self) -> Flow {
        if self.is_closed() {
            self.state = State::Stopped;
            return Flow::Continue;
        }
        info!(
            "Restarting process {} (attempt {})",
            self.spec.name, self.restarts
        );
        self.begin_pipeline(PipelineMode::Cached, Launch::Auto(self.backoff_cause));
        Flow::Continue
    }

    /// Stop the child (SIGTERM to its group, SIGKILL after the timeout), join the capture and
    /// record how it ended. The state is left to the caller.
    async fn terminate(&mut self, cap: Option<Duration>) {
        self.state = State::Stopping;
        self.publish();
        if let Some(token) = self.launch_token.take() {
            token.cancel();
        }
        self.health = None;
        if let Some(child) = &mut self.child {
            let mut timeout = Duration::from_millis(self.spec.kill_timeout_ms);
            if let Some(cap) = cap {
                timeout = timeout.min(cap);
            }
            info!("Stopping app {} (id: {})", self.spec.name, self.id);
            let kill = self.kill.clone();
            let terminated = tokio::select! {
                terminated = terminate_group(child, timeout) => terminated,
                // the second signal, or the shutdown's fallback: no more grace
                _ = kill.cancelled() => {
                    warn!("Hard kill of {} while stopping it", self.spec.name);
                    let _ = kill_group(child.pgid(), Signal::SIGKILL);
                    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
                        .await
                        .ok()
                        .and_then(|waited| waited.ok());
                    child.disarm();
                    Terminated { status, escalated: true }
                }
            };
            if terminated.escalated {
                warn!("Process didn't stop gracefully, sent SIGKILL to its process group");
            }
            let (code, signal) = match (&terminated.status, terminated.escalated) {
                (Some(status), false) => exit_parts(status),
                (Some(status), true) => (None, status.signal()),
                (None, _) => (None, None),
            };
            self.last_exit_code = code;
            self.last_exit_signal = signal;
        }
        self.live.set_proc(None);
        self.uptime_deadline = None;
        self.child = None;
        self.running_pin = None;
        self.pid_record = None;
        self.join_capture().await;
    }

    /// Wait for the capture tasks to drain; abort what is still running after the timeout, so a
    /// grandchild that escaped the group cannot leave a second writer for the next launch.
    async fn join_capture(&mut self) {
        let Some(mut capture) = self.capture.take() else {
            return;
        };
        for handle in &mut capture.handles {
            if tokio::time::timeout(CAPTURE_JOIN, &mut *handle)
                .await
                .is_err()
            {
                warn!(
                    "The log capture of {} did not end; aborting it",
                    self.spec.name
                );
                handle.abort();
            }
        }
    }

    fn hard_kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = kill_group(child.pgid(), Signal::SIGKILL);
            // killed now; the drop must not signal the group again, its id may be reused by then
            child.disarm();
        }
        self.pid_record = None;
        self.live.set_proc(None);
        self.uptime_deadline = None;
    }
}

/// Truncate the active log files of app `name` that exist (`O_WRONLY|O_TRUNC`). Both are tried;
/// the error names every file that could not be emptied.
pub fn truncate_log_files(paths: &Paths, name: &str) -> Result<()> {
    let mut failures = Vec::new();
    for stream in ["out", "err"] {
        let path = paths.log_path(name, stream);
        if !path.exists() {
            continue;
        }
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
        {
            failures.push(format!("{}: {}", path.display(), e));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::IoError(std::io::Error::other(failures.join("; "))))
    }
}

/// Wait until the group has no members, SIGKILL whoever is left after `grace`.
async fn reap_group_members(pgid: i32, grace: Duration) {
    let deadline = Instant::now() + grace;
    while group_exists(pgid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if group_exists(pgid) {
        let _ = kill_group(pgid, Signal::SIGKILL);
    }
}

fn describe_exit(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(code), _) => format!("code {}", code),
        (None, Some(signal)) => format!("signal {}", signal),
        (None, None) => "an unknown status".to_string(),
    }
}

/// An error to hand to each of several waiting replies.
pub fn clone_error(e: &Error) -> Error {
    match e {
        Error::AppNotFound(s) => Error::AppNotFound(s.clone()),
        Error::AppAlreadyExists(s) => Error::AppAlreadyExists(s.clone()),
        Error::ShuttingDown => Error::ShuttingDown,
        Error::Cancelled(s) => Error::Cancelled(s.clone()),
        Error::Busy(s) => Error::Busy(s.clone()),
        Error::DependencyMissing(s) => Error::DependencyMissing(s.clone()),
        Error::DependencyCycle(s) => Error::DependencyCycle(s.clone()),
        Error::DependencyFailed(s) => Error::DependencyFailed(s.clone()),
        Error::DependencyTimeout(s) => Error::DependencyTimeout(s.clone()),
        Error::BuildFailed(s) => Error::BuildFailed(s.clone()),
        Error::ProcessStartFailed(s) => Error::ProcessStartFailed(s.clone()),
        Error::InvalidSpec(s) => Error::InvalidSpec(s.clone()),
        Error::Unsupported(s) => Error::Unsupported(s.clone()),
        Error::DbError(s) => Error::DbError(s.clone()),
        Error::InvalidMode(s) => Error::InvalidMode(s.clone()),
        Error::AdapterMissing(s) => Error::AdapterMissing(s.clone()),
        Error::AdapterUnavailable(s) => Error::AdapterUnavailable(s.clone()),
        Error::AdapterTimeout(s) => Error::AdapterTimeout(s.clone()),
        Error::AdapterCrashed(s) => Error::AdapterCrashed(s.clone()),
        Error::AdapterProtocol(s) => Error::AdapterProtocol(s.clone()),
        Error::AdapterBusy(s) => Error::AdapterBusy(s.clone()),
        Error::AdapterInvalid(s) => Error::AdapterInvalid(s.clone()),
        Error::AdapterTampered(s) => Error::AdapterTampered(s.clone()),
        Error::IoError(io) => Error::IoError(std::io::Error::new(io.kind(), io.to_string())),
        other => Error::Internal(other.to_string()),
    }
}

/// Where and with what environment a script probe of `spec` runs.
pub(super) fn probe_context(spec: &AppSpec) -> ScriptContext {
    ScriptContext {
        cwd: Some(spec.cwd.clone()),
        env: script_env(spec, &[]),
    }
}

async fn wait_child(child: &mut Option<GroupChild>) -> std::io::Result<ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

async fn poll_pipeline(pipeline: &mut Option<Pipeline>) -> PipelineResult {
    match pipeline {
        Some(pipeline) => pipeline.fut.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// The launch pipeline: what must happen before a child may be spawned. A future the actor owns;
/// dropping it cancels the dependency wait, the delay, the wait for a launch slot and any build
/// (a build runs in its own process group that is killed with the future).
///
/// For an operator launch the order is: dependency gate (no slot held), startup delay, launch
/// slot, `prepare`, `plan`, and a last look at the dependencies. If one of them went down while
/// the build ran, the slot is given back and the launch waits again; the timeout clock started
/// with the pipeline and is not reset.
async fn run_pipeline(
    ctx: Arc<ActorCtx>,
    spec: Arc<AppSpec>,
    mode: PipelineMode,
    origin: Origin,
    cached: Option<PreparedLaunch>,
    building: Arc<AtomicBool>,
    phase: Arc<Notify>,
) -> PipelineResult {
    let gate = match mode {
        PipelineMode::Cached => match cached {
            Some(plan) => return PipelineResult::Ready(plan, None, Vec::new()),
            None => {
                // not expected (the plan is cached by the first launch); prepare again
                false
            }
        },
        PipelineMode::Operator { gate } => gate,
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(spec.depends_timeout_secs);
    // False once a fail-open timeout let the launch through: nothing left to re-check.
    let mut satisfied = true;
    if gate {
        match gate_wait(&ctx, &spec, deadline).await {
            Ok(ok) => satisfied = ok,
            Err(e) => return PipelineResult::Failed(e),
        }
        if let Some(delay_ms) = spec.startup_delay_ms.filter(|ms| *ms > 0) {
            info!("Waiting {}ms before starting {}...", delay_ms, spec.name);
            tokio::time::sleep(Duration::from_millis(delay_ms.min(MAX_APP_TIMER_MS))).await;
        }
    }
    // What a runtime adapter has to say is asked before a launch slot is taken: a hung adapter
    // then holds up the apps of its runtime only, never a plain `cmd` app or the whole boot.
    let runner = match ctx.runtimes.runner_for(&spec.mode, origin).await {
        Ok(runner) => runner,
        Err(e) => return PipelineResult::Failed(e),
    };
    if let Err(e) = runner.consult(&spec).await {
        return PipelineResult::Failed(e);
    }
    let mut slot = match ctx.launch_slots.clone().acquire_owned().await {
        Ok(slot) => slot,
        Err(_) => {
            return PipelineResult::Failed(Error::Internal("the launch slots are closed".into()))
        }
    };
    building.store(true, Ordering::SeqCst);
    phase.notify_one();

    info!("Preparing {} ({})...", spec.name, spec.mode);
    let prepared = match runner.prepare(&spec).await {
        Ok(result) if result.success => result,
        Ok(result) => {
            error!("Prepare failed for {}: {}", spec.name, result.output);
            return PipelineResult::Failed(Error::BuildFailed(result.output));
        }
        Err(e) => return PipelineResult::Failed(e),
    };
    info!("Prepare successful for {}", spec.name);
    let warnings = prepared.warnings;
    let invocation = if runner.mode_name() == "adapter" {
        prepared.binary_path
    } else {
        None
    };
    let plan = match prepared.plan {
        Some(plan) => plan,
        None => match runner.plan(&spec) {
            Ok(plan) => plan,
            Err(e) => return PipelineResult::Failed(e),
        },
    };

    // immediately before the spawn: the dependencies must still be running
    while gate && satisfied && !spec.depends_on.is_empty() {
        let Some(registry) = ctx.registry() else {
            return PipelineResult::Failed(Error::ShuttingDown);
        };
        match deps::check_now(&registry, &spec) {
            Verdict::Ready => break,
            Verdict::Failed(message) => {
                return PipelineResult::Failed(Error::DependencyFailed(message))
            }
            Verdict::Wait(name) => {
                info!(
                    "Dependency '{}' of {} went down during the build; waiting again",
                    name, spec.name
                );
                drop(slot);
                building.store(false, Ordering::SeqCst);
                phase.notify_one();
                match gate_wait(&ctx, &spec, deadline).await {
                    Ok(ok) => satisfied = ok,
                    Err(e) => return PipelineResult::Failed(e),
                }
                slot = match ctx.launch_slots.clone().acquire_owned().await {
                    Ok(slot) => slot,
                    Err(_) => {
                        return PipelineResult::Failed(Error::Internal(
                            "the launch slots are closed".into(),
                        ))
                    }
                };
                building.store(true, Ordering::SeqCst);
                phase.notify_one();
            }
        }
    }
    let pin_owner = (invocation.is_some() && uses_pinned_binary(&plan)).then(|| Arc::from(runner));
    PipelineResult::Ready(
        PreparedLaunch {
            plan,
            invocation,
            pin_owner,
        },
        Some(slot),
        warnings,
    )
}

/// The dependency wait of a pipeline: `Ok(true)` when every dependency is running, `Ok(false)`
/// when the deadline passed on a `depends_fail_open` spec.
async fn gate_wait(ctx: &ActorCtx, spec: &AppSpec, deadline: tokio::time::Instant) -> Result<bool> {
    if spec.depends_on.is_empty() {
        return Ok(true);
    }
    let Some(registry) = ctx.registry() else {
        return Err(Error::ShuttingDown);
    };
    deps::wait_ready(&registry, spec, deadline).await
}
