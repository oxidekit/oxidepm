//! The process supervisor: the facade the daemon's request handlers call.
//!
//! Every registered app has an actor (the `actor` and `group` modules); the facade finds its
//! handle, sends it a command and returns the answer. It keeps no lock across an await and no
//! global lock at all: operations on one app are ordered by that app's mailbox, different apps
//! run concurrently, and reads (`status`, `show`) come from the published snapshots.

mod actor;
mod deps;
mod events;
mod group;
mod handle;
mod health_task;
mod hooks;
mod persist;
mod pids;
mod plan;
mod registry;
mod sampler;
pub use self::sampler::{group_usage, GroupUsage};
mod stats;
mod store;
#[cfg(test)]
mod tests;

pub use self::actor::RuntimeSourceRef;
pub use self::events::{
    EventDetail, EventDraft, EventSink, Lifecycle, LifecycleEvent, LifecycleKind,
};
pub use self::handle::{AppId, Cause, RestartKind, RestartOutcome, CURRENT_ACTOR};
pub use self::hooks::{HookCtx, HookRunner};
pub use self::pids::{
    start_ticks, sweep as sweep_orphans, PidGuard, PidRecord, PidTable, SweepReport,
};
pub use self::stats::SupervisorStats;

use self::persist::SavedEntry;
use crate::policy::TargetView;

use self::actor::{clone_error, spawn_process_actor, AbortOnDrop, ActorCtx, ActorInit, Initial};
use self::events::HookSink;
use self::group::{instance_port, spawn_group_actor, GroupInit};
use self::handle::{CmdCtx, Command, HandleKind, ProcessHandle, RestartCause, StartOrigin};
use self::registry::{NameGuard, Registry};
use self::stats::Stats;
use self::store::AppStore;
use nix::sys::signal::Signal;
use oxidepm_core::graph::{topo_layers, Node};
use oxidepm_core::{AppInfo, AppSpec, AppStatus, Desired, Error, Paths, Result, Runtime, Selector};
use oxidepm_db::{AppRecord, Database};
use oxidepm_logs::LogReader;
use oxidepm_runtime::kill_group;
#[cfg(feature = "test-hooks")]
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

#[cfg(feature = "test-hooks")]
pub use self::actor::SpawnPause;

/// Seconds a layer's stop may take beyond its kill cap before the supervisor stops waiting for
/// it and kills the app's group through its actor instead (the reply-timeout fallback).
const REPLY_GRACE: Duration = Duration::from_secs(15);

/// How long the supervisor waits for an actor to end after its kill token was cancelled, before
/// it aborts the task.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Floor of the per-layer kill cap, however little of the budget is left.
const MIN_KILL_CAP: Duration = Duration::from_secs(2);

/// Settings of the supervisor itself (the daemon fills them from `daemon.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// How many apps may be building and spawning at once.
    pub launch_concurrency: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            launch_concurrency: oxidepm_core::daemon_config::DEFAULT_LAUNCH_CONCURRENCY,
        }
    }
}

/// What a shutdown did.
#[derive(Debug, Default)]
pub struct ShutdownReport {
    /// Apps whose stop found something to stop.
    pub stopped: usize,
    /// The specs of the apps that were running when the supervisor closed (what `saved.json`
    /// should list so that they come back on resurrect).
    pub running: Vec<AppSpec>,
}

/// `--check`: does the saved file at `path` have a form this build can read? (A single bad
/// entry is not a problem of the file.)
pub fn check_saved_file(path: &std::path::Path) -> Result<()> {
    persist::read_saved(path).map(|_| ())
}

/// What loading the registry from the database found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RegistryLoad {
    /// Apps registered by this call (rows, cluster parents included).
    pub loaded: usize,
    /// Of those, rows whose stored spec did not parse: registered as errored placeholders.
    pub broken: usize,
}

/// One app a resurrect could not start.
#[derive(Debug)]
pub struct ResurrectFailure {
    pub id: u32,
    pub name: String,
    pub error: Error,
}

/// What a resurrect did.
#[derive(Debug, Default)]
pub struct ResurrectReport {
    /// Apps started (imported ones included).
    pub started: usize,
    /// Apps taken from the saved file that the registry did not have.
    pub imported: usize,
    pub failed: Vec<ResurrectFailure>,
    /// Why the saved file could not be used (the registry-driven part ran regardless).
    pub saved_error: Option<Error>,
    /// Entries of the saved file that were not usable specs, or failed validation (they are
    /// also in `failed`). While there are any, the file is worth keeping as it is.
    pub saved_rejected: usize,
}

/// The kill cap for the next layer: the remaining budget shared out over the layers still to
/// stop, never below [`MIN_KILL_CAP`]. A slow lower layer therefore cannot starve the layers
/// after it, and once the budget is spent every remaining layer gets the floor.
pub fn kill_cap(remaining: Duration, layers_left: usize) -> Duration {
    let share = remaining / layers_left.max(1) as u32;
    share.max(MIN_KILL_CAP)
}

/// One app's result in a batch start.
#[derive(Debug)]
pub struct BatchItem {
    /// The app's id (0 when it was refused before it got one).
    pub id: u32,
    pub name: String,
    pub result: Result<BatchStarted>,
}

/// What a batch start did for one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchStarted {
    /// `waiting` when it is held back by dependencies that are not running yet.
    pub status: AppStatus,
    pub changed: bool,
}

/// What `set_markers` did to one app that was named (a cluster is one app).
#[derive(Debug, Clone)]
pub struct MarkerChange {
    pub id: AppId,
    pub name: String,
    /// A marker of the app or of one of its instances was different.
    pub changed: bool,
    /// The markers of the app now.
    pub protected: bool,
    pub ai_control: bool,
}

/// An app that is registered with an idle actor and waits for its first `Start`.
struct Registered {
    handle: ProcessHandle,
    spec: AppSpec,
    /// This request inserted the app's row: a failed first launch takes it out again.
    created: bool,
}

impl Registered {
    fn new(handle: ProcessHandle, spec: AppSpec, created: bool) -> Self {
        Self {
            handle,
            spec,
            created,
        }
    }
}

/// The refusal of a start that names another instance count than the cluster has.
pub(super) fn instance_count_message(name: &str, have: u32) -> String {
    format!(
        "cluster '{}' has {} instances; delete and recreate it to change the instance count",
        name, have
    )
}

/// The refusal of a start that asks for `wanted` instances of an app that exists with `have`:
/// a single app is never turned into a cluster, nor a cluster into a single app.
fn count_conflict_message(name: &str, have: u32, wanted: u32) -> String {
    if have > 1 {
        instance_count_message(name, have)
    } else {
        format!(
            "app '{}' is a single app, not a cluster of {} instances; delete and recreate it to change the instance count",
            name, wanted
        )
    }
}

/// A start of an app that exists never changes its markers: only creating the app and
/// `SetPolicy` do. Re-running `oxidepm start <file>` (the way to apply a config change) can
/// therefore never clear `protected`. A file that disagrees with the stored markers is logged.
fn keep_stored_markers(spec: &mut AppSpec, protected: bool, ai_control: bool) {
    if spec.protected != protected || spec.ai_control != ai_control {
        warn!(
            "Start of '{}': the spec says protected={} ai_control={} but the app has protected={}              ai_control={}; the stored markers are kept (change them with `oxidepm policy set`)",
            spec.name, spec.protected, spec.ai_control, protected, ai_control
        );
    }
    spec.protected = protected;
    spec.ai_control = ai_control;
}

/// Does the spec ask for a cluster (and not for one instance of it)?
fn asks_for_cluster(spec: &AppSpec) -> bool {
    spec.instances > 1 && spec.instance_id.is_none()
}

/// Process supervisor
pub struct Supervisor {
    pub(super) db: Arc<Database>,
    paths: Arc<Paths>,
    pub(super) registry: Arc<Registry>,
    ctx: Arc<ActorCtx>,
    /// Runs the lifecycle hooks (in their own process groups) and tracks the ones still running.
    hooks: Arc<HookRunner>,
    /// Serialises `resurrect`, so a request that arrives during the boot resurrect joins it.
    resurrect_lock: tokio::sync::Mutex<()>,
    /// Set by the first shutdown; later ones find the work done.
    closing: AtomicBool,
    _metrics: AbortOnDrop,
}

