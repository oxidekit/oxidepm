//! Main daemon orchestration

use chrono::{DateTime, Utc};
use oxidepm_adapter_proto::messages::OutputStream;
use oxidepm_core::actor::{ActorClass, Via};
use oxidepm_core::paths::HomeDir;
use oxidepm_core::{DaemonConfig, Error, ErrorCode, Paths, Result};
use oxidepm_db::Database;
use oxidepm_ipc::protocol::{DaemonPhase, FEATURES, PROTOCOL_MIN, PROTOCOL_VERSION};
use oxidepm_ipc::server::{Incoming, IpcConnection, ReadError, ServerLimits};
use oxidepm_ipc::{AdapterRequest, InvokeArgs, IpcServer, Request, Response, ResponseEnvelope};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{error, info, warn};

use crate::adapters::events::AdapterEventSink;
use crate::adapters::host::{AdapterHost, HostConfig};
use crate::adapters::hostapi::Dispatch;
use crate::adapters::runtime::DaemonRuntimes;
use crate::handlers::{self, RequestHandler};
use crate::lock::DaemonLock;
use crate::policy::{self, log_field, PeerCred, PeerLabel, ReqCtx};
use crate::supervisor::{EventSink, RuntimeSourceRef, Supervisor, SupervisorConfig, CURRENT_ACTOR};

/// What `oxidepmd --check` verifies: the home directory can be prepared, the database opens
/// (or is refused as incompatible) and `saved.json`, if present, parses. Takes no lock and
/// binds nothing.
pub async fn check_state(paths: &Paths) -> Result<()> {
    paths.ensure_dirs()?;
    DaemonConfig::load(&paths.daemon_config())?;
    let db = Database::new(&paths.db()).await?;
    db.close().await;

    crate::supervisor::check_saved_file(&paths.saved())?;
    Ok(())
}

/// How long a second shutdown signal waits for the kills to take effect before the daemon
/// exits regardless.
const FORCED_EXIT_WAIT: Duration = Duration::from_secs(1);

/// How long the daemon waits, after the shutdown has finished, for a `Kill` client to be
/// answered before it exits.
const KILL_REPLY_WAIT: Duration = Duration::from_secs(2);

/// How long closing the database may take.
const DB_CLOSE_WAIT: Duration = Duration::from_secs(5);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
const BOOT_DISCOVERY_WAIT: Duration = Duration::from_secs(2);

const MAIN_PEER_LOOKUPS: usize = 32;
const MAIN_NON_HUMAN_CONNECTIONS: usize = 64;
const MAIN_CONNECTIONS_PER_PID: usize = 32;
const MAIN_NON_HUMAN_CONNECTIONS_PER_PEER: usize = 8;

#[derive(Hash, PartialEq, Eq)]
enum SocketPeer {
    Process(i32),
    DeclaredAi(i32),
    Adapter(String),
    Unresolved,
}

struct MainSocketLimits {
    cli: Arc<Semaphore>,
    non_human: Arc<Semaphore>,
    peers: Mutex<HashMap<SocketPeer, Weak<Semaphore>>>,
}

struct MainSocketSlot {
    _group: OwnedSemaphorePermit,
    _peer: OwnedSemaphorePermit,
    restricted: bool,
}

impl MainSocketLimits {
    fn new() -> Self {
        let total = ServerLimits::MAIN.max_connections.expect("main socket cap");
        Self {
            cli: Arc::new(Semaphore::new(total - MAIN_NON_HUMAN_CONNECTIONS)),
            non_human: Arc::new(Semaphore::new(MAIN_NON_HUMAN_CONNECTIONS)),
            peers: Mutex::default(),
        }
    }

    fn admit(&self, peer: SocketPeer) -> Option<MainSocketSlot> {
        let restricted = !matches!(peer, SocketPeer::Process(_));
        let (group, cap) = if restricted {
            (&self.non_human, MAIN_NON_HUMAN_CONNECTIONS_PER_PEER)
        } else {
            (&self.cli, MAIN_CONNECTIONS_PER_PID)
        };
        let group = Arc::clone(group).try_acquire_owned().ok()?;
        let mut peers = self.peers.lock();
        peers.retain(|_, slots| slots.strong_count() != 0);
        let entry = peers.entry(peer).or_default();
        let slots = entry.upgrade().unwrap_or_else(|| {
            let slots = Arc::new(Semaphore::new(cap));
            *entry = Arc::downgrade(&slots);
            slots
        });
        Some(MainSocketSlot {
            _group: group,
            _peer: slots.try_acquire_owned().ok()?,
            restricted,
        })
    }
}

