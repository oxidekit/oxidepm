//! The adapter host: the registry of installed adapters, loaded at boot and stopped at shutdown.
//!
//! `AdapterHost::load` runs in `Daemon::with_paths` before the boot resurrect is spawned. It reads
//! the `adapters` table (a database read, no process is started) and builds one
//! [`AdapterEntry`] per row; nothing it does can stop the daemon from booting: a database that
//! cannot be read, a directory that is a file, a manifest that no longer validates are warnings
//! and `failed` entries, never errors. A home with no `adapters.db` runs no code at all.
//!
//! Services are started in the background after the boot, at most four at a time with a stagger;
//! tools wait for their first call. Nothing in the boot or the resurrect waits for any of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use oxidepm_adapter_proto::messages::Grant;
use oxidepm_adapter_proto::{IndexKey, Kind, Manifest, Permissions, ValidateCtx};
use oxidepm_core::Paths;
use oxidepm_db::AdapterRow;
use oxidepm_ipc::AdapterStatus;
use parking_lot::{Mutex, RwLock};
use tracing::{info, warn};

use super::entry::{AdapterEntry, EntryConfig, EntrySpec, EntryState, FailReason, ProfileRecipe};
use super::events::EventHub;
use super::hostapi::{Dispatch, HostApi, HostLink};
use super::peer::{self, AdapterProc, ProcReader};
use super::proc::{EnvSource, Launch, SandboxSetup};
use super::store::AdapterStore;
use crate::audit::Audit;
use crate::policy::{AdapterGrant, PeerLabel};
use crate::supervisor::PidTable;
use oxidepm_sandbox::{EnforcedRecord, Profile, ProfileInput};

/// Services that may be starting at the same moment at boot.
const BOOT_PARALLEL: usize = 4;
/// The pause between two service starts at boot.
const BOOT_STAGGER: Duration = Duration::from_millis(500);
/// The most grace the services get at shutdown, all together.
pub const SERVICE_GRACE: Duration = Duration::from_secs(5);

/// Adapter host settings. Tests can override [`HostConfig::production`].
#[derive(Clone)]
pub struct HostConfig {
    pub entry: EntryConfig,
    pub env_source: EnvSource,
    pub boot_stagger: Duration,
    /// The sandbox launcher `oxidepm-sandbox`; `None` when it is not installed.
    pub launcher: Option<PathBuf>,
    /// A pause before a main-socket peer is looked at, so that a test can let a short-lived peer
    /// go first (always zero in production; only a test build reads it from the environment).
    pub peer_walk_delay: Duration,
    /// The keys an index may be signed with (the embedded ones; a test build reads
    /// `OXIDEPM_TEST_INDEX_KEYS`).
    pub index_keys: Vec<IndexKey>,
    /// How long an updated adapter is watched before the update is kept (30 s; a test build reads
    /// `OXIDEPM_TEST_PROBATION_MS`).
    pub probation: Duration,
    /// How long one line of a relayed verb's output may take to reach the CLI before the call is
    /// cancelled (30 s; a test build reads `OXIDEPM_TEST_RELAY_WRITE_MS`).
    pub relay_write: Duration,
    /// How long the tokens of a session are valid (60 s; a test build reads
    /// `OXIDEPM_TEST_SESSION_WINDOW_MS`).
    pub session_window: Duration,
    /// The longest a session lasts (12 hours; a test build reads `OXIDEPM_TEST_SESSION_MAX_MS`).
    pub session_max: Duration,
    /// `$XDG_RUNTIME_DIR`, where session sockets go if the home's path is too long for one.
    pub runtime_dir: Option<PathBuf>,
}

/// A relayed line that the client does not take within this long ends the call.
pub const RELAY_WRITE: Duration = Duration::from_secs(30);

/// Test builds only: the numbers of relayed verbs come from the environment.
#[cfg(feature = "test-hooks")]
fn test_millis(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(not(feature = "test-hooks"))]
fn test_millis(_name: &str) -> Option<Duration> {
    None
}