impl Supervisor {
    /// Create a new supervisor that keeps its files under `paths`
    pub async fn new(db: Database, paths: Arc<Paths>) -> Result<Self> {
        Self::with_runtimes(db, paths, ActorCtx::default_runtimes()).await
    }

    /// `new` with the supervisor's own settings (the daemon's way in).
    pub async fn with_settings(
        db: Database,
        paths: Arc<Paths>,
        config: SupervisorConfig,
    ) -> Result<Self> {
        Self::with_config(db, paths, ActorCtx::default_runtimes(), config).await
    }

    /// `with_settings` with one more event sink, which sees every lifecycle event after the hook
    /// sink and the notification sink (the daemon's adapter sink; it must not block), and the
    /// source of the runtimes (the daemon's: built-in runners and adapters).
    pub async fn with_extra_sink(
        db: Database,
        paths: Arc<Paths>,
        config: SupervisorConfig,
        sink: Arc<dyn EventSink>,
        runtimes: RuntimeSourceRef,
    ) -> Result<Self> {
        Self::build(db, paths, runtimes, config, Some(sink)).await
    }

    /// `new` with the runtimes chosen by the caller (tests inject a fake runner).
    pub async fn with_runtimes(
        db: Database,
        paths: Arc<Paths>,
        runtimes: RuntimeSourceRef,
    ) -> Result<Self> {
        Self::with_config(db, paths, runtimes, SupervisorConfig::default()).await
    }

    /// `with_runtimes` with the supervisor's own settings.
    pub async fn with_config(
        db: Database,
        paths: Arc<Paths>,
        runtimes: RuntimeSourceRef,
        config: SupervisorConfig,
    ) -> Result<Self> {
        Self::build(db, paths, runtimes, config, None).await
    }

    /// `with_config` with one more event sink (the tests' recorder).
    #[cfg(test)]
    pub(crate) async fn with_event_sink(
        db: Database,
        paths: Arc<Paths>,
        runtimes: RuntimeSourceRef,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self> {
        Self::build(db, paths, runtimes, SupervisorConfig::default(), Some(sink)).await
    }

    async fn build(
        db: Database,
        paths: Arc<Paths>,
        runtimes: RuntimeSourceRef,
        config: SupervisorConfig,
        extra_sink: Option<Arc<dyn EventSink>>,
    ) -> Result<Self> {
        let db = Arc::new(db);

        let hooks = HookRunner::new(Arc::clone(&paths));
        let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::new(HookSink::new(Arc::clone(&hooks)))];
        sinks.extend(extra_sink);

        let registry = Registry::new();
        let ctx = Arc::new(ActorCtx {
            store: AppStore::new(Arc::clone(&db)),
            paths: Arc::clone(&paths),
            lifecycle: Arc::new(Lifecycle::new(sinks)),
            registry: Arc::downgrade(&registry),
            stats: Arc::new(Stats::default()),
            pids: PidTable::new(paths.pids()),
            runtimes,
            launch_slots: Arc::new(tokio::sync::Semaphore::new(
                config.launch_concurrency.max(1),
            )),
            #[cfg(feature = "test-hooks")]
            spawn_pause: Mutex::new(None),
        });
        let metrics = sampler::spawn(Arc::downgrade(&registry));

        Ok(Self {
            db,
            paths,
            registry,
            ctx,
            hooks,
            resurrect_lock: tokio::sync::Mutex::new(()),
            closing: AtomicBool::new(false),
            _metrics: metrics,
        })
    }

    /// The instance ids of a cluster parent (empty for any other app).
    pub fn child_ids(&self, id: u32) -> Vec<u32> {
        self.registry
            .get(id)
            .map(|h| h.snapshot().children.clone())
            .unwrap_or_default()
    }

    pub fn paths(&self) -> &Arc<Paths> {
        &self.paths
    }

    // ---- registry at boot --------------------------------------------------------------

    /// Register every app of the database as an idle actor, whatever its desired state: from
    /// here on `status`, `show` and the selectors are in-memory reads, and a stopped app is
    /// as addressable as a running one. A cluster parent becomes a group over its instances.
    /// A row whose stored spec does not parse becomes an errored placeholder that refuses
    /// `Start` and `Restart` and can be deleted (4.1); one bad row never aborts the boot.
    ///
    /// Apps that are already registered are left alone, so calling it twice is harmless.
    pub async fn load_registry(&self) -> Result<RegistryLoad> {
        self.refuse_if_closed()?;
        let records = self.store().records().await?;
        let mut load = RegistryLoad::default();

        // Everything that is not a cluster parent first (instances included), so that a group
        // finds its children registered.
        let mut groups: Vec<(AppSpec, Desired)> = Vec::new();
        for record in records {
            if self.registry.get(record.id()).is_some() {
                continue;
            }
            match record {
                AppRecord::Ok {
                    spec,
                    parent_id: None,
                    desired,
                } if spec.instances > 1 && spec.instance_id.is_none() => {
                    groups.push((spec, desired))
                }
                AppRecord::Ok {
                    spec,
                    parent_id,
                    desired,
                } => {
                    let name = spec.name.clone();
                    let handle = spawn_process_actor(
                        &self.ctx,
                        ActorInit {
                            spec: Arc::new(spec),
                            desired,
                            parent_id,
                            created_pending: false,
                            initial: Initial::Idle,
                        },
                    );
                    self.registry.insert(&name, handle, None)?;
                    load.loaded += 1;
                }
                AppRecord::Broken {
                    id,
                    name,
                    mode,
                    protected,
                    parent_id,
                    desired,
                    error,
                    ..
                } => {
                    warn!(
                        "App '{}' (id {}): stored spec is invalid: {}",
                        name, id, error
                    );
                    let message = format!(
                        "stored spec is invalid: {} (delete the app and re-create it)",
                        error
                    );
                    let handle = spawn_process_actor(
                        &self.ctx,
                        ActorInit {
                            spec: Arc::new(placeholder_spec(id, &name, &mode, protected)),
                            desired,
                            parent_id,
                            created_pending: false,
                            initial: Initial::Broken(message),
                        },
                    );
                    self.registry.insert(&name, handle, None)?;
                    load.loaded += 1;
                    load.broken += 1;
                }
            }
        }

        for (spec, desired) in groups {
            let name = spec.name.clone();
            let mut children: Vec<(Option<u32>, AppId)> = self
                .registry
                .list()
                .iter()
                .filter(|h| h.snapshot().parent_id == Some(spec.id))
                .map(|h| (h.snapshot().spec.instance_id, h.id))
                .collect();
            children.sort();
            let handle = spawn_group_actor(
                &self.ctx,
                GroupInit {
                    spec: Arc::new(spec),
                    desired,
                    children: children.into_iter().map(|(_, id)| id).collect(),
                },
            );
            self.registry.insert(&name, handle, None)?;
            load.loaded += 1;
        }

        info!(
            "Registry loaded: {} apps ({} unreadable)",
            load.loaded, load.broken
        );
        Ok(load)
    }