/// How the daemon's main loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    /// Every app was stopped (or killed within its budget) and the state saved.
    Clean,
    /// A second shutdown signal cut the shutdown short: every child was killed at once.
    Forced,
}

/// Where the shutdown stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownState {
    Running,
    ShuttingDown,
    Done,
}

/// The signals the daemon reacts to.
struct Signals {
    term: Signal,
    int: Signal,
    quit: Signal,
    hup: Signal,
}

enum SignalEvent {
    /// SIGTERM, SIGINT or SIGQUIT: stop (the name says which)
    Stop(&'static str),
    /// SIGHUP: ignored. The daemon is detached from its terminal; an unhandled SIGHUP would
    /// terminate it by default and orphan every app.
    Hangup,
}

impl Signals {
    /// Register the handlers. From here on none of these signals has its default action.
    fn install() -> std::io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn next(&mut self) -> SignalEvent {
        tokio::select! {
            _ = self.term.recv() => SignalEvent::Stop("SIGTERM"),
            _ = self.int.recv() => SignalEvent::Stop("SIGINT"),
            _ = self.quit.recv() => SignalEvent::Stop("SIGQUIT"),
            _ = self.hup.recv() => SignalEvent::Hangup,
        }
    }
}

/// Everything the connection tasks and the main loop share.
struct Core {
    pid: u32,
    started_at: DateTime<Utc>,
    phase: watch::Sender<DaemonPhase>,
    handler: RequestHandler,
    supervisor: Arc<Supervisor>,
    /// The installed adapters. Loaded before the boot resurrect starts; nothing waits for them.
    adapters: Arc<AdapterHost>,
    shutdown: watch::Sender<ShutdownState>,
    /// The boot resurrect ran to its end, so what is running now is what `saved.json` should
    /// say. A daemon stopped mid-boot leaves the file as it was.
    resurrect_done: AtomicBool,
    /// Apps whose stop found something to stop, set by the shutdown.
    stopped: AtomicUsize,
    /// `Kill` requests still waiting to be answered.
    kill_waiters: AtomicUsize,
    budget: Duration,
}

impl Core {
    fn phase(&self) -> DaemonPhase {
        self.phase.borrow().clone()
    }

    fn set_phase(&self, phase: DaemonPhase) {
        self.phase.send_replace(phase);
    }

    /// Give initial app discovery a chance to finish without delaying requests indefinitely.
    async fn discovered_phase(&self) -> DaemonPhase {
        let mut phase = self.phase.subscribe();
        let discovered = tokio::time::timeout(
            BOOT_DISCOVERY_WAIT,
            phase.wait_for(|phase| !matches!(phase, DaemonPhase::Booting)),
        )
        .await;
        match discovered {
            Ok(Ok(phase)) => phase.clone(),
            _ => self.phase(),
        }
    }

    fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow() != ShutdownState::Running
    }

    /// Start the shutdown if it has not started (idempotent; does not wait). Returns whether
    /// this call started it.
    fn begin_shutdown(self: &Arc<Self>, reason: &str) -> bool {
        let first = self.shutdown.send_if_modified(|state| {
            if *state == ShutdownState::Running {
                *state = ShutdownState::ShuttingDown;
                true
            } else {
                false
            }
        });
        if first {
            info!("Shutting down: {}", reason);
            self.set_phase(DaemonPhase::ShuttingDown);
            let core = Arc::clone(self);
            // Its own task: whoever asked for the shutdown (a connection that may go away, the
            // main loop that must keep listening for a second signal) does not own it.
            tokio::spawn(async move {
                core.run_shutdown().await;
                core.shutdown.send_replace(ShutdownState::Done);
            });
        }
        first
    }

    /// Begin the shutdown and wait until it is over. Callers after the first just wait.
    /// Returns how many apps were stopped.
    async fn shutdown(self: &Arc<Self>, reason: &str) -> usize {
        self.begin_shutdown(reason);
        self.wait_done().await;
        self.stopped.load(Ordering::SeqCst)
    }

    async fn wait_done(&self) {
        let mut state = self.shutdown.subscribe();
        let _ = state.wait_for(|s| *s == ShutdownState::Done).await;
    }