/// Test builds only: `OXIDEPM_TEST_INVOKE_SLOTS` sets the relayed verbs one adapter runs at once.
#[cfg(feature = "test-hooks")]
fn test_invoke_slots() -> Option<usize> {
    std::env::var("OXIDEPM_TEST_INVOKE_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
}

#[cfg(not(feature = "test-hooks"))]
fn test_invoke_slots() -> Option<usize> {
    None
}

/// Test builds only: the keys and the probation come from the environment.
#[cfg(feature = "test-hooks")]
fn test_index_keys() -> Option<Vec<IndexKey>> {
    let list = std::env::var("OXIDEPM_TEST_INDEX_KEYS").ok()?;
    Some(
        list.split(',')
            .filter(|k| !k.trim().is_empty())
            .filter_map(|k| IndexKey::parse(k.trim()).ok())
            .collect(),
    )
}

#[cfg(not(feature = "test-hooks"))]
fn test_index_keys() -> Option<Vec<IndexKey>> {
    None
}

#[cfg(feature = "test-hooks")]
fn test_probation() -> Option<Duration> {
    std::env::var("OXIDEPM_TEST_PROBATION_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(not(feature = "test-hooks"))]
fn test_probation() -> Option<Duration> {
    None
}

/// An update is watched this long (a tool: until it has answered `initialize`).
pub const PROBATION: Duration = Duration::from_secs(30);

/// The directories the names of `spawn.mode = named` are looked up in: the adapter's own `PATH`.
const ADAPTER_PATH: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// The launcher installed next to this executable (the three binaries are installed, staged and
/// updated together).
pub fn find_launcher() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("oxidepm-sandbox");
    std::fs::metadata(&candidate)
        .ok()
        .filter(|m| m.is_file())
        .map(|_| candidate)
}

impl HostConfig {
    pub fn production() -> Self {
        let mut entry = EntryConfig::production();
        if let Some(idle) = test_millis("OXIDEPM_TEST_INVOKE_IDLE_MS") {
            entry.invoke_idle = idle;
        }
        if let Some(slots) = test_invoke_slots() {
            entry.invoke_slots = slots;
        }
        Self {
            entry,
            env_source: Arc::new(|key: &str| std::env::var(key).ok()),
            boot_stagger: BOOT_STAGGER,
            launcher: find_launcher(),
            peer_walk_delay: test_peer_walk_delay(),
            index_keys: test_index_keys().unwrap_or_else(oxidepm_adapter_proto::official_keys),
            probation: test_probation().unwrap_or(PROBATION),
            relay_write: test_millis("OXIDEPM_TEST_RELAY_WRITE_MS").unwrap_or(RELAY_WRITE),
            session_window: test_millis("OXIDEPM_TEST_SESSION_WINDOW_MS")
                .unwrap_or(super::session::DEFAULT_WINDOW),
            session_max: test_millis("OXIDEPM_TEST_SESSION_MAX_MS")
                .unwrap_or(super::session::DEFAULT_MAX),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        }
    }
}

/// How many `host.request`s an adapter may have in flight: the default, or what its grant asks
/// for, at most the ceiling.
pub fn host_in_flight_for(requested: Option<u32>, limits: &oxidepm_adapter_proto::Limits) -> u32 {
    requested.map_or(limits.host_in_flight, |n| {
        n.clamp(1, limits.max_in_flight_ceiling)
    })
}

/// Test builds only: `OXIDEPM_TEST_PEER_WALK_DELAY_MS` delays the look at a peer.
#[cfg(feature = "test-hooks")]
fn test_peer_walk_delay() -> Duration {
    std::env::var("OXIDEPM_TEST_PEER_WALK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis)
}

#[cfg(not(feature = "test-hooks"))]
fn test_peer_walk_delay() -> Duration {
    Duration::ZERO
}

/// The registry of adapters.
pub struct AdapterHost {
    pub(super) entries: RwLock<BTreeMap<String, Arc<AdapterEntry>>>,
    pub(super) warnings: Mutex<Vec<String>>,
    pub(super) store: Mutex<Option<Arc<AdapterStore>>>,
    pub(super) cfg: HostConfig,
    closing: AtomicBool,
    /// What the kernel offers the sandbox, probed once when the host is made.
    caps: oxidepm_sandbox::SandboxCaps,
    /// The queues of the services that listen to events.
    pub(super) events: Arc<EventHub>,
    /// Where the host APIs of the adapters send their requests: the daemon's entry point, bound
    /// once the daemon has built it.
    pub(super) link: Arc<HostLink>,
    /// The recorded adapter processes (`adapter-pids.json`): set when there are adapters.
    pub(super) pids: OnceLock<Arc<PidTable>>,
    /// Reads `/proc` for the attribution of main-socket peers, and counts the reads.
    proc_reader: Arc<ProcReader>,
    /// The paths and the audit log, set when the host is loaded (the install, enable, update and
    /// remove operations need them).
    pub(super) ctx: OnceLock<HostCtx>,
    /// One administrative operation at a time (install, enable, disable, remove, update).
    pub(super) admin: tokio::sync::Mutex<()>,
    /// The next process-table slot for an adapter that is added after the boot.
    pub(super) next_slot: AtomicU32,
    /// Adapters disabled because the index revokes their executable, with the reason.
    pub(super) revoked: Mutex<BTreeMap<String, String>>,
    /// Sessions that may be open at once (pending or running).
    pub(super) session_slots: Arc<tokio::sync::Semaphore>,
}

/// What the administrative operations need from the daemon.
pub(super) struct HostCtx {
    pub paths: Arc<Paths>,
    pub audit: Option<Arc<Audit>>,
}

impl AdapterHost {
    /// A host with no adapters.
    pub fn empty(cfg: HostConfig) -> Arc<AdapterHost> {
        Arc::new(AdapterHost {
            entries: RwLock::new(BTreeMap::new()),
            warnings: Mutex::new(Vec::new()),
            store: Mutex::new(None),
            cfg,
            closing: AtomicBool::new(false),
            caps: oxidepm_sandbox::probe(),
            events: EventHub::new(),
            link: HostLink::new(),
            pids: OnceLock::new(),
            proc_reader: Arc::new(ProcReader::default()),
            ctx: OnceLock::new(),
            admin: tokio::sync::Mutex::new(()),
            next_slot: AtomicU32::new(1),
            revoked: Mutex::new(BTreeMap::new()),
            session_slots: Arc::new(tokio::sync::Semaphore::new(
                oxidepm_ipc::session::MAX_SESSIONS,
            )),
        })
    }

    /// Bind the daemon's request entry point: from now on an adapter's `host.request` is answered.
    pub fn bind_dispatch(&self, target: Arc<dyn Dispatch>) {
        self.link.bind(target);
    }

    /// The event hub the daemon's adapter sink is bound to.
    pub fn events(&self) -> &Arc<EventHub> {
        &self.events
    }

    /// What the sandbox can do on this machine, for `DaemonStatus`.
    pub fn sandbox_status(&self) -> oxidepm_ipc::SandboxStatus {
        let caps = &self.caps;
        oxidepm_ipc::SandboxStatus {
            landlock_abi: caps.landlock_abi,
            landlock_unix_connect: caps.landlock_unix_connect,
            seccomp: caps.seccomp,
            no_new_privs: caps.no_new_privs,
            ptrace_scope: caps.ptrace_scope,
            cgroup_delegated: caps.cgroup_delegated,
            launcher: self.cfg.launcher.is_some(),
            summary: caps.summary(),
        }
    }

    pub fn caps(&self) -> &oxidepm_sandbox::SandboxCaps {
        &self.caps
    }

    /// Read the adapter table and build the entries. Never fails: whatever cannot be used is a
    /// warning, and the daemon boots without it.
    pub async fn load(
        paths: &Arc<Paths>,
        audit: Option<Arc<Audit>>,
        cfg: HostConfig,
    ) -> Arc<AdapterHost> {
        let host = AdapterHost::empty(cfg);
        let _ = host.ctx.set(HostCtx {
            paths: Arc::clone(paths),
            audit: audit.clone(),
        });

        // `ensure_dirs` does not make the adapter directories, so a file in their place is found
        // here and is a warning, not a failed boot
        match std::fs::symlink_metadata(paths.adapters_dir()) {
            Ok(meta) if !meta.is_dir() => {
                host.warn(format!(
                    "{} is not a directory; adapters are unavailable until it is moved away",
                    paths.adapters_dir().display()
                ));
                return host;
            }
            Ok(_) | Err(_) => {}
        }

        // an import that was never finished leaves a staging directory behind
        super::install::clean_staging(paths);
        // a session cannot outlive the daemon: its socket is stale
        super::session::clean_sessions(paths, host.cfg.runtime_dir.as_deref());

        let store = match AdapterStore::open_existing(paths).await {
            Ok(Some(store)) => Arc::new(store),
            Ok(None) => return host,
            Err(e) => {
                host.warn(format!(
                    "{} cannot be read ({}); adapters are unavailable",
                    paths.adapters_db().display(),
                    e
                ));
                return host;
            }
        };
        let rows = match store.repo().list().await {
            Ok(rows) => rows,
            Err(e) => {
                host.warn(format!(
                    "the adapter table cannot be read ({e}); adapters are unavailable"
                ));
                store.close().await;
                return host;
            }
        };
        // directories that no row refers to are what a crash between two steps of an install or
        // an update left behind
        super::install::sweep_installed(paths, &rows);
        if rows.is_empty() {
            // nothing to run and nothing to keep open
            store.close().await;
            return host;
        }

        let pids = PidTable::new(paths.adapter_pids());
        let _ = host.pids.set(Arc::clone(&pids));
        let ctx = ValidateCtx::new(
            semver::Version::parse(env!("CARGO_PKG_VERSION"))
                .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
        );
        for (index, row) in rows.iter().enumerate() {
            let slot = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let (entry, problem) = host.build(row, paths, &pids, audit.clone(), &ctx, slot);
            if let Some((reason, note)) = problem {
                entry.mark_failed(reason, note);
            }
            host.watch_note(&entry, &store);
            host.entries.write().insert(row.name.clone(), entry);
        }
        host.next_slot.store(
            u32::try_from(rows.len() + 1).unwrap_or(u32::MAX),
            Ordering::SeqCst,
        );
        *host.store.lock() = Some(Arc::clone(&store));
        // an executable the cached index revokes must not run, whatever the row says
        if let Err(e) = host.apply_cached_revocations(&store).await {
            host.entries.write().clear();
            host.warn(format!(
                "adapters are unavailable because revocations cannot be checked: {e}"
            ));
        }
        host.write_verb_table();
        info!("Adapter host loaded {} adapter(s)", rows.len());
        host
    }

    pub(super) fn warn(&self, text: String) {
        warn!("{text}");
        self.warnings.lock().push(text);
    }

    /// The entry for one row, and what is wrong with the row if anything.
    pub(super) fn build(
        &self,
        row: &AdapterRow,
        paths: &Paths,
        pids: &Arc<PidTable>,
        audit: Option<Arc<Audit>>,
        ctx: &ValidateCtx,
        slot: u32,
    ) -> (Arc<AdapterEntry>, Option<(FailReason, String)>) {
        let kind = if row.kind == "service" {
            Kind::Service
        } else {
            Kind::Tool
        };
        let mut problem: Option<(FailReason, String)> = None;

        // the manifest is validated again at every load
        let manifest = match Manifest::parse_validated(row.manifest_json.as_bytes(), ctx) {
            Ok(m) => Some(m),
            Err(e) => {
                problem = Some((
                    FailReason::Internal,
                    format!("the stored manifest no longer validates: {e}"),
                ));
                None
            }
        };
        if let Some(m) = &manifest {
            if m.name != row.name || m.version != row.version || m.kind != kind {
                problem = Some((
                    FailReason::Tampered,
                    "the stored manifest does not match the adapter's record".to_string(),
                ));
            }
        }

        // what the user accepted, else (for a disabled adapter, which needs no grant) the manifest's
        let grant_permissions: Option<Permissions> = match &row.grant_json {
            Some(text) => match serde_json::from_str::<Permissions>(text) {
                Ok(p) => Some(p),
                Err(e) => {
                    if problem.is_none() {
                        problem = Some((
                            FailReason::Internal,
                            format!("the stored grant cannot be read: {e}"),
                        ));
                    }
                    None
                }
            },
            None => {
                if row.enabled && problem.is_none() {
                    problem = Some((
                        FailReason::Internal,
                        "the adapter is enabled but has no recorded grant".to_string(),
                    ));
                }
                None
            }
        };
        let permissions =
            grant_permissions.or_else(|| manifest.as_ref().map(|m| m.permissions.clone()));
        let (level, scope, env_pass) = match &permissions {
            Some(p) => (p.host.level, p.host.scope, p.env_pass.clone()),
            None => (
                oxidepm_adapter_proto::HostLevel::Read,
                oxidepm_adapter_proto::HostScope::OptedIn,
                Vec::new(),
            ),
        };

        let exec = match &manifest {
            Some(m) => paths.adapter_exec(&row.name, &row.version, &m.exec),
            None => paths.adapter_installed(&row.name, &row.version),
        };
        if problem.is_none() && std::fs::symlink_metadata(&exec).is_err() {
            problem = Some((
                FailReason::NeedsReinstall,
                format!("{} is missing; import the adapter again", exec.display()),
            ));
        }

        // the OS layer: the launcher with the profile of what was granted. Without the launcher an
        // adapter runs only if the user accepted that nothing is enforced.
        let allow_unenforced =
            EnforcedRecord::parse(row.enforced_json.as_deref()).is_some_and(|r| r.allow_unenforced);
        let state_dir = paths.adapter_state(&row.name);
        let sandbox = permissions.as_ref().and_then(|perms| match &self.cfg.launcher {
            Some(launcher) => {
                let search: Vec<PathBuf> = ADAPTER_PATH.iter().map(PathBuf::from).collect();
                let input = ProfileInput { state_dir: &state_dir, daemon_home: paths.home(), cwd: None, search_path: &search };
                Some(SandboxSetup { launcher: launcher.clone(), profile_json: Profile::from_permissions(perms, &input).to_json(), allow_unenforced })
            }
            None => {
                if row.enabled && problem.is_none() && !allow_unenforced {
                    problem = Some((
                        FailReason::Sandbox,
                        "the sandbox launcher oxidepm-sandbox is not installed next to oxidepmd; install it, or enable the adapter with --allow-unenforced".to_string(),
                    ));
                }
                None
            }
        });
        let memory_mb = permissions.as_ref().map(|p| p.memory_mb).unwrap_or(0);

        let spec = EntrySpec {
            name: row.name.clone(),
            version: row.version.clone(),
            kind,
            launch: Launch {
                name: row.name.clone(),
                version: row.version.clone(),
                exec,
                exe_sha256: row.exe_sha256.clone(),
                state_dir,
                daemon_home: paths.home().to_path_buf(),
                log_dir: paths.home().join("adapter-logs").join(&row.name),
                env_pass,
                connect_env_socket: permissions
                    .as_ref()
                    .map(|p| p.connect_env_socket.clone())
                    .unwrap_or_default(),
                env_source: Arc::clone(&self.cfg.env_source),
                sandbox,
                memory_mb,
            },
            grant: Grant { level, scope },
            slot,
            ai_driven: manifest.as_ref().is_some_and(|m| m.ai_driven),
            verbs: manifest
                .as_ref()
                .map(|m| m.provides.verbs.clone())
                .unwrap_or_default(),
            runtime: manifest
                .as_ref()
                .filter(|m| !m.provides.runtimes.is_empty())
                .map(|m| super::entry::RuntimeProvides {
                    runtimes: m.provides.runtimes.clone(),
                    rules: oxidepm_runtime::step::RuntimeRules::from_manifest(
                        m,
                        oxidepm_runtime::step::BUILD_TIMEOUT_MAX,
                    ),
                }),
            recipe: permissions.as_ref().map(|p| ProfileRecipe {
                permissions: p.clone(),
                daemon_home: paths.home().to_path_buf(),
                search_path: ADAPTER_PATH.iter().map(PathBuf::from).collect(),
            }),
        };
        // the host API answers under the grant the user accepted; the grant may raise the number of
        // `host.request`s in flight
        let mut entry_cfg = self.cfg.entry.clone();
        let requested = permissions.as_ref().and_then(|p| p.host.max_in_flight);
        entry_cfg.conn.limits.host_in_flight =
            host_in_flight_for(requested, &entry_cfg.conn.limits);
        let host_api = HostApi::new(
            &row.name,
            AdapterGrant { level, scope },
            Arc::clone(&self.link),
            audit.clone(),
        );
        let entry = AdapterEntry::new(
            spec,
            entry_cfg,
            Arc::clone(pids),
            host_api,
            audit,
            row.enabled,
        );
        // the families the user accepted; a disabled adapter listens to none
        let families: Vec<String> = match (&permissions, row.enabled) {
            (Some(p), true) => p.events.clone(),
            _ => Vec::new(),
        };
        entry.bind_events(&self.events, &families);
        (entry, problem)
    }

    /// Keep the row's `state_note` in step with a failure: the reason and what the host found.
    pub(super) fn watch_note(&self, entry: &Arc<AdapterEntry>, store: &Arc<AdapterStore>) {
        let mut rx = entry.subscribe();
        let weak: Weak<AdapterEntry> = Arc::downgrade(entry);
        let store = Arc::clone(store);
        let name = entry.name().to_string();
        tokio::spawn(async move {
            // a failure that was recorded before the watcher started
            let mut last: Option<String> = None;
            loop {
                let state = *rx.borrow_and_update();
                // a failure, or a disable with a reason (the state directory outgrew its quota)
                let disabled_with_reason = state == EntryState::Disabled;
                if matches!(state, EntryState::Failed(_)) || disabled_with_reason {
                    let Some(entry) = weak.upgrade() else { break };
                    // an entry that was replaced or removed no longer speaks for the row
                    if entry.is_detached() {
                        break;
                    }
                    let version = entry.version().to_string();
                    let view = entry.view();
                    let note = match (&view.state, &view.note) {
                        (EntryState::Failed(reason), Some(note)) => {
                            format!("{}: {}", reason.as_str(), note)
                        }
                        (EntryState::Failed(reason), None) => reason.as_str().to_string(),
                        (EntryState::Disabled, Some(note)) => format!("disabled: {note}"),
                        _ => String::new(),
                    };
                    if !note.is_empty() && last.as_deref() != Some(note.as_str()) {
                        // only the note (and, for a disable, the flag) is written: reading the row
                        // and writing all of it back could undo a change made in between
                        let written = if disabled_with_reason {
                            store.repo().disable_with_note(&name, &version, &note).await
                        } else {
                            store.repo().set_note(&name, &version, Some(&note)).await
                        };
                        if let Err(e) = written {
                            warn!("Could not record the state of adapter '{}': {}", name, e);
                        }
                        last = Some(note);
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    /// Start the enabled services in the background: at most four starting at once, with a pause
    /// between starts. Returns at once.
    pub fn start_services(self: &Arc<Self>) {
        let services: Vec<Arc<AdapterEntry>> = self
            .entries
            .read()
            .values()
            .filter(|e| e.kind() == Kind::Service && e.state() == EntryState::Stopped)
            .cloned()
            .collect();
        if services.is_empty() {
            return;
        }
        let host = Arc::clone(self);
        tokio::spawn(async move {
            for entry in services {
                // wait for room among the services still starting
                loop {
                    if host.closing.load(Ordering::SeqCst) {
                        return;
                    }
                    let starting = host
                        .entries
                        .read()
                        .values()
                        .filter(|e| e.state() == EntryState::Starting)
                        .count();
                    if starting < BOOT_PARALLEL {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                entry.start();
                tokio::time::sleep(host.cfg.boot_stagger).await;
            }
        });
    }

    /// The adapters as `DaemonStatus` reports them.
    pub fn status(&self) -> Vec<AdapterStatus> {
        self.entries
            .read()
            .values()
            .map(|entry| {
                let view = entry.view();
                let note = match (&view.state, view.note) {
                    (EntryState::Failed(reason), Some(note)) => {
                        Some(format!("{}: {}", reason.as_str(), note))
                    }
                    (EntryState::Failed(reason), None) => Some(reason.as_str().to_string()),
                    (_, note) => note,
                };
                AdapterStatus {
                    name: view.name,
                    state: view.state.name().to_string(),
                    note,
                }
            })
            .collect()
    }

    /// Sentences for the operator about what could not be loaded.
    pub fn warnings(&self) -> Vec<String> {
        let mut all = self.warnings.lock().clone();
        for (name, reason) in self.revoked.lock().iter() {
            all.push(format!(
                "adapter '{name}' was disabled because the index revokes its executable: {reason}"
            ));
        }
        for entry in self.entries.read().values() {
            let view = entry.view();
            if let (EntryState::Disabled, Some(note)) = (view.state, view.note) {
                all.push(format!("adapter '{}' was disabled: {}", view.name, note));
            }
        }
        all
    }

    /// Who the peer of a main-socket connection is, looked at once when the connection is
    /// accepted.
    ///
    /// `None` when no adapter process is alive (nothing is read from `/proc`, the common case),
    /// or when the peer is not in an adapter's process tree. The claim in the envelope then
    /// decides. `Some(Adapter)` when the peer is an adapter
    /// or one of its descendants. `Some(Unresolved)` when the peer could not be looked at while an
    /// adapter is alive (it was gone, `SO_PEERCRED` gave no pid, the chain was too long): stricter
    /// than a person, never a person.
    pub async fn label_peer(&self, pid: Option<i32>) -> Option<PeerLabel> {
        let adapters: Vec<AdapterProc> = self
            .pids
            .get()?
            .snapshot()
            .into_iter()
            .map(|r| AdapterProc {
                name: r.name,
                pid: r.pid,
                start_ticks: r.start_ticks,
            })
            .collect();
        if adapters.is_empty() {
            return None;
        }
        if !self.cfg.peer_walk_delay.is_zero() {
            tokio::time::sleep(self.cfg.peer_walk_delay).await;
        }
        let Some(pid) = pid.and_then(|p| u32::try_from(p).ok()) else {
            return Some(PeerLabel::Unresolved);
        };
        let reader = Arc::clone(&self.proc_reader);
        let found = tokio::task::spawn_blocking(move || {
            let read = |pid: u32| reader.read(pid);
            peer::walk(pid, &adapters, &read)
        })
        .await;
        match found {
            Ok(peer::Walk::Adapter(name)) => {
                // the adapter may have been removed since the pid table was read; then it is a
                // process we cannot place
                let grant = self.entry(&name).map(|e| e.grant());
                Some(match grant {
                    Some(grant) => PeerLabel::Adapter { name, grant },
                    None => PeerLabel::Unresolved,
                })
            }
            Ok(peer::Walk::Outside) => None,
            Ok(peer::Walk::Unresolved) | Err(_) => Some(PeerLabel::Unresolved),
        }
    }

    /// How many `/proc` files the attribution of peers has opened (a daemon with no running
    /// adapter has opened none).
    pub fn peer_proc_reads(&self) -> u64 {
        self.proc_reader.reads()
    }

    pub fn entry(&self, name: &str) -> Option<Arc<AdapterEntry>> {
        self.entries.read().get(name).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// services get `shutdown` so a notifier can deliver the messages the
    /// stop of the apps just produced; the grace is `min(5 s, budget_left)` for all of them
    /// together; then the daemon kills each adapter and its descendants. Tools are
    /// killed without grace. Bounded by the grace and the time the kills take.
    pub async fn shutdown(&self, budget_left: Duration) {
        self.closing.store(true, Ordering::SeqCst);
        let all: Vec<Arc<AdapterEntry>> = self.entries.read().values().cloned().collect();
        let (services, tools): (Vec<_>, Vec<_>) =
            all.into_iter().partition(|e| e.kind() == Kind::Service);

        let started = Instant::now();
        futures::future::join_all(tools.iter().map(|e| e.kill_now())).await;

        let grace = budget_left.min(SERVICE_GRACE);
        let polite = futures::future::join_all(services.iter().map(|e| e.stop(false)));
        if tokio::time::timeout(grace, polite).await.is_err() {
            warn!("Adapters did not stop within {:?}; killing them", grace);
            futures::future::join_all(services.iter().map(|e| e.kill_now())).await;
        }
        info!("Adapters stopped in {:?}", started.elapsed());

        let store = self.store.lock().take();
        if let Some(store) = store {
            store.close().await;
        }
    }

    /// The second signal of a shutdown: kill every adapter's process tree now, without waiting.
    pub fn kill_all_now(&self) {
        self.closing.store(true, Ordering::SeqCst);
        for entry in self.entries.read().values() {
            entry.kill_signal();
        }
    }
}