    /// Test hook: every launch that has spawned its child waits (before it is recorded) until
    /// the returned `release` is notified.
    #[cfg(feature = "test-hooks")]
    pub fn pause_starts_after_spawn(&self) -> SpawnPause {
        let pause = SpawnPause {
            reached: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        *self.ctx.spawn_pause.lock() = Some(pause.clone());
        pause
    }

    /// Test hook: the number of restart-cron timers that have parsed their schedule and are
    /// waiting for it.
    #[cfg(feature = "test-hooks")]
    pub fn cron_tasks_armed(&self) -> usize {
        self.ctx.stats.snapshot().cron_tasks
    }

    /// Test-only: how many tasks of each kind the supervisor owns.
    #[cfg(feature = "test-stats")]
    pub fn stats(&self) -> SupervisorStats {
        self.ctx.stats.snapshot()
    }

    /// Test hook: make the actor of app `id` panic.
    #[cfg(feature = "test-hooks")]
    pub fn panic_actor_for_test(&self, id: AppId) -> bool {
        match self.registry.get(id) {
            Some(handle) => handle.try_post(Command::TestPanic),
            None => false,
        }
    }

    /// Has the supervisor been closed for shutdown?
    pub fn is_closed(&self) -> bool {
        self.registry.is_closed()
    }

    /// `Err(ShuttingDown)` once the supervisor is closed.
    fn refuse_if_closed(&self) -> Result<()> {
        if self.is_closed() {
            Err(Error::ShuttingDown)
        } else {
            Ok(())
        }
    }

    /// Checkpoint and close the database (graceful shutdown)
    pub async fn close_db(&self) {
        self.db.close().await;
    }

    fn store(&self) -> &AppStore {
        &self.ctx.store
    }

    // ---- start -------------------------------------------------------------------------

    /// Start an application from a spec a client sent.
    ///
    /// The one entry for specs that come from outside (IPC `Start`, the saved file): the spec is
    /// made safe, validated and checked against the machine before anything exists, so a refusal
    /// leaves no row, no log file and no process. Its dependencies are checked against the
    /// registry too: an unknown one, a cycle, or one that is stopped or errored right now is an
    /// error here instead of a wait.
    pub async fn start(&self, spec: AppSpec) -> Result<u32> {
        self.refuse_if_closed()?;
        let spec = self.admit(spec)?;
        self.check_instance_count(&spec)?;
        self.check_dependencies(&spec, &HashSet::new())?;
        self.start_admitted(spec).await
    }

    /// A start cannot change the instance count of an app that exists: a single app stays a
    /// single app and a cluster keeps its instances (a cluster's own count is checked by its
    /// group, which also sees the case where the cluster is not registered). Refused before
    /// the dependencies are looked at, so the refusal does not depend on what they are doing.
    fn check_instance_count(&self, spec: &AppSpec) -> Result<()> {
        let Some(handle) = self.registry.by_name(&spec.name) else {
            return Ok(());
        };
        let snap = handle.snapshot();
        let cluster = handle.kind == handle::HandleKind::Group;
        if cluster != asks_for_cluster(spec) {
            let have = if cluster { snap.spec.instances } else { 1 };
            return Err(Error::InvalidSpec(count_conflict_message(
                &spec.name,
                have,
                spec.instances,
            )));
        }
        Ok(())
    }

    /// The dependency checks of a start (2.7a): `in_set` names the other apps of the same
    /// batch, which are about to start and so exempt from the check of their state.
    fn check_dependencies(&self, spec: &AppSpec, in_set: &HashSet<&str>) -> Result<()> {
        if let Some(cycle) =
            deps::cycles_through_registry(&self.registry, &[spec]).remove(&spec.name)
        {
            return Err(Error::DependencyCycle(cycle));
        }
        deps::preflight(&self.registry, spec, in_set)
    }

    /// Ingress checks for a spec from outside: reset the fields a client does not choose, the
    /// pure validation, and the checks that depend on the machine. Program resolution is still
    /// done by the runner's `prepare` and launch.
    fn admit(&self, mut spec: AppSpec) -> Result<AppSpec> {
        spec.sanitize_ingress();
        spec.validate()?;
        Self::preflight(&spec)?;
        // Check the runtime and requested adapters before a row or a process exists.
        self.ctx.runtimes.check(&spec)?;
        Ok(spec)
    }

    /// Filesystem checks that do not belong in `AppSpec::validate`: the working directory must
    /// be an existing directory.
    fn preflight(spec: &AppSpec) -> Result<()> {
        if !spec.cwd.is_dir() {
            return Err(Error::InvalidSpec(format!(
                "cwd '{}' is not an existing directory",
                spec.cwd.display()
            )));
        }
        Ok(())
    }

    /// Apps whose runtime has no provider now (an adapter that was disabled or removed after
    /// the app was created): they stay in the list, running ones keep running, and their next
    /// start fails. One line each, for `DaemonStatus.warnings`.
    pub fn runtime_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for handle in self.registry.list() {
            let snap = handle.snapshot();
            if snap.parent_id.is_some() {
                continue;
            }
            let mut probe = (*snap.spec).clone();
            probe.adapters.clear();
            if let Err(Error::AdapterMissing(why)) = self.ctx.runtimes.check(&probe) {
                let name = &snap.spec.name;
                let runtime = &snap.spec.mode;
                // the source names an adapter when it knows one (installed, or in the hints)
                if why.starts_with("no adapter provides") {
                    out.push(format!(
                        "app '{name}' uses runtime '{runtime}' that no adapter provides"
                    ));
                } else {
                    out.push(format!(
                        "app '{name}' uses runtime '{runtime}' whose adapter is not enabled"
                    ));
                }
            }
        }
        out.sort();
        out
    }

    async fn start_admitted(&self, spec: AppSpec) -> Result<u32> {
        let registered = self.register_for_start(spec).await?;
        let outcome = self.send_first_start(registered).await?;
        Ok(outcome.id)
    }

    /// Start a set of apps together (`StartBatch`, a config file): three phases.
    ///
    /// 1. Every spec is admitted like any spec from outside and the set is checked as a graph:
    ///    a dependency that is neither in the set nor registered, a cycle (every app on it is
    ///    refused), a dependency outside the set that is stopped or errored, and everything that
    ///    depends on an app refused. An app refused here gets its own error; the others go on.
    /// 2. Every remaining app is registered (name reserved, row, idle actor), so that no
    ///    dependent can look up a dependency whose actor does not exist yet.
    /// 3. Only then are the `Start`s sent, all at once; the dependency wait inside the actors
    ///    orders the launches. A dependent whose dependencies are not running answers `waiting`.
    ///
    /// The result has one item per spec, in the order given. A gate that closes during the
    /// batch gives the remaining apps `ShuttingDown`. The call itself fails for a name used
    /// twice and when the supervisor is already closed.
    pub async fn start_batch(&self, specs: Vec<AppSpec>) -> Result<Vec<BatchItem>> {
        self.refuse_if_closed()?;
        let mut seen = HashSet::new();
        for spec in &specs {
            if !seen.insert(spec.name.as_str()) {
                return Err(Error::InvalidSpec(format!(
                    "app '{}' is defined more than once",
                    spec.name
                )));
            }
        }
        let order: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        let in_set: HashSet<&str> = order.iter().map(String::as_str).collect();

        // Validate the batch and find dependencies before registering apps.
        let mut failed: HashMap<String, Error> = HashMap::new();
        let mut admitted: Vec<AppSpec> = Vec::new();
        for spec in specs {
            let name = spec.name.clone();
            match self
                .admit(spec)
                .and_then(|spec| self.check_instance_count(&spec).map(|()| spec))
            {
                Ok(spec) => admitted.push(spec),
                Err(e) => {
                    failed.insert(name, e);
                }
            }
        }
        let refs: Vec<&AppSpec> = admitted
            .iter()
            .filter(|s| !failed.contains_key(&s.name))
            .collect();
        let failed = deps::reject_set(&self.registry, &refs, &in_set, failed);

        // Reserve names and register the apps.
        let mut results: HashMap<String, (u32, Result<BatchStarted>)> = HashMap::new();
        let mut registered: Vec<Registered> = Vec::new();
        for spec in admitted {
            let name = spec.name.clone();
            if let Some(e) = failed.get(&name) {
                results.insert(name, (0, Err(clone_error(e))));
                continue;
            }
            match self.register_for_start(spec).await {
                Ok(r) => registered.push(r),
                Err(e) => {
                    results.insert(name, (0, Err(e)));
                }
            }
        }
        for (name, e) in failed {
            results.entry(name).or_insert((0, Err(e)));
        }

        // Launch registered apps in dependency order.
        let sends = registered.into_iter().map(|r| async move {
            let name = r.spec.name.clone();
            let id = r.handle.id;
            let result = self.send_first_start(r).await.map(|o| BatchStarted {
                status: o.status,
                changed: o.changed,
            });
            (name, id, result)
        });
        for (name, id, result) in futures::future::join_all(sends).await {
            results.insert(name, (id, result));
        }

        Ok(order
            .into_iter()
            .map(|name| {
                let (id, result) = results
                    .remove(&name)
                    .unwrap_or_else(|| (0, Err(Error::Internal("no result for the app".into()))));
                BatchItem { id, name, result }
            })
            .collect())
    }