    /// The shutdown routine: close the gate and stop everything within the budget, write
    /// `saved.json` from what was running, wait for (and then kill) running hooks, close the
    /// database.
    async fn run_shutdown(&self) {
        let began = std::time::Instant::now();
        let report = self.supervisor.shutdown(self.budget).await;
        self.stopped.store(report.stopped, Ordering::SeqCst);

        if self.resurrect_done.load(Ordering::SeqCst) {
            // from the registry (the database), so clusters are written once and an app that is
            // meant to run but crashed is still listed
            if let Err(e) = self.supervisor.save(true).await {
                error!("Could not write the saved apps: {}", e);
            }
        } else {
            warn!("The boot resurrect had not finished: leaving saved.json as it was");
        }

        // the apps are stopped, so the services get their chance to deliver the messages
        // that produced; then every adapter's process group is killed. None of it waits for an
        // adapter longer than the grace.
        self.adapters
            .shutdown(self.budget.saturating_sub(began.elapsed()))
            .await;

        // The stop hooks of the apps just stopped may still be running: give them a moment, then
        // kill their process groups, so no hook outlives the daemon.
        self.supervisor.drain_hooks().await;

        if tokio::time::timeout(DB_CLOSE_WAIT, self.handler.close_db())
            .await
            .is_err()
        {
            warn!("Timed out closing the database; exiting without a checkpoint");
        }
    }

    /// The one place a request becomes a response, and the one place an error does.
    ///
    /// `Ping` and `Hello` need no app state and only go through the rate check. Everything else goes
    /// through the gate (`RequestHandler::guarded`): the policy decides, once, which apps the
    /// request may act on, and the handler acts on those. `Kill` is the same routine as a
    /// signal, after the same gate.
    async fn dispatch(self: &Arc<Self>, ctx: &ReqCtx, request: Request) -> Response {
        if matches!(request, Request::Ping | Request::Hello { .. }) {
            if let Err(e) = self.handler.admit_poll(ctx, request.name()) {
                return Response::from_error(&e);
            }
        }
        policy::log_request(ctx, &request);

        match &request {
            Request::Ping => return Response::Pong,
            Request::Hello { client } => {
                info!("Hello from {}", log_field(client));
                return Response::Hello {
                    protocol_min: PROTOCOL_MIN,
                    protocol_max: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    pid: self.pid,
                    started_at: self.started_at,
                    phase: self.discovered_phase().await,
                    features: FEATURES.iter().map(|f| f.to_string()).collect(),
                };
            }
            Request::Status => {
                self.discovered_phase().await;
            }
            // Kill is the same routine as a signal; the reply is sent when it is over.
            Request::Kill => {
                let dry = RequestHandler::is_dry_run(ctx, &request);
                let (auth, ticket) = match self.handler.begin(ctx, &request).await {
                    Ok(begun) => begun,
                    Err(e) if dry && handlers::is_refusal(&e) => {
                        return handlers::denied_plan(&request, &e)
                    }
                    Err(e) => return Response::from_error(&e),
                };
                // a rehearsal says what the shutdown would do and leaves the daemon running
                if dry {
                    let result = self.handler.plan(auth, request).await;
                    self.handler.finish(ticket, &result);
                    return result.unwrap_or_else(|e| Response::from_error(&e));
                }
                let stopped = self.shutdown("Kill request").await;
                let response = Response::ok(format!(
                    "Stopped {} process(es), daemon shutting down",
                    stopped
                ));
                // written before the reply: the audit queue is drained after the last Kill client
                // has been answered
                self.handler.finish(ticket, &Ok(response.clone()));
                return response;
            }
            _ => {}
        }

        if self.is_shutting_down() {
            return Response::from_error(&Error::ShuttingDown);
        }

        // No lock around the handler: operations on one app are serialised by the supervisor's
        // per-app lock, everything else runs concurrently.
        let result = self.handler.guarded(ctx, request).await;
        result.unwrap_or_else(|e| {
            warn!(
                "Request {} failed: {}",
                log_field(&ctx.request_id),
                log_field(&e)
            );
            // A request that passed the check above and then met a shutdown that closed the
            // database under it is answered as what it is, not with a database error.
            if self.is_shutting_down() {
                return Response::from_error(&Error::ShuttingDown);
            }
            Response::from_error(&e)
        })
    }
}

/// The entry point an adapter's `host.request` goes through: `Core::dispatch`, the one
/// every client request uses, with the actor of the adapter in scope for the lifecycle events
/// the request causes. It holds the core weakly, so the adapters the core owns do not keep it
/// alive.
struct CoreDispatch(std::sync::Weak<Core>);

#[async_trait::async_trait]
impl Dispatch for CoreDispatch {
    async fn dispatch(&self, ctx: &ReqCtx, request: Request) -> Response {
        let Some(core) = self.0.upgrade() else {
            return Response::from_error(&Error::ShuttingDown);
        };
        CURRENT_ACTOR
            .scope(Arc::from(ctx.actor.wire()), core.dispatch(ctx, request))
            .await
    }
}