    /// Register an app before launching it: the name is reserved, the row exists, the actor is registered and
    /// idle. Nothing has been launched. For a name the registry already has, that app's handle.
    async fn register_for_start(&self, mut spec: AppSpec) -> Result<Registered> {
        let name = spec.name.clone();
        if let Some(handle) = self.registry.by_name(&name) {
            // `start` and batch validation have checked the count already; this only
            // guards the window in which another call registered the app in between.
            self.check_instance_count(&spec)?;
            keep_stored_markers(
                &mut spec,
                handle.markers().protected(),
                handle.markers().ai_control(),
            );
            return Ok(Registered::new(handle, spec, false));
        }
        let guard = self.registry.reserve_name(&name)?;
        // registered while this call was reserving the name (a race, like the check above)
        if let Some(handle) = self.registry.by_name(&name) {
            drop(guard);
            self.check_instance_count(&spec)?;
            keep_stored_markers(
                &mut spec,
                handle.markers().protected(),
                handle.markers().ai_control(),
            );
            return Ok(Registered::new(handle, spec, false));
        }
        let clustered = asks_for_cluster(&spec);

        match self.store().record_by_name(&name).await? {
            Some(AppRecord::Broken { name, error, .. }) => Err(Error::StateIncompatible(format!(
                "stored spec of app '{}' is invalid: {} (delete the app and re-create it)",
                name, error
            ))),
            // an app from before the daemon restarted: its row is there, its actor is not
            Some(AppRecord::Ok {
                spec: row,
                parent_id,
                desired,
            }) => {
                spec.id = row.id;
                keep_stored_markers(&mut spec, row.protected, row.ai_control);
                if clustered {
                    if row.instances != spec.instances {
                        return Err(Error::InvalidSpec(count_conflict_message(
                            &name,
                            row.instances,
                            spec.instances,
                        )));
                    }
                    return self
                        .register_cluster(spec, Some((row.id, desired)), guard, Desired::Stopped)
                        .await;
                }
                if row.instances > 1 {
                    return Err(Error::InvalidSpec(instance_count_message(
                        &name,
                        row.instances,
                    )));
                }
                let handle = spawn_process_actor(
                    &self.ctx,
                    ActorInit {
                        spec: Arc::new(row),
                        desired,
                        parent_id,
                        created_pending: false,
                        initial: Initial::Idle,
                    },
                );
                self.registry.insert(&name, handle.clone(), Some(guard))?;
                Ok(Registered::new(handle, spec, false))
            }
            None => {
                if clustered {
                    return self
                        .register_cluster(spec, None, guard, Desired::Stopped)
                        .await;
                }
                // a new row starts `stopped`: a daemon crash before the first launch can never
                // resurrect an app that never launched
                let id = self.store().insert(&spec, None, Desired::Stopped).await?;
                spec.id = id;
                let handle = spawn_process_actor(
                    &self.ctx,
                    ActorInit {
                        spec: Arc::new(spec.clone()),
                        desired: Desired::Stopped,
                        parent_id: None,
                        created_pending: true,
                        initial: Initial::Idle,
                    },
                );
                if let Err(e) = self.registry.insert(&name, handle.clone(), Some(guard)) {
                    let _ = self.store().delete(id).await;
                    return Err(e);
                }
                Ok(Registered::new(handle, spec, true))
            }
        }
    }

    /// Send the first `Start` to a registered app.
    async fn send_first_start(&self, registered: Registered) -> Result<handle::StartOutcome> {
        let Registered {
            handle,
            spec,
            created,
        } = registered;
        // A cluster that was made by this call and fails to start deletes itself (its group
        // actor does it); one that existed is left as it is.
        self.start_command(&handle, spec, created).await
    }

    async fn start_command(
        &self,
        handle: &ProcessHandle,
        spec: AppSpec,
        created: bool,
    ) -> Result<handle::StartOutcome> {
        let spec = Arc::new(spec);
        let outcome = handle
            .request(|reply| Command::Start {
                spec,
                origin: StartOrigin::User,
                created,
                ctx: CmdCtx::user(),
                reply,
            })
            .await?;
        info!(
            "Started {} (id {}): {}, {} instances",
            outcome.name,
            outcome.id,
            outcome.status,
            outcome.child_ids.len()
        );
        Ok(outcome)
    }

    /// Register a cluster: the parent's row, one app per instance, and the group over them.
    /// Nothing is started.
    ///
    /// A new cluster is made all at once: every instance name must be free in the registry (and
    /// is reserved while this runs), then the parent and the instances are written in one
    /// transaction, so a collision or a failure leaves no row behind. A cluster whose row exists
    /// without an actor (rows from before the registry was loaded) gets its actors from the rows.
    async fn register_cluster(
        &self,
        mut spec: AppSpec,
        existing: Option<(AppId, Desired)>,
        guard: NameGuard,
        new_desired: Desired,
    ) -> Result<Registered> {
        let name = spec.name.clone();
        info!(
            "Registering cluster '{}' with {} instances",
            name, spec.instances
        );
        let child_specs: Vec<AppSpec> = (0..spec.instances)
            .map(|index| spec.for_instance(index, instance_port(&spec, index)))
            .collect();

        let (parent_id, children, created, desired) = match existing {
            Some((parent_id, desired)) => {
                let mut children = Vec::with_capacity(child_specs.len());
                for child_spec in child_specs {
                    children.push(self.cluster_child(parent_id, child_spec).await?);
                }
                (parent_id, children, false, desired)
            }
            None => {
                // all N + 1 names free, before anything is written
                let mut guards = Vec::with_capacity(child_specs.len());
                for child in &child_specs {
                    guards.push(self.registry.reserve_name(&child.name)?);
                }
                let (parent_id, ids) = self
                    .store()
                    .insert_group(&spec, &child_specs, new_desired)
                    .await?;
                let mut children = Vec::with_capacity(ids.len());
                for ((mut child_spec, id), child_guard) in
                    child_specs.into_iter().zip(ids).zip(guards)
                {
                    child_spec.id = id;
                    let child_name = child_spec.name.clone();
                    let handle = spawn_process_actor(
                        &self.ctx,
                        ActorInit {
                            spec: Arc::new(child_spec),
                            desired: Desired::Stopped,
                            parent_id: Some(parent_id),
                            created_pending: new_desired == Desired::Stopped,
                            initial: Initial::Idle,
                        },
                    );
                    if let Err(e) = self.registry.insert(&child_name, handle, Some(child_guard)) {
                        self.undo_import(parent_id, &children).await;
                        return Err(e);
                    }
                    children.push(id);
                }
                (parent_id, children, true, new_desired)
            }
        };
        spec.id = parent_id;

        let group = spawn_group_actor(
            &self.ctx,
            GroupInit {
                spec: Arc::new(spec.clone()),
                desired,
                children,
            },
        );
        if let Err(e) = self.registry.insert(&name, group.clone(), Some(guard)) {
            if created {
                let children = group.snapshot().children.clone();
                self.undo_import(parent_id, &children).await;
            }
            return Err(e);
        }
        Ok(Registered {
            handle: group,
            spec,
            created,
        })
    }

    /// The registered app of one instance: the one that is there, one made from its row, or a
    /// new one with a new row, when a stored cluster needs its actors restored.
    async fn cluster_child(&self, parent_id: AppId, mut child_spec: AppSpec) -> Result<AppId> {
        let name = child_spec.name.clone();
        if let Some(existing) = self.registry.by_name(&name) {
            return if existing.snapshot().parent_id == Some(parent_id) {
                Ok(existing.id)
            } else {
                Err(Error::AppAlreadyExists(name))
            };
        }
        let (spec, desired, created) = match self.store().record_by_name(&name).await? {
            Some(AppRecord::Ok {
                spec,
                desired,
                parent_id: parent,
            }) if parent == Some(parent_id) => (spec, desired, false),
            Some(AppRecord::Ok { .. }) => return Err(Error::AppAlreadyExists(name)),
            Some(AppRecord::Broken { name, error, .. }) => {
                return Err(Error::StateIncompatible(format!(
                    "stored spec of app '{}' is invalid: {} (delete the app and re-create it)",
                    name, error
                )))
            }
            None => {
                let id = self
                    .store()
                    .insert(&child_spec, Some(parent_id), Desired::Stopped)
                    .await?;
                child_spec.id = id;
                (child_spec, Desired::Stopped, true)
            }
        };
        let id = spec.id;
        let handle = spawn_process_actor(
            &self.ctx,
            ActorInit {
                spec: Arc::new(spec),
                desired,
                parent_id: Some(parent_id),
                created_pending: created,
                initial: Initial::Idle,
            },
        );
        self.registry.insert(&name, handle, None)?;
        Ok(id)
    }

    // ---- stop, restart, delete -------------------------------------------------------

    /// Stop an application
    pub async fn stop(&self, id: u32) -> Result<bool> {
        // An id that is not registered has nothing to stop.
        let Some(handle) = self.registry.get(id) else {
            return Ok(false);
        };
        let outcome = handle
            .request(|reply| Command::Stop {
                ctx: CmdCtx::user(),
                reply,
            })
            .await?;
        if let Some(warning) = outcome.warning {
            warn!("Stopping app {}: {}", id, warning);
        }
        Ok(outcome.changed)
    }

    /// Stop several apps (`stop all`, a tag), dependents before their dependencies: the apps are
    /// grouped into layers of the dependency graph (the shutdown's grouping), the layers are
    /// stopped from the top down, and the apps of one layer concurrently. A cluster is stopped
    /// through its instances, which join the layers, and then through its group; its result
    /// says "changed" if any instance was.
    ///
    /// One result per requested id, in the order given. An id that is not registered has
    /// nothing to stop (`Ok(false)`), as with [`stop`](Self::stop).
    pub async fn stop_many(&self, ids: &[u32]) -> Vec<(u32, Result<bool>)> {
        let mut seen = HashSet::new();
        let requested: Vec<u32> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        let found: Vec<ProcessHandle> = requested
            .iter()
            .filter_map(|id| self.registry.get(*id))
            .collect();

        // the instances of a requested cluster take part in the layering
        let mut all: Vec<ProcessHandle> = found.clone();
        for group in found.iter().filter(|h| h.kind == HandleKind::Group) {
            for child in group.snapshot().children.iter() {
                if seen.insert(*child) {
                    all.extend(self.registry.get(*child));
                }
            }
        }

        let mut results: HashMap<u32, Result<bool>> = HashMap::new();
        for layer in Self::stop_layers(&all) {
            let stops = layer
                .iter()
                .map(|h| async move { (h.id, self.stop(h.id).await) });
            results.extend(futures::future::join_all(stops).await);
        }
        let groups: Vec<&ProcessHandle> =
            all.iter().filter(|h| h.kind == HandleKind::Group).collect();
        for group in groups {
            let mut result = self.stop(group.id).await;
            if let Ok(changed) = &mut result {
                let children = group.snapshot().children.clone();
                *changed |= children
                    .iter()
                    .any(|c| matches!(results.get(c), Some(Ok(true))));
            }
            results.insert(group.id, result);
        }

        requested
            .into_iter()
            .map(|id| (id, results.remove(&id).unwrap_or(Ok(false))))
            .collect()
    }

    /// Restart an application
    pub async fn restart(&self, id: u32) -> Result<bool> {
        Ok(self
            .restart_outcome(id, RestartCause::User)
            .await?
            .is_some())
    }

    /// Reload an application. A reload is a restart: there is no zero-downtime path.
    pub async fn reload(&self, id: u32) -> Result<bool> {
        Ok(self.reload_outcome(id).await?.is_some())
    }

    /// `reload`, with the outcome (its kind is always `Restart`). `None` if the app is not
    /// registered.
    pub async fn reload_outcome(&self, id: u32) -> Result<Option<RestartOutcome>> {
        self.restart_outcome(id, RestartCause::Reload).await
    }

    async fn restart_outcome(
        &self,
        id: u32,
        cause: RestartCause,
    ) -> Result<Option<RestartOutcome>> {
        self.refuse_if_closed()?;
        let Some(handle) = self.registry.get(id) else {
            return Ok(None);
        };
        // A restart ends the running child before it asks the runtime adapter for the new
        // launch. With the adapter down that would kill a healthy app and leave it errored, so
        // the look is taken first and the app keeps running.
        self.ctx.runtimes.ready(&handle.snapshot().spec.mode)?;
        let outcome = handle
            .request(|reply| Command::Restart {
                cause,
                ctx: CmdCtx::with_cause(cause.cause()),
                reply,
            })
            .await?;
        Ok(Some(outcome))
    }

    /// Delete an application
    pub async fn delete(&self, id: u32) -> Result<bool> {
        match self.registry.get(id) {
            Some(handle) => {
                handle
                    .request(|reply| Command::Delete {
                        row_removed: false,
                        ctx: CmdCtx::user(),
                        reply,
                    })
                    .await?;
            }
            // not registered (an app from before the daemon restarted): only the row
            None => {
                let name = self
                    .store()
                    .record_by_id(id)
                    .await?
                    .map(|r| r.name().to_string());
                self.store().delete(id).await?;
                if let Some(name) = name {
                    self.ctx.runtimes.forget(&name).await;
                }
            }
        }
        info!("Deleted app (id: {})", id);
        Ok(true)
    }

    // ---- reads -----------------------------------------------------------------------

    /// The spec and state of a registered app, from its snapshot. The markers come from the
    /// handle (the one source of truth), not from the copy in the spec.
    fn info_of(handle: &ProcessHandle) -> AppInfo {
        let mut spec = (*handle.snapshot().spec).clone();
        spec.protected = handle.markers().protected();
        spec.ai_control = handle.markers().ai_control();
        AppInfo::new(spec, handle.run_state())
    }

    /// Empty the active log files of an app. `Ok(false)` if there is no such app.
    pub async fn flush(&self, id: u32) -> Result<bool> {
        match self.registry.get(id) {
            Some(handle) => {
                handle.request(|reply| Command::FlushLogs { reply }).await?;
                Ok(true)
            }
            // not registered (an app from before the daemon restarted): no writer exists
            None => match self.store().record_by_id(id).await? {
                Some(record) => {
                    actor::truncate_log_files(&self.paths, record.name())?;
                    Ok(true)
                }
                None => Ok(false),
            },
        }
    }

    /// Get status of all apps (from the snapshots of the registry, oldest id first)
    pub async fn status(&self) -> Result<Vec<AppInfo>> {
        Ok(self.registry.list().iter().map(Self::info_of).collect())
    }

    /// The registered apps a selector names, in id order; `All` names every one. Reads the
    /// snapshots only (a tag is matched on the live spec), so it never waits for an actor.
    fn matching(&self, selector: &Selector) -> Vec<ProcessHandle> {
        match selector {
            Selector::All => self.registry.list(),
            Selector::ById(id) => self.registry.get(*id).into_iter().collect(),
            Selector::ByName(name) => self.registry.by_name(name).into_iter().collect(),
            Selector::ByTag(tag) => self
                .registry
                .list()
                .into_iter()
                .filter(|h| h.snapshot().spec.tags.contains(tag))
                .collect(),
        }
    }

    /// Get info for a single app. `All` names none; a tag names the first app that has it.
    pub async fn show(&self, selector: &Selector) -> Result<Option<AppInfo>> {
        if matches!(selector, Selector::All) {
            return Ok(None);
        }
        Ok(self.matching(selector).first().map(Self::info_of))
    }