/// Main daemon struct
pub struct Daemon {
    server: IpcServer,
    /// The agent socket: every connection on it is class `ai`. Absent when `daemon.toml` turns
    /// it off or it could not be bound (the daemon runs without it).
    agent: Option<IpcServer>,
    core: Arc<Core>,
    signals: Mutex<Option<Signals>>,
    /// Held for the life of the process: one daemon per home.
    _lock: DaemonLock,
    _home: HomeDir,
}

impl Daemon {
    /// Create a new daemon instance in the default home (`OXIDEPM_HOME` or `~/.oxidepm`)
    pub async fn new() -> Result<Self> {
        Self::with_paths(Arc::new(Paths::from_env()?)).await
    }

    /// Create a new daemon instance that keeps all its files under `paths`.
    ///
    /// The order matters: the single-instance lock comes first (before anything slow, so
    /// daemons started together serialise), then the database and the supervisor, then the
    /// socket, and only then are the saved apps started, in the background, so a daemon that
    /// is busy resurrecting already answers its clients.
    pub async fn with_paths(paths: Arc<Paths>) -> Result<Self> {
        // From the first moment a SIGTERM or SIGHUP must not kill the process by default
        // (apps may already be starting): handle them, act on them once the loop runs.
        let signals = Signals::install()?;

        let home = paths.open_home()?;
        home.ensure_logs_dir()?;

        // One daemon per home. A second one gets `DaemonAlreadyRunning` here.
        let lock = DaemonLock::acquire(&Paths::new(home.descriptor_path()).lock())?;

        home.verify_path()?;
        let config = DaemonConfig::load(&paths.daemon_config())?;

        // Children do not survive the daemon: whatever a daemon that was SIGKILLed left running
        // is killed before anything is started (3.9). The lock is ours, so nobody else owns
        // those processes.
        {
            // the adapters a dead daemon left are recorded apart from the apps (adapter-pids.json)
            let pids = paths.pids();
            let adapter_pids = paths.adapter_pids();
            let swept = tokio::task::spawn_blocking(move || {
                let mut report = crate::supervisor::sweep_orphans(&pids);
                let adapters = crate::supervisor::sweep_orphans(&adapter_pids);
                report.killed.extend(adapters.killed);
                report
            })
            .await;
            match swept {
                Ok(report) if !report.killed.is_empty() => {
                    info!(
                        "Orphan sweep killed {} orphaned process group(s)",
                        report.killed.len()
                    )
                }
                Ok(_) => {}
                Err(e) => warn!("Orphan sweep failed: {}", e),
            }
        }

        // Initialize database
        home.verify_path()?;
        let db_path = paths.db();
        let db = Database::new(&db_path).await?;
        info!("Database initialized at {}", db_path.display());

        // Create supervisor and register every app of the database (stopped ones too)
        // The adapter sink is the third lifecycle sink. The supervisor takes its sinks at
        // construction, before the adapter host exists, so the sink is bound to the host's
        // event hub later.
        let adapter_sink = AdapterEventSink::new();
        let runtimes = DaemonRuntimes::new(config.reuse_plan);
        let supervisor = Arc::new(
            Supervisor::with_extra_sink(
                db,
                Arc::clone(&paths),
                SupervisorConfig {
                    launch_concurrency: config.launch_concurrency,
                },
                Arc::clone(&adapter_sink) as Arc<dyn EventSink>,
                Arc::clone(&runtimes) as RuntimeSourceRef,
            )
            .await?,
        );
        supervisor.load_registry().await?;

        // The agent socket is an extra: a bind that fails (a path that is too long, a file in the
        // way) leaves the daemon without it and never stops the boot.
        let agent = if config.agent_socket {
            home.verify_path()?;
            let agent_path = paths.agent_socket();
            match IpcServer::bind_with(&agent_path, ServerLimits::AGENT).await {
                Ok(server) => Some(server),
                Err(e) => {
                    warn!(
                        "The agent socket {} is not available ({}); the daemon runs without it",
                        agent_path.display(),
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        // Create IPC server. This process holds the lock, so it alone may replace a stale
        // socket; `bind` still refuses one that answers.
        home.verify_path()?;
        let socket_path = paths.socket();
        let server = IpcServer::bind_with(
            &socket_path,
            ServerLimits {
                max_connections: None,
                ..ServerLimits::MAIN
            },
        )
        .await?;
        info!("IPC server listening on {}", socket_path.display());

        let mut handler = RequestHandler::new(Arc::clone(&supervisor));
        handler.set_agent_socket(config.agent_socket, agent.is_some());

        // The adapter table is read before the boot resurrect is spawned below, so an app that
        // needs an adapter finds it there. This starts no process and cannot fail the boot.
        home.verify_path()?;
        let adapters = AdapterHost::load(
            &paths,
            Some(Arc::clone(handler.audit())),
            HostConfig::production(),
        )
        .await;
        handler.set_adapter_host(Arc::clone(&adapters));
        adapter_sink.bind(Arc::clone(adapters.events()));
        runtimes.bind(Arc::clone(&adapters));
        let core = Arc::new(Core {
            pid: std::process::id(),
            started_at: Utc::now(),
            phase: watch::channel(DaemonPhase::Booting).0,
            handler,
            supervisor,
            adapters,
            shutdown: watch::channel(ShutdownState::Running).0,
            resurrect_done: AtomicBool::new(false),
            stopped: AtomicUsize::new(0),
            kill_waiters: AtomicUsize::new(0),
            budget: Duration::from_secs(config.shutdown_budget_secs),
        });

        // From here an adapter's `host.request` is answered, through the same entry point as a
        // client's request.
        core.adapters
            .bind_dispatch(Arc::new(CoreDispatch(Arc::downgrade(&core))));

        // Enabled services start in the background; the boot waits for none of them.
        core.adapters.start_services();

        // Resurrect any saved processes, without holding up the accept loop
        {
            let core = Arc::clone(&core);
            tokio::spawn(async move {
                let progress = {
                    let core = Arc::clone(&core);
                    move |done, total| {
                        // a shutdown that began meanwhile owns the phase
                        if !core.is_shutting_down() {
                            core.set_phase(DaemonPhase::Resurrecting { done, total });
                        }
                    }
                };
                match core.supervisor.resurrect_with(progress).await {
                    Ok(report) => {
                        if report.started > 0 {
                            info!("Resurrected {} saved processes", report.started);
                        }
                        // a saved file that could not be used is left as it is: the shutdown
                        // must not write over it
                        match &report.saved_error {
                            None if report.saved_rejected == 0 => {
                                core.resurrect_done.store(true, Ordering::SeqCst)
                            }
                            None => warn!(
                                "{} entries of the saved file were not usable; leaving it as it is",
                                report.saved_rejected
                            ),
                            Some(e) => error!("The saved apps cannot be used: {}", e),
                        }
                    }
                    // a start refused because the daemon is shutting down is not a failure
                    Err(Error::ShuttingDown) => {}
                    Err(e) => error!("Resurrecting the saved processes failed: {}", e),
                }
                if !core.is_shutting_down() {
                    core.set_phase(DaemonPhase::Ready);
                }
            });
        }

        Ok(Self {
            server,
            agent,
            core,
            signals: Mutex::new(Some(signals)),
            _lock: lock,
            _home: home,
        })
    }

    /// Run the daemon main loop until a shutdown (SIGTERM, SIGINT, SIGQUIT or a `Kill`
    /// request) has stopped every app and saved the state. A second SIGTERM/SIGINT/SIGQUIT
    /// during the shutdown kills every app at once and ends the loop with [`RunExit::Forced`].
    /// SIGHUP is ignored.
    pub async fn run(&self) -> Result<RunExit> {
        info!("Daemon running, waiting for connections...");

        let mut signals = self
            .signals
            .lock()
            .take()
            .ok_or_else(|| Error::ipc("the daemon main loop is already running"))?;
        let mut done = self.core.shutdown.subscribe();
        let mut main_retry_at = tokio::time::Instant::now();
        let mut agent_retry_at = main_retry_at;
        let lookups = Arc::new(Semaphore::new(MAIN_PEER_LOOKUPS));
        let main_limits = Arc::new(MainSocketLimits::new());

        loop {
            tokio::select! {
                result = accept_main_at(&self.server, main_retry_at, &lookups) => {
                    match result {
                        Ok((conn, lookup)) => {
                            let core = Arc::clone(&self.core);
                            let limits = Arc::clone(&main_limits);
                            tokio::spawn(serve_main_connection(core, conn, limits, lookup));
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                            main_retry_at = tokio::time::Instant::now() + ACCEPT_RETRY_DELAY;
                        }
                    }
                }
                result = accept_at(self.agent.as_ref(), agent_retry_at) => {
                    match result {
                        Ok(conn) => {
                            let core = Arc::clone(&self.core);
                            tokio::spawn(serve_connection(core, conn, Via::Agent, None, None));
                        }
                        Err(e) => {
                            error!("Failed to accept an agent connection: {}", e);
                            agent_retry_at = tokio::time::Instant::now() + ACCEPT_RETRY_DELAY;
                        }
                    }
                }
                event = signals.next() => match event {
                    SignalEvent::Hangup => {
                        info!("Received SIGHUP, ignoring it (the daemon runs detached)");
                    }
                    SignalEvent::Stop(name) => {
                        if self.core.begin_shutdown(&format!("received {name}")) {
                            continue;
                        }
                        warn!("Received {} during the shutdown: killing every app now", name);
                        self.core.supervisor.kill_all_now();
                        self.core.adapters.kill_all_now();
                        let _ = tokio::time::timeout(FORCED_EXIT_WAIT, self.core.wait_done()).await;
                        self.core.handler.close_audit().await;
                        return Ok(RunExit::Forced);
                    }
                },
                _ = done.wait_for(|s| *s == ShutdownState::Done) => {
                    // let a `Kill` client read its answer before the process goes
                    let deadline = tokio::time::Instant::now() + KILL_REPLY_WAIT;
                    while self.core.kill_waiters.load(Ordering::SeqCst) > 0
                        && tokio::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    // the last records (a Kill's result among them) leave the queue before the exit
                    self.core.handler.close_audit().await;
                    info!("Daemon exiting");
                    return Ok(RunExit::Clean);
                }
            }
        }
    }
}

/// Wait for this listener's retry deadline without delaying the other listener or signals.
async fn accept_at(
    server: Option<&IpcServer>,
    retry_at: tokio::time::Instant,
) -> Result<IpcConnection> {
    match server {
        Some(server) => {
            if retry_at > tokio::time::Instant::now() {
                tokio::time::sleep_until(retry_at).await;
            }
            server.accept().await
        }
        None => std::future::pending().await,
    }
}

/// Keep the number of accepted sockets awaiting attribution bounded separately.
async fn accept_main_at(
    server: &IpcServer,
    retry_at: tokio::time::Instant,
    lookups: &Arc<Semaphore>,
) -> Result<(IpcConnection, OwnedSemaphorePermit)> {
    let lookup = Arc::clone(lookups)
        .acquire_owned()
        .await
        .expect("lookup pool stays open");
    Ok((accept_at(Some(server), retry_at).await?, lookup))
}

async fn serve_main_connection(
    core: Arc<Core>,
    mut conn: IpcConnection,
    limits: Arc<MainSocketLimits>,
    lookup: OwnedSemaphorePermit,
) {
    // Attribute the connection before it takes a serving slot or reads any requests.
    let (label, key) = match conn.peer_pid().filter(|&pid| pid > 0) {
        Some(pid) => {
            let label = core.adapters.label_peer(Some(pid)).await;
            let key = match &label {
                Some(PeerLabel::Adapter { name, .. }) => SocketPeer::Adapter(name.clone()),
                Some(PeerLabel::Unresolved) => SocketPeer::Unresolved,
                None => SocketPeer::Process(pid),
            };
            (label, key)
        }
        None => (Some(PeerLabel::Unresolved), SocketPeer::Unresolved),
    };
    let slot = limits.admit(key);
    drop(lookup);
    let Some(slot) = slot else { return };
    if slot.restricted {
        conn.restrict_line_size(ServerLimits::AGENT.max_line);
    }
    serve_connection(core, conn, Via::Main, label.as_ref(), Some((&limits, slot))).await;
}

/// Serve one client connection until it ends. `via` is the socket it came in on.
async fn serve_connection(
    core: Arc<Core>,
    mut conn: IpcConnection,
    via: Via,
    label: Option<&PeerLabel>,
    mut main: Option<(&MainSocketLimits, MainSocketSlot)>,
) {
    let peer = PeerCred {
        pid: conn.peer_pid(),
        uid: conn.peer_uid(),
    };
    loop {
        let claim_limit = main
            .as_ref()
            .is_some_and(|(_, slot)| !slot.restricted)
            .then_some(ServerLimits::AGENT.max_line);
        match conn.read_request_with_claim_limit(claim_limit).await {
            Ok(Some(Incoming::Request(envelope))) => {
                let ctx = ReqCtx::attributed(peer, via, &envelope, label);
                if let Some((limits, slot)) = &mut main {
                    if !slot.restricted && ctx.actor.class == ActorClass::Ai {
                        // Claims can narrow the connection's budget. Use the kernel PID so
                        // another claim on the same connection or from the same peer cannot
                        // increase that budget.
                        let key = SocketPeer::DeclaredAi(peer.pid.expect("provisional peer PID"));
                        let Some(restricted) = limits.admit(key) else {
                            let reply = ResponseEnvelope::new(
                                ctx.request_id,
                                Response::error(
                                    ErrorCode::RateLimited,
                                    "Too many connections for this peer",
                                ),
                            );
                            let _ = conn.send_response(&reply).await;
                            break;
                        };
                        *slot = restricted;
                        conn.restrict_line_size(ServerLimits::AGENT.max_line);
                    }
                }
                // opening a session needs to know who is asking and what the host knows of the verb
                if let Request::Adapter(adapter_request) = &envelope.request {
                    if let AdapterRequest::SessionOpen(args) = &**adapter_request {
                        policy::log_request(&ctx, &envelope.request);
                        let response = if core.is_shutting_down() {
                            Response::from_error(&Error::ShuttingDown)
                        } else {
                            CURRENT_ACTOR
                                .scope(
                                    Arc::from(ctx.actor.wire()),
                                    core.handler.open_session(&ctx, &envelope.request, args),
                                )
                                .await
                                .unwrap_or_else(|e| {
                                    warn!(
                                        "Request {} failed: {}",
                                        log_field(&ctx.request_id),
                                        log_field(&e)
                                    );
                                    Response::from_error(&e)
                                })
                        };
                        if let Err(e) = conn
                            .send_response(&ResponseEnvelope::new(ctx.request_id.clone(), response))
                            .await
                        {
                            error!("Failed to send response: {}", e);
                            break;
                        }
                        continue;
                    }
                }
                // a relayed verb answers with a stream of lines, not with one response
                if let Request::Adapter(adapter_request) = &envelope.request {
                    if let AdapterRequest::Invoke(args) = &**adapter_request {
                        policy::log_request(&ctx, &envelope.request);
                        let args = args.clone();
                        if serve_invoke(&core, &ctx, &envelope.request, *args, &mut conn).await {
                            continue;
                        }
                        break;
                    }
                }
                let is_kill = matches!(envelope.request, Request::Kill) && !envelope.dry_run;
                if is_kill {
                    core.kill_waiters.fetch_add(1, Ordering::SeqCst);
                }
                // The supervisor's facade builds its commands' actor from this scope.
                let response = CURRENT_ACTOR
                    .scope(
                        Arc::from(ctx.actor.wire()),
                        core.dispatch(&ctx, envelope.request),
                    )
                    .await;
                let reply = ResponseEnvelope::new(ctx.request_id, response);

                // The answer to a Kill may have nobody left to read it; the shutdown does not
                // depend on that.
                let written = conn.send_response(&reply).await;
                if is_kill {
                    core.kill_waiters.fetch_sub(1, Ordering::SeqCst);
                }
                if let Err(e) = written {
                    error!("Failed to send response: {}", e);
                    break;
                }
                if is_kill {
                    break;
                }
            }
            Ok(Some(Incoming::Reject { reply, close })) => {
                warn!("Rejected a request line");
                if conn.send_reply(&reply).await.is_err() || close {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                warn!("Error reading request: {}", e);
                if matches!(e, ReadError::TooLarge { .. }) {
                    let reply = ResponseEnvelope::new(
                        "",
                        Response::error(ErrorCode::RequestTooLarge, e.to_string()),
                    );
                    let _ = conn.send_response(&reply).await;
                }
                break;
            }
        }
    }
}

/// The name of an output stream on the wire.
fn stream_name(stream: OutputStream) -> &'static str {
    match stream {
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
        OutputStream::Status => "status",
    }
}

/// Answer a relayed verb: check it, run it, and write its output to the client as it comes,
/// each line with `more` set, and then the end. The tool is cancelled, and killed if the daemon
/// started it for this call, when the client closes the connection or stops reading for longer
/// than the relay write timeout. Returns whether the connection can serve another request.
async fn serve_invoke(
    core: &Arc<Core>,
    ctx: &ReqCtx,
    request: &Request,
    args: InvokeArgs,
    conn: &mut oxidepm_ipc::server::IpcConnection,
) -> bool {
    let relay = core.adapters.relay_write_timeout();
    let (mut writer, mut watch) = conn.stream_halves();
    let finish = |response: Response| ResponseEnvelope::new(ctx.request_id.clone(), response);

    // everything before the tool runs: the daemon's state, the verb, the directory, the policy
    let prepared = async {
        if core.is_shutting_down() {
            return Err(Error::ShuttingDown);
        }
        core.handler.prepare_invoke(ctx, request, &args).await
    }
    .await;
    let (resolved, cwd, ticket) = match prepared {
        Ok(ok) => ok,
        Err(e) => {
            warn!(
                "Request {} failed: {}",
                log_field(&ctx.request_id),
                log_field(&e)
            );
            return writer
                .send(&finish(Response::from_error(&e)), relay)
                .await
                .is_ok();
        }
    };

    let (out, mut frames) = tokio::sync::mpsc::channel(crate::adapters::conn::RELAY_QUEUE_FRAMES);
    let cancel = tokio_util::sync::CancellationToken::new();
    let run = CURRENT_ACTOR.scope(
        Arc::from(ctx.actor.wire()),
        core.handler
            .run_invoke(ctx, &args, &resolved, cwd, out, cancel.clone()),
    );
    tokio::pin!(run);

    let mut alive = true;
    let result: Result<Response> = loop {
        tokio::select! {
            biased;
            // the client left: end the call, and wait for the tool to be gone
            _ = watch.gone(), if alive => {
                alive = false;
                cancel.cancel();
            }
            Some(frame) = frames.recv(), if alive => {
                let part = ResponseEnvelope::part(
                    ctx.request_id.clone(),
                    Response::InvokeOutput { stream: stream_name(frame.stream).to_string(), text: frame.text },
                );
                if let Err(e) = writer.send(&part, relay).await {
                    // a client that does not read (stopped, or suspended) must not hold the tool
                    warn!("Request {}: {}; the call is cancelled", log_field(&ctx.request_id), log_field(&e));
                    alive = false;
                    cancel.cancel();
                }
            }
            r = &mut run => break r,
        }
    };

    // what was reported before the end
    if alive {
        while let Ok(frame) = frames.try_recv() {
            let part = ResponseEnvelope::part(
                ctx.request_id.clone(),
                Response::InvokeOutput {
                    stream: stream_name(frame.stream).to_string(),
                    text: frame.text,
                },
            );
            if writer.send(&part, relay).await.is_err() {
                alive = false;
                break;
            }
        }
    }
    core.handler.finish(ticket, &result);
    if !alive {
        return false;
    }
    let response = result.unwrap_or_else(|e| {
        warn!(
            "Request {} failed: {}",
            log_field(&ctx.request_id),
            log_field(&e)
        );
        Response::from_error(&e)
    });
    writer.send(&finish(response), relay).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_ipc::protocol::Envelope;
    use std::os::unix::fs::MetadataExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn daemon_keeps_its_home_open() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Daemon::with_paths(Arc::new(Paths::new(dir.path())))
            .await
            .unwrap();
        let metadata = std::fs::metadata(dir.path()).unwrap();
        let home_is_open = || {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|entry| std::fs::metadata(entry.ok()?.path()).ok())
                .any(|fd| fd.dev() == metadata.dev() && fd.ino() == metadata.ino())
        };

        assert!(home_is_open());
        drop(daemon);
        assert!(!home_is_open());
    }

    #[tokio::test]
    async fn status_waits_for_initial_app_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Daemon::with_paths(Arc::new(Paths::new(dir.path())))
            .await
            .unwrap();
        let mut phase = daemon.core.phase.subscribe();
        phase
            .wait_for(|phase| matches!(phase, DaemonPhase::Ready))
            .await
            .unwrap();
        daemon.core.set_phase(DaemonPhase::Booting);

        let envelope = Envelope::new("status", Request::Status);
        let context = ReqCtx::attributed(
            PeerCred {
                pid: None,
                uid: None,
            },
            Via::Main,
            &envelope,
            None,
        );
        let status = daemon.core.dispatch(&context, envelope.request);
        tokio::pin!(status);
        assert!(futures::poll!(status.as_mut()).is_pending());
        daemon
            .core
            .set_phase(DaemonPhase::Resurrecting { done: 0, total: 1 });
        assert!(matches!(status.await, Response::Status { .. }));
    }

    #[tokio::test]
    async fn dry_run_kill_keeps_the_connection_open() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Daemon::with_paths(Arc::new(Paths::new(dir.path())))
            .await
            .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let core = Arc::clone(&daemon.core);
        let handler = tokio::spawn(serve_connection(
            core,
            IpcConnection::new(server, None),
            Via::Main,
            None,
            None,
        ));
        let mut client = BufReader::new(client);
        let mut kill = Envelope::new("preview", Request::Kill);
        kill.dry_run = true;

        for request in [kill, Envelope::new("check", Request::Ping)] {
            let mut line = serde_json::to_string(&request).unwrap();
            line.push('\n');
            client.get_mut().write_all(line.as_bytes()).await.unwrap();
            line.clear();
            client.read_line(&mut line).await.unwrap();
            let response: ResponseEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(response.request_id, request.request_id);
            if request.dry_run {
                assert!(matches!(response.response, Response::DryRun { .. }));
            } else {
                assert!(matches!(response.response, Response::Pong));
            }
        }
        assert!(!daemon.core.is_shutting_down());
        drop(client);
        handler.await.unwrap();
    }
}