    /// Get logs for an app
    pub async fn logs(
        self: &Arc<Self>,
        selector: &Selector,
        lines: usize,
        stdout: bool,
        stderr: bool,
    ) -> Result<Vec<String>> {
        let supervisor = Arc::clone(self);
        let selector = selector.clone();
        tokio::task::spawn_blocking(move || {
            supervisor.read_logs(&selector, lines, stdout, stderr, None)
        })
        .await
        .map_err(|e| Error::Internal(format!("the log read failed: {e}")))?
    }

    /// The log lines of an app, read on the calling thread (a caller that must not hold an
    /// async worker runs it on a blocking one). With `scrub_from` every environment value of the
    /// app that is at least that many bytes long is replaced by `<redacted>`, longest first: an
    /// exact-match filter, for readers that are not a person.
    pub fn read_logs(
        &self,
        selector: &Selector,
        lines: usize,
        stdout: bool,
        stderr: bool,
        scrub_from: Option<usize>,
    ) -> Result<Vec<String>> {
        if matches!(selector, Selector::All) {
            return Err(Error::InvalidSelector("Cannot get logs for 'all'".into()));
        }
        // For tags, return logs from the first matching app
        let handle = self
            .matching(selector)
            .into_iter()
            .next()
            .ok_or_else(|| Error::AppNotFound(selector.to_string()))?;
        let name = handle.snapshot().spec.name.clone();

        // Bound retained data before serialization, including requests for large histories.
        const MAX_LINES: usize = 10_000;
        const MAX_STREAM_BYTES: usize = 1024 * 1024;
        let lines = lines.min(MAX_LINES);

        let mut all_lines = Vec::new();

        if stdout || !stderr {
            let reader = LogReader::new(self.paths.log_path(&name, "out"));
            all_lines.extend(reader.tail_bounded(lines, MAX_STREAM_BYTES)?);
        }

        if stderr || !stdout {
            let reader = LogReader::new(self.paths.log_path(&name, "err"));
            all_lines.extend(reader.tail_bounded(lines, MAX_STREAM_BYTES)?);
        }

        // Sort by timestamp if we have both
        if stdout && stderr {
            all_lines.sort();
        }

        // Limit to requested lines
        if all_lines.len() > lines {
            let skip_count = all_lines.len() - lines;
            all_lines = all_lines.into_iter().skip(skip_count).collect();
        }

        if let Some(min) = scrub_from {
            let spec = handle.snapshot().spec.clone();
            let values: Vec<&str> = spec
                .env
                .values()
                .map(String::as_str)
                .filter(|v| v.len() >= min)
                .collect();
            if !values.is_empty() {
                for line in &mut all_lines {
                    *line = oxidepm_core::scrub_message(line, &values);
                }
            }
        }

        Ok(all_lines)
    }

    /// Resolve selector to app IDs, from the registry (an instance whose cluster is in the set too is dropped). A selector that names nothing is
    /// `AppNotFound`, except `all`, which names every app there is (possibly none).
    pub async fn resolve_selector(&self, selector: &Selector) -> Result<Vec<u32>> {
        let matched = self.matching(selector);
        // A cluster and its instances matched together (a tag, `all`) is one thing to act on:
        // the cluster. An instance named on its own is acted on alone.
        let ids: Vec<u32> = matched
            .iter()
            .filter(|h| match h.snapshot().parent_id {
                Some(parent) => !matched.iter().any(|other| other.id == parent),
                None => true,
            })
            .map(|h| h.id)
            .collect();
        if ids.is_empty() && !matches!(selector, Selector::All) {
            return Err(Error::AppNotFound(match selector {
                Selector::ByTag(tag) => format!("@{}", tag),
                Selector::ById(id) => id.to_string(),
                Selector::ByName(name) => name.clone(),
                Selector::All => unreachable!("handled above"),
            }));
        }
        Ok(ids)
    }

    /// Change the policy markers of `ids` (and, for a cluster, of every instance) without a
    /// restart: the rows first, in one transaction, and only then the atomics on the handles,
    /// so a failed write changes nothing. A marker given as `None` stays as it is. One result
    /// per id that is registered, in the order given.
    pub async fn set_markers(
        &self,
        ids: &[u32],
        protected: Option<bool>,
        ai_control: Option<bool>,
    ) -> Result<Vec<MarkerChange>> {
        if protected.is_none() && ai_control.is_none() {
            return Err(Error::InvalidSpec(
                "set_policy needs protected and/or ai_control".to_string(),
            ));
        }
        self.refuse_if_closed()?;
        // every app that takes part: the named ones and the instances of a named cluster
        let mut groups: Vec<(ProcessHandle, Vec<ProcessHandle>)> = Vec::new();
        for id in ids {
            let Some(handle) = self.registry.get(*id) else {
                continue;
            };
            let mut members = vec![handle.clone()];
            if handle.kind == HandleKind::Group {
                members.extend(
                    handle
                        .snapshot()
                        .children
                        .iter()
                        .filter_map(|c| self.registry.get(*c)),
                );
            }
            groups.push((handle, members));
        }
        let rows: Vec<u32> = {
            let mut seen = HashSet::new();
            groups
                .iter()
                .flat_map(|(_, members)| members.iter().map(|m| m.id))
                .filter(|id| seen.insert(*id))
                .collect()
        };
        self.store()
            .set_markers(&rows, protected, ai_control)
            .await?;

        let mut out = Vec::with_capacity(groups.len());
        for (handle, members) in groups {
            let mut changed = false;
            for member in &members {
                let m = member.markers();
                let (was_protected, was_open) = (m.protected(), m.ai_control());
                let (now_protected, now_open) = (
                    protected.unwrap_or(was_protected),
                    ai_control.unwrap_or(was_open),
                );
                changed |= (was_protected, was_open) != (now_protected, now_open);
                m.set(now_protected, now_open);
            }
            out.push(MarkerChange {
                id: handle.id,
                name: handle.snapshot().spec.name.clone(),
                changed,
                protected: handle.markers().protected(),
                ai_control: handle.markers().ai_control(),
            });
        }
        info!("Policy markers set on {} app(s)", out.len());
        Ok(out)
    }

    /// What the gate needs to know about the apps a request reaches: for each id, the app and
    /// (for a cluster) every instance, with the markers read from the handles. An instance counts
    /// as protected when it or its cluster is, and as open to `ai` control only when both are.
    pub fn target_views(&self, ids: &[u32]) -> Vec<TargetView> {
        let view = |handle: &ProcessHandle, parent: Option<&ProcessHandle>| {
            // the adapters the app or its cluster lists (the scope of an `opted-in` grant)
            let mut adapters: Vec<String> =
                handle.snapshot().spec.adapters.keys().cloned().collect();
            if let Some(parent) = parent {
                adapters.extend(parent.snapshot().spec.adapters.keys().cloned());
                adapters.sort();
                adapters.dedup();
            }
            TargetView {
                id: handle.id,
                name: handle.snapshot().spec.name.clone(),
                protected: handle.markers().protected()
                    || parent.is_some_and(|p| p.markers().protected()),
                ai_control: handle.markers().ai_control()
                    && parent.is_none_or(|p| p.markers().ai_control()),
                adapters,
            }
        };
        let mut views = Vec::new();
        for id in ids {
            let Some(handle) = self.registry.get(*id) else {
                continue;
            };
            let parent = handle
                .snapshot()
                .parent_id
                .and_then(|p| self.registry.get(p));
            views.push(view(&handle, parent.as_ref()));
            if handle.kind == HandleKind::Group {
                for child in handle.snapshot().children.iter() {
                    if let Some(child) = self.registry.get(*child) {
                        views.push(view(&child, Some(&handle)));
                    }
                }
            }
        }
        views
    }

    // ---- save and resurrect --------------------------------------------------------------

    /// Register an app from the saved file that the registry does not have: validated like any
    /// spec from outside, a row with `desired = running`, an idle actor (a cluster: its
    /// instances and the group over them). Nothing is launched; the resurrect that called this
    /// starts it with the others. `Ok(false)`: the name is taken after all.
    async fn import_spec(&self, spec: AppSpec) -> Result<bool> {
        let mut spec = self.admit(spec)?;
        let name = spec.name.clone();
        let guard = match self.registry.reserve_name(&name) {
            Ok(guard) => guard,
            Err(Error::AppAlreadyExists(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        if spec.instances > 1 {
            self.register_cluster(spec, None, guard, Desired::Running)
                .await?;
            return Ok(true);
        }

        let id = self.store().insert(&spec, None, Desired::Running).await?;
        spec.id = id;
        let handle = spawn_process_actor(
            &self.ctx,
            ActorInit {
                spec: Arc::new(spec),
                desired: Desired::Running,
                parent_id: None,
                created_pending: false,
                initial: Initial::Idle,
            },
        );
        if let Err(e) = self.registry.insert(&name, handle, Some(guard)) {
            self.undo_import(id, &[]).await;
            return Err(e);
        }
        Ok(true)
    }

    /// An import that failed half way: the parent row (its instance rows go with it) and the
    /// instances already registered.
    async fn undo_import(&self, parent: AppId, children: &[AppId]) {
        let _ = self.store().delete(parent).await;
        for child in children {
            self.registry.remove(*child);
        }
    }

    /// Save the process list: the top-level apps the database says should be running.
    ///
    /// The database is the registry, so an empty in-memory map can no longer blank the file.
    /// A save that would replace a file that lists apps with an empty list is refused
    /// (`SaveWouldEmpty`) unless `force`.
    pub async fn save(&self, force: bool) -> Result<usize> {
        let mut specs: Vec<AppSpec> = Vec::new();
        for record in self.store().records().await? {
            match record {
                AppRecord::Ok {
                    spec,
                    parent_id: None,
                    desired: Desired::Running,
                } => specs.push(spec),
                AppRecord::Broken {
                    id,
                    name,
                    parent_id: None,
                    desired: Desired::Running,
                    ..
                } => warn!(
                    "Not saving app '{}' (id {}): its stored spec is invalid",
                    name, id
                ),
                _ => {}
            }
        }
        let path = self.paths.saved();
        if specs.is_empty() && !force && persist::saved_app_count(&path).is_some_and(|n| n > 0) {
            return Err(persist::would_empty(&path));
        }
        persist::write_saved(&self.paths, &specs)
    }

    /// Resurrect saved processes; the number started. An unusable saved file is an error.
    pub async fn resurrect(&self) -> Result<usize> {
        Ok(self.resurrect_report().await?.started)
    }

    /// An explicit resurrect: like [`resurrect_with`](Self::resurrect_with), but a saved file
    /// that cannot be used is the call's error (nothing is started then).
    pub async fn resurrect_report(&self) -> Result<ResurrectReport> {
        self.resurrect_inner(&|_, _| {}, true).await
    }

    /// The boot resurrect, reporting `(done, total)` as it goes. A saved file that cannot be
    /// used is reported in the result, not as an error: the apps of the registry still start.
    pub async fn resurrect_with(
        &self,
        progress: impl Fn(usize, usize) + Sync,
    ) -> Result<ResurrectReport> {
        self.resurrect_inner(&progress, false).await
    }

    /// Import what the saved file has and the registry does not, then start every app that is
    /// meant to run and is not up (4.5). Only one resurrect runs at a time: a second caller
    /// waits for the first and then finds nothing left to start.
    ///
    /// The targets are checked as a graph and started together, one `Start` with the `Resurrect`
    /// origin per app. Validation and launch reuse batch start logic, and every target is already
    /// registered): the actor re-reads the desired state when it accepts it, so a stop that
    /// lands after the targets were chosen is never overridden, and the dependency wait orders
    /// the launches. A failure is that app's, reported in the result; it stays registered
    /// (errored, still meant to run).
    async fn resurrect_inner(
        &self,
        progress: &(dyn Fn(usize, usize) + Sync),
        strict: bool,
    ) -> Result<ResurrectReport> {
        self.refuse_if_closed()?;
        let _only_one = self.resurrect_lock.lock().await;
        let mut report = ResurrectReport::default();

        let entries = match persist::read_saved(&self.paths.saved()) {
            Ok(entries) => entries,
            Err(e) if strict => return Err(e),
            Err(e) => {
                report.saved_error = Some(e);
                Vec::new()
            }
        };

        // what the file lists and the registry does not have is registered first
        // (this is what lets `delete all` followed by `resurrect` restore the saved list);
        // a name the registry has is ignored, the stored copy is newer
        for entry in entries {
            self.refuse_if_closed()?;
            match entry {
                SavedEntry::Bad { name, error } => {
                    let name = name.unwrap_or_else(|| "(unreadable entry)".to_string());
                    warn!("Saved entry '{}' cannot be used: {}", name, error);
                    report.saved_rejected += 1;
                    report.failed.push(ResurrectFailure {
                        id: 0,
                        name,
                        error: Error::StateIncompatible(error),
                    });
                }
                // A cluster instance is recreated by its cluster's own entry; on its own it is
                // not a spec a client could send (it carries the instance id `admit` resets).
                SavedEntry::Spec(spec) if spec.instance_id.is_some() => {}
                SavedEntry::Spec(spec) => {
                    if self.registry.by_name(&spec.name).is_some() {
                        continue;
                    }
                    let name = spec.name.clone();
                    match self.import_spec(*spec).await {
                        Ok(true) => report.imported += 1,
                        Ok(false) => {}
                        Err(Error::ShuttingDown) => return Err(Error::ShuttingDown),
                        Err(error) => {
                            warn!("Failed to import {} from the saved file: {}", name, error);
                            report.saved_rejected += 1;
                            report.failed.push(ResurrectFailure { id: 0, name, error });
                        }
                    }
                }
            }
        }

        // the top-level apps that should run and are not up (instances start with
        // their cluster)
        let targets: Vec<ProcessHandle> = self
            .registry
            .list()
            .into_iter()
            .filter(|h| {
                let snap = h.snapshot();
                snap.parent_id.is_none()
                    && snap.desired == Desired::Running
                    && !snap.state.status.is_active()
            })
            .collect();

        // the graph of the targets. A dependency outside them must be registered and
        // not at rest; a cycle and everything that depends on a refused app fails here and is
        // reported, the rest is started together (2.3)
        let total = targets.len();
        progress(0, total);
        let specs: Vec<Arc<AppSpec>> = targets.iter().map(|h| h.snapshot().spec.clone()).collect();
        let in_set: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let refs: Vec<&AppSpec> = specs.iter().map(|s| s.as_ref()).collect();
        let mut refused = deps::reject_set(&self.registry, &refs, &in_set, HashMap::new());
        let mut done = 0;
        let mut jobs = Vec::new();
        for (handle, spec) in targets.into_iter().zip(specs) {
            match refused.remove(&spec.name) {
                Some(error) => {
                    warn!("Not resurrecting {}: {}", spec.name, error);
                    report.failed.push(ResurrectFailure {
                        id: handle.id,
                        name: spec.name.clone(),
                        error,
                    });
                    done += 1;
                    progress(done, total);
                }
                None => jobs.push((handle, spec)),
            }
        }

        // one Start per app, all at once. The actor re-reads the desired state when it
        // accepts it, so a stop that lands after the targets were chosen is never overridden;
        // the dependency wait inside the actors orders the launches.
        let mut sends: futures::stream::FuturesUnordered<_> = jobs
            .into_iter()
            .map(|(handle, spec)| async move {
                let name = spec.name.clone();
                let result = handle
                    .request(|reply| Command::Start {
                        spec,
                        origin: StartOrigin::Resurrect,
                        created: false,
                        ctx: CmdCtx::resurrect(),
                        reply,
                    })
                    .await;
                (handle.id, name, result)
            })
            .collect();
        while let Some((id, name, result)) = futures::StreamExt::next(&mut sends).await {
            match result {
                Ok(outcome) => {
                    if outcome.changed {
                        report.started += 1;
                    }
                }
                // A shutdown ends the resurrect and counts as an unfinished one, so the caller
                // does not mistake the apps that were never started for "all done".
                Err(Error::ShuttingDown) => return Err(Error::ShuttingDown),
                Err(error) => {
                    warn!("Failed to resurrect {}: {}", name, error);
                    report.failed.push(ResurrectFailure { id, name, error });
                }
            }
            done += 1;
            progress(done, total);
        }

        info!(
            "Resurrected {} processes ({} imported, {} failed)",
            report.started,
            report.imported,
            report.failed.len()
        );
        Ok(report)
    }

    // ---- shutdown ----------------------------------------------------------------------

    /// Close the supervisor and stop everything (the daemon's shutdown).
    ///
    /// 1. Close the gate: from now on `start`, `restart`, `reload` and `resurrect` answer
    ///    `ShuttingDown`, and a start that is already in flight is cancelled or, if it has
    ///    already spawned, killed again. The specs of the apps that were running at that
    ///    instant are returned for `saved.json`.
    /// 2. Stop every registered app (not only the running ones: an app that is stopped but has
    ///    a start in flight must learn about the shutdown too), dependents before their
    ///    dependencies, layer by layer. The `budget` is shared out over the layers: each layer
    ///    waits at most [`kill_cap`] for its apps after SIGTERM, whatever their own kill
    ///    timeouts say.
    /// 3. An app that does not answer within its layer's cap plus [`REPLY_GRACE`] has its
    ///    actor told to kill the group.
    ///
    /// Apps keep their registry entry and their desired state: they come back on resurrect.
    pub async fn shutdown(&self, budget: Duration) -> ShutdownReport {
        let (running, handles) = self.close();
        let layers = Self::stop_layers(&handles);
        let started = Instant::now();
        let total = layers.len();

        let mut stopped = 0;
        for (index, layer) in layers.iter().enumerate() {
            let cap = kill_cap(budget.saturating_sub(started.elapsed()), total - index);
            debug!(
                "Stopping layer {} of {} ({} apps, cap {:?})",
                index + 1,
                total,
                layer.len(),
                cap
            );
            let results =
                futures::future::join_all(layer.iter().map(|h| Self::stop_for_shutdown(h, cap)))
                    .await;
            stopped += results.into_iter().filter(|stopped| *stopped).count();
        }
        // the group actors have nothing to stop; they just close their mailbox
        let groups = handles.iter().filter(|h| h.kind == HandleKind::Group);
        futures::future::join_all(groups.map(|h| Self::stop_for_shutdown(h, MIN_KILL_CAP))).await;

        info!(
            "Stopped {} apps for shutdown in {:?}",
            stopped,
            started.elapsed()
        );
        ShutdownReport { stopped, running }
    }

    /// Kill every app's group at once (a second shutdown signal). Stops nothing gracefully and
    /// waits for nothing; the shutdown that is already running notices the actors finishing.
    /// Each actor kills its own group, so a pid is never signalled from here.
    pub fn kill_all_now(&self) {
        for handle in self.registry.list() {
            handle.kill_now();
        }
        for group in self.hooks.live_groups() {
            let _ = kill_group(group, Signal::SIGKILL);
        }
    }

    /// Close the gate and return the specs of the apps that are running with every handle that
    /// was registered at that instant.
    fn close(&self) -> (Vec<AppSpec>, Vec<ProcessHandle>) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return (Vec::new(), Vec::new()); // already closed: whoever closed it has the snapshot
        }
        let handles = self.registry.close();
        let mut running: Vec<(u32, AppSpec)> = handles
            .iter()
            .filter_map(|h| {
                let snap = h.snapshot();
                (snap.state.status.is_active() && snap.desired == Desired::Running)
                    .then(|| (h.id, (*snap.spec).clone()))
            })
            .collect();
        running.sort_by_key(|(id, _)| *id);
        (running.into_iter().map(|(_, spec)| spec).collect(), handles)
    }

    /// One app's shutdown, with the reply-timeout fallback. True if it stopped something.
    async fn stop_for_shutdown(handle: &ProcessHandle, cap: Duration) -> bool {
        let command = handle.request_unchecked(|reply| Command::Shutdown {
            kill_cap: cap,
            reply,
        });
        match tokio::time::timeout(cap + REPLY_GRACE, command).await {
            Ok(Ok(stopped)) => stopped,
            Ok(Err(e)) => {
                error!("Error stopping app {} for shutdown: {}", handle.id, e);
                false
            }
            Err(_) => {
                warn!(
                    "App {} did not stop within {:?}; killing it",
                    handle.id,
                    cap + REPLY_GRACE
                );
                handle.kill_now();
                tokio::time::sleep(KILL_GRACE).await;
                handle.abort();
                false
            }
        }
    }

    /// The registered apps grouped for stopping: dependents first, their dependencies in later
    /// layers. Cluster parents are not nodes (they own no process); an app that depends on a
    /// cluster waits for all of its instances. A dependency cycle (which `validate_set` should
    /// have refused) falls back to stopping everything in one layer.
    fn stop_layers(handles: &[ProcessHandle]) -> Vec<Vec<ProcessHandle>> {
        let snapshots: HashMap<AppId, Arc<handle::Snapshot>> =
            handles.iter().map(|h| (h.id, h.snapshot())).collect();

        // Instance names of every cluster parent, so that "depends on web" means its instances
        let clusters: HashMap<&str, Vec<&str>> = handles
            .iter()
            .filter(|h| h.kind == HandleKind::Group)
            .map(|h| {
                let snap = &snapshots[&h.id];
                let instances = snap
                    .children
                    .iter()
                    .filter_map(|id| snapshots.get(id).map(|i| i.spec.name.as_str()))
                    .collect();
                (snap.spec.name.as_str(), instances)
            })
            .collect();

        let mut by_name: HashMap<&str, Vec<&ProcessHandle>> = HashMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        for handle in handles.iter().filter(|h| h.kind == HandleKind::Process) {
            let spec = &snapshots[&handle.id].spec;
            let name = spec.name.as_str();
            by_name.entry(name).or_default().push(handle);
            let mut depends: Vec<String> = Vec::new();
            for dep in &spec.depends_on {
                match clusters.get(dep.as_str()) {
                    Some(instances) => depends.extend(instances.iter().map(|n| n.to_string())),
                    None => depends.push(dep.clone()),
                }
            }
            if !nodes.iter().any(|(n, _)| n == name) {
                nodes.push((name.to_string(), depends));
            }
        }

        match topo_layers(&nodes) {
            Ok(layers) => layers
                .into_iter()
                .rev()
                .map(|layer| {
                    layer
                        .iter()
                        .flat_map(|name| by_name.get(name.as_str()).cloned().unwrap_or_default())
                        .cloned()
                        .collect::<Vec<ProcessHandle>>()
                })
                .filter(|layer| !layer.is_empty())
                .collect(),
            Err(cycle) => {
                warn!("Dependency cycle {:?}; stopping every app at once", cycle);
                let all: Vec<ProcessHandle> =
                    by_name.values().flatten().map(|h| (*h).clone()).collect();
                if all.is_empty() {
                    Vec::new()
                } else {
                    vec![all]
                }
            }
        }
    }

    /// Wait for the lifecycle hooks that are still running (their grace is
    /// `HookRunner::drain_grace`), then SIGKILL the process group of each that is still there,
    /// so no hook outlives the daemon. The shutdown calls this after every app is stopped.
    pub async fn drain_hooks(&self) {
        self.hooks.drain(self.hooks.drain_grace()).await;
    }
}

/// The spec of a placeholder for a stored row that does not parse: only what the columns say.
fn placeholder_spec(id: u32, name: &str, mode: &str, protected: bool) -> AppSpec {
    let mode = mode.parse().unwrap_or(Runtime::CMD);
    let mut spec = AppSpec::try_new(
        name.to_string(),
        mode.clone(),
        String::new(),
        PathBuf::from("/"),
    )
    .or_else(|_| {
        AppSpec::try_new(
            format!("broken-{}", id),
            mode,
            String::new(),
            PathBuf::from("/"),
        )
    })
    .expect("a generated name is valid");
    spec.id = id;
    spec.protected = protected;
    spec
}
