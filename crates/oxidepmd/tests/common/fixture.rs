//! Runs the fixture adapter (`oxidepm-fixture-adapter`) for the host tests.
//!
//! The binary belongs to another package, so Cargo does not hand its path to these tests; the
//! first call builds it (a no-op when it is current) into the same target directory. Every
//! fixture runs as the leader of its own process group, with a cleared environment, in a temporary
//! directory; dropping a `Fixture` kills the group, so nothing outlives a failing test.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use oxidepm_adapter_proto::messages::Grant;
use oxidepm_adapter_proto::{HostLevel, HostScope, Kind};
use oxidepm_runtime::{group_command, GroupChild};
use oxidepmd::adapters::conn::{AdapterConn, ConnConfig, Inbound, NoHostApi};
use oxidepmd::adapters::entry::{AdapterEntry, EntryConfig, EntrySpec};
use oxidepmd::adapters::proc::{Launch, ShutdownTimings};
use oxidepmd::supervisor::PidTable;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::process::{ChildStdin, ChildStdout};

/// The path of the fixture adapter binary, built if it is missing or stale.
pub fn fixture_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        // Cargo supplies the final daemon artifact path, alongside the fixture and launcher.
        // The test harness itself can live in a separate build directory.
        let profile_dir = Path::new(env!("CARGO_BIN_EXE_oxidepmd"))
            .parent()
            .expect("daemon binary directory");
        let target_dir = profile_dir.parent().expect("target directory");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        // the sandbox launcher is built with it: the daemon looks for it next to its own binary
        cmd.args([
            "build",
            "-p",
            "oxidepm-adapter-fixture",
            "-p",
            "oxidepm-sandbox",
            "--offline",
            "--quiet",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_TARGET_DIR", target_dir);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            cmd.arg("--release");
        }
        let output = cmd
            .output()
            .expect("run cargo to build the fixture adapter");
        assert!(
            output.status.success(),
            "building the fixture adapter failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bin = profile_dir.join("oxidepm-fixture-adapter");
        assert!(
            bin.is_file(),
            "the fixture adapter was not built at {}",
            bin.display()
        );
        bin
    })
}

/// A private `0700` per-run directory holding staged copies of the daemon, CLI, and sandbox
/// launcher so tests do not depend on the permissions of the Cargo target directory.
fn staged_bin_dir() -> &'static Path {
    use std::os::unix::fs::PermissionsExt;
    static DIR: OnceLock<TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = TempDir::new().expect("create private test binary directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod private test binary directory");
        dir
    })
    .path()
}

/// Copy `src` into the private `0700` per-run directory as `name` with mode `0700`.
pub fn stage_binary(src: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = staged_bin_dir();
    let dst = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    std::fs::copy(src, &tmp)
        .unwrap_or_else(|e| panic!("copy {} to {}: {e}", src.display(), tmp.display()));
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", tmp.display()));
    std::fs::rename(&tmp, &dst)
        .unwrap_or_else(|e| panic!("rename {} to {}: {e}", tmp.display(), dst.display()));
    dst
}

/// The path of the sandbox launcher `oxidepm-sandbox`, built together with the fixture and staged
/// into a private `0700` directory so the launcher check passes regardless of target directory
/// permissions.
pub fn launcher_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let fixture = fixture_bin();
        let launcher = fixture
            .parent()
            .expect("profile directory")
            .join("oxidepm-sandbox");
        assert!(
            launcher.is_file(),
            "the sandbox launcher was not built at {}",
            launcher.display()
        );
        stage_binary(&launcher, "oxidepm-sandbox")
    })
    .clone()
}

/// A running fixture adapter, before or after it is handed to a connection.
pub struct Fixture {
    child: GroupChild,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    pub dir: TempDir,
    pub report_path: PathBuf,
}

impl Fixture {
    /// Start the fixture with `script` (a JSON array of rules).
    pub fn spawn(script: Value) -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).expect("write the script");
        let report_path = dir.path().join("report.jsonl");

        let mut cmd = group_command(fixture_bin());
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("OXIDEPM_ADAPTER_PROTOCOLS", "1")
            .env("FIXTURE_SCRIPT", &script_path)
            .env("FIXTURE_REPORT", &report_path)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = GroupChild::spawn(&mut cmd).expect("spawn the fixture adapter");
        let stdin = child.stdin.take();
        let stdout = child.take_stdout();
        Fixture {
            child,
            stdin,
            stdout,
            dir,
            report_path,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.pid()
    }

    /// Hand the pipes to a connection with the given configuration; nobody answers `host.request`.
    pub fn connect(&mut self, cfg: ConnConfig) -> Arc<AdapterConn> {
        self.connect_with(cfg, Arc::new(NoHostApi))
    }

    pub fn connect_with(&mut self, cfg: ConnConfig, inbound: Arc<dyn Inbound>) -> Arc<AdapterConn> {
        let stdout = self.stdout.take().expect("pipes already taken");
        let stdin = self.stdin.take().expect("pipes already taken");
        AdapterConn::start(stdout, stdin, cfg, inbound)
    }

    /// Everything the fixture has recorded so far.
    pub fn report(&self) -> Vec<Value> {
        oxidepm_adapter_fixture::report::read_report(&self.report_path)
    }

    /// Wait (bounded) for a report record of kind `what`.
    pub async fn wait_report(&self, what: &str) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(found) = self.report().into_iter().find(|r| r["what"] == what) {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The `initialize` params the daemon sends.
pub fn initialize_params(name: &str, version: &str, kind: &str) -> Value {
    json!({
        "protocols": [1],
        "host": {"name": "oxidepmd", "version": "0.0.0"},
        "adapter": {"name": name, "version": version},
        "kind": kind,
        "grant": {"level": "read", "scope": "all"},
        "paths": {"state": "/nonexistent/state"},
        "limits": {"max_line": 262144, "max_in_flight": 32, "event_queue": 1024}
    })
}

/// Poll `f` until it is true or `secs` seconds pass.
pub async fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Is a process with this pid alive (a zombie does not count)?
pub fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rfind(')')
            .and_then(|i| {
                stat[i + 1..]
                    .split_whitespace()
                    .next()
                    .map(|s| s != "Z" && s != "X")
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// The sha256 of the fixture binary, as hex.
pub fn fixture_sha256() -> &'static str {
    use sha2::{Digest, Sha256};
    static SHA: OnceLock<String> = OnceLock::new();
    SHA.get_or_init(|| {
        let bytes = std::fs::read(fixture_bin()).expect("read the fixture binary");
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

/// Shorten supervision timings for tests.
pub fn quick_config() -> EntryConfig {
    let mut cfg = EntryConfig::production();
    cfg.shutdown = ShutdownTimings {
        stdin_phase: Duration::from_millis(400),
        term_phase: Duration::from_millis(400),
    };
    cfg.ready_wait = Duration::from_secs(8);
    cfg.init_timeout = Duration::from_secs(5);
    cfg
}

/// An adapter entry over the fixture, in a private directory, with its pid table.
pub struct Rig {
    pub entry: Arc<AdapterEntry>,
    pub dir: TempDir,
    pub pids: Arc<PidTable>,
    pub pids_path: PathBuf,
    pub report_path: PathBuf,
    pub script_path: PathBuf,
    /// The directory standing in for the daemon home (sandboxed rigs).
    pub home: PathBuf,
}

/// The permissions the sandboxed rigs start from; `perms` replaces keys of it, and `$IO` in the
/// replacements is the rig's directory (the script and the report live there).
pub fn rig_permissions(perms: &Value, io: &Path) -> oxidepm_adapter_proto::Permissions {
    let io_text = io.display().to_string();
    let mut permissions = json!({
        "host": {"level": "read", "scope": "all"},
        "events": [],
        "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
        "network": {"mode": "none"},
        "fs": {"read": ["$STATE", io_text], "write": ["$STATE", io_text]},
        "spawn": {"mode": "none"},
        "memory_mb": 64,
        "requires_enforcement": []
    });
    if let Value::Object(changes) = perms {
        for (key, value) in changes {
            let text = value.to_string().replace("$IO", &io_text);
            permissions[key.as_str()] = serde_json::from_str(&text).expect("perms stay JSON");
        }
    }
    serde_json::from_value(permissions).expect("the permissions parse")
}

impl Rig {
    /// An adapter started directly without the sandbox launcher.
    pub fn new(name: &str, kind: Kind, script: Value, tweak: impl FnOnce(&mut EntryConfig)) -> Rig {
        Rig::build(name, kind, script, None, None, tweak)
    }

    /// An adapter started through the sandbox launcher with `perms` (see [`rig_permissions`]).
    pub fn sandboxed(
        name: &str,
        kind: Kind,
        script: Value,
        perms: Value,
        tweak: impl FnOnce(&mut EntryConfig),
    ) -> Rig {
        Rig::build(name, kind, script, Some(perms), None, tweak)
    }

    /// An adapter started through the launcher with this text as the profile argument, whatever it is.
    pub fn with_raw_profile(name: &str, kind: Kind, script: Value, profile_json: &str) -> Rig {
        Rig::build(
            name,
            kind,
            script,
            Some(json!({})),
            Some(profile_json.to_string()),
            |_| {},
        )
    }

    fn build(
        name: &str,
        kind: Kind,
        script: Value,
        perms: Option<Value>,
        profile_override: Option<String>,
        tweak: impl FnOnce(&mut EntryConfig),
    ) -> Rig {
        let dir = TempDir::new().expect("temp dir");
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).expect("write the script");
        let report_path = dir.path().join("report.jsonl");
        let pids_path = dir.path().join("adapter-pids.json");
        let pids = PidTable::new(pids_path.clone());
        let mut cfg = quick_config();
        tweak(&mut cfg);

        let home = dir.path().join("home");
        let state_dir = if perms.is_some() {
            home.join("adapters").join("state").join(name)
        } else {
            dir.path().join("state").join(name)
        };
        let (sandbox, memory_mb) = match &perms {
            Some(perms) => {
                // the rig's own files live in a directory the adapter may use, outside the home
                let io = dir.path().join("io");
                std::fs::create_dir_all(&io).expect("io dir");
                std::fs::create_dir_all(&home).expect("home dir");
                let permissions = rig_permissions(perms, &io);
                let search = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
                let profile = oxidepm_sandbox::Profile::from_permissions(
                    &permissions,
                    &oxidepm_sandbox::ProfileInput {
                        state_dir: &state_dir,
                        daemon_home: &home,
                        cwd: None,
                        search_path: &search,
                    },
                );
                let setup = oxidepmd::adapters::proc::SandboxSetup {
                    launcher: launcher_bin(),
                    profile_json: profile_override.unwrap_or_else(|| profile.to_json()),
                    allow_unenforced: false,
                };
                (Some(setup), permissions.memory_mb)
            }
            None => (None, 0),
        };
        // with a sandbox the script and the report must be where the profile lets the adapter in
        let (script_path, report_path) = if perms.is_some() {
            let io = dir.path().join("io");
            let moved = (io.join("script.json"), io.join("report.jsonl"));
            std::fs::rename(&script_path, &moved.0).expect("move the script");
            moved
        } else {
            (script_path, report_path)
        };

        let (script_value, report_value) = (
            script_path.display().to_string(),
            report_path.display().to_string(),
        );
        let spec = EntrySpec {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            kind,
            launch: Launch {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                exec: fixture_bin().to_path_buf(),
                exe_sha256: fixture_sha256().to_string(),
                state_dir,
                daemon_home: home.clone(),
                log_dir: dir.path().join("adapter-logs").join(name),
                env_pass: vec!["FIXTURE_SCRIPT".to_string(), "FIXTURE_REPORT".to_string()],
                connect_env_socket: Vec::new(),
                env_source: Arc::new(move |key: &str| match key {
                    "FIXTURE_SCRIPT" => Some(script_value.clone()),
                    "FIXTURE_REPORT" => Some(report_value.clone()),
                    _ => None,
                }),
                sandbox,
                memory_mb,
            },
            grant: Grant {
                level: HostLevel::Read,
                scope: HostScope::All,
            },
            slot: 1,
            verbs: Vec::new(),
            runtime: None,
            ai_driven: false,
            recipe: None,
        };
        let entry = AdapterEntry::new(
            spec,
            cfg,
            Arc::clone(&pids),
            Arc::new(NoHostApi),
            None,
            true,
        );
        Rig {
            entry,
            dir,
            pids,
            pids_path,
            report_path,
            script_path,
            home,
        }
    }

    /// The script the next process of this adapter will run.
    pub fn set_script(&self, script: Value) {
        std::fs::write(&self.script_path, script.to_string()).expect("write the script");
    }

    pub fn report(&self) -> Vec<Value> {
        oxidepm_adapter_fixture::report::read_report(&self.report_path)
    }

    /// Wait (bounded) until the fixture has recorded something of kind `what`.
    pub async fn wait_for_report(&self, what: &str) -> bool {
        wait_until(15, || self.report().iter().any(|r| r["what"] == what)).await
    }

    /// How many processes of the fixture have started.
    pub fn starts(&self) -> usize {
        self.report()
            .iter()
            .filter(|r| r["what"] == "start")
            .count()
    }

    /// The pids of the processes that started, in order.
    pub fn start_pids(&self) -> Vec<u32> {
        self.report()
            .iter()
            .filter(|r| r["what"] == "start")
            .filter_map(|r| r["pid"].as_u64())
            .map(|p| p as u32)
            .collect()
    }
}

/// Wait until the entry reaches `want`.
pub async fn wait_state(
    entry: &Arc<AdapterEntry>,
    secs: u64,
    want: impl Fn(&oxidepmd::adapters::entry::EntryState) -> bool,
) -> bool {
    wait_until(secs, || want(&entry.state())).await
}

/// An adapter installed into a home: the files on disk and the row in `adapters.db`, standing in
/// for what `oxidepm adapter import` will do. The daemon is not running while this is
/// made, and the daemon is given `env` so the adapter finds its script and report.
pub struct Installed {
    pub script_path: PathBuf,
    pub report_path: PathBuf,
}

impl Installed {
    /// The environment to give the daemon (the manifest passes these two names on).
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "FIXTURE_SCRIPT".to_string(),
                self.script_path.display().to_string(),
            ),
            (
                "FIXTURE_REPORT".to_string(),
                self.report_path.display().to_string(),
            ),
        ]
    }

    pub fn report(&self) -> Vec<Value> {
        oxidepm_adapter_fixture::report::read_report(&self.report_path)
    }

    /// The pids of the adapter processes that started.
    pub fn start_pids(&self) -> Vec<u32> {
        self.report()
            .iter()
            .filter(|r| r["what"] == "start")
            .filter_map(|r| r["pid"].as_u64())
            .map(|p| p as u32)
            .collect()
    }

    pub async fn wait_report(&self, what: &str) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(found) = self.report().into_iter().find(|r| r["what"] == what) {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// What to install.
pub struct InstallSpec {
    pub name: &'static str,
    pub kind: Kind,
    pub enabled: bool,
    pub script: Value,
    /// Record this digest instead of the executable's (an executable that changed since import).
    pub wrong_digest: bool,
    /// Do not put the executable on disk (a row whose files are gone).
    pub no_files: bool,
    /// Replacements for keys of the permissions (`fs`, `spawn`, `network`, `memory_mb`,
    /// `requires_enforcement`). The text `$IO` in them is the directory that holds the script and
    /// the report of the fixture.
    pub perms: Option<Value>,
    /// The `enforced_json` that `enable` would record.
    pub enforced: Option<Value>,
    /// The manifest's `provides` (a runtime adapter lists `runtimes`, `programs`, ...).
    pub provides: Value,
}

impl InstallSpec {
    pub fn service(name: &'static str, script: Value) -> Self {
        Self {
            name,
            kind: Kind::Service,
            enabled: true,
            script,
            wrong_digest: false,
            no_files: false,
            perms: None,
            enforced: None,
            provides: json!({}),
        }
    }

    pub fn tool(name: &'static str, script: Value) -> Self {
        Self {
            name,
            kind: Kind::Tool,
            enabled: true,
            script,
            wrong_digest: false,
            no_files: false,
            perms: None,
            enforced: None,
            provides: json!({}),
        }
    }
}

/// Install the fixture adapter as `spec` says into the OxidePM home `home`.
pub async fn install(home: &Path, spec: InstallSpec) -> Installed {
    use oxidepm_core::Paths;
    use oxidepm_db::AdapterRow;
    use oxidepmd::adapters::store::AdapterStore;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(home).expect("create the home");
    let paths = Paths::new(home.to_path_buf());
    let store = AdapterStore::open_or_create(&paths)
        .await
        .expect("open adapters.db");

    let exec_name = "oxidepm-fixture-adapter";
    let version = "1.0.0";
    if !spec.no_files {
        let exec = paths.adapter_exec(spec.name, version, exec_name);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode_700()
            .create(exec.parent().unwrap())
            .expect("create the install directory");
        std::fs::copy(fixture_bin(), &exec).expect("copy the fixture adapter");
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o500))
            .expect("chmod the executable");
    }

    let kind_text = if spec.kind == Kind::Service {
        "service"
    } else {
        "tool"
    };
    // the script and the report live outside the daemon home, in a directory the sandbox opens
    let io_dir = home
        .parent()
        .expect("a home has a parent")
        .join("fixture-io");
    std::fs::create_dir_all(&io_dir).expect("create the fixture io directory");
    let io_text = io_dir.display().to_string();
    let mut permissions = json!({
        "host": {"level": "read", "scope": "all"},
        "events": [],
        "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
        "network": {"mode": "none"},
        "fs": {"read": ["$STATE", io_text], "write": ["$STATE", io_text]},
        "spawn": {"mode": "any"},
        "memory_mb": 64,
        "requires_enforcement": []
    });
    if let Some(Value::Object(changes)) = &spec.perms {
        for (key, value) in changes {
            let text = value.to_string().replace("$IO", &io_text);
            permissions[key.as_str()] = serde_json::from_str(&text).expect("perms stay JSON");
        }
    }
    let manifest = json!({
        "schema": 1,
        "name": spec.name,
        "version": version,
        "kind": kind_text,
        "summary": "The fixture adapter.",
        "protocol": [1],
        "min_daemon": "0.1.0",
        "exec": exec_name,
        "provides": spec.provides,
        "permissions": permissions
    });
    let digest = if spec.wrong_digest {
        "0".repeat(64)
    } else {
        fixture_sha256().to_string()
    };
    let row = AdapterRow {
        name: spec.name.to_string(),
        kind: kind_text.to_string(),
        version: version.to_string(),
        exe_sha256: digest,
        manifest_json: manifest.to_string(),
        source: "bundled".to_string(),
        trust: "pinned".to_string(),
        enabled: spec.enabled,
        grant_json: spec.enabled.then(|| permissions.to_string()),
        grant_fingerprint: None,
        enforced_json: spec.enforced.as_ref().map(|v| v.to_string()),
        prev_version: None,
        prev_exe_sha256: None,
        prev_manifest_json: None,
        prev_grant_json: None,
        state_note: None,
        installed_at: "2026-11-01T10:00:00Z".to_string(),
        updated_at: "2026-11-01T10:00:00Z".to_string(),
    };
    store
        .repo()
        .insert(&row)
        .await
        .expect("insert the adapter row");
    store.close().await;

    let script_path = io_dir.join(format!("{}-script.json", spec.name));
    std::fs::write(&script_path, spec.script.to_string()).expect("write the script");
    Installed {
        script_path,
        report_path: io_dir.join(format!("{}-report.jsonl", spec.name)),
    }
}

trait DirBuilderMode {
    fn mode_700(&mut self) -> &mut Self;
}

impl DirBuilderMode for std::fs::DirBuilder {
    fn mode_700(&mut self) -> &mut Self {
        use std::os::unix::fs::DirBuilderExt;
        self.mode(0o700)
    }
}

/// The adapters `DaemonStatus` lists: `(name, state, note)`.
pub fn adapter_states(daemon: &super::daemon::DaemonProc) -> Vec<(String, String, Option<String>)> {
    let reply = daemon.rpc(json!({ "type": "daemon_status" }));
    assert_eq!(reply["type"], "daemon_status", "{reply}");
    reply["adapters"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap_or("").to_string(),
                a["state"].as_str().unwrap_or("").to_string(),
                a["note"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// Wait (bounded) until the adapter `name` is in state `state` in `DaemonStatus`.
pub async fn wait_adapter_state(
    daemon: &super::daemon::DaemonProc,
    name: &str,
    state: &str,
    secs: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if adapter_states(daemon)
            .iter()
            .any(|(n, s, _)| n == name && s == state)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Make `cmd`'s child inherit each `(from, to)` descriptor as number `to`, without close-on-exec.
/// The numbers of the test process's own files depend on what else it has open (a `from` that
/// already equals its `to` would keep its close-on-exec flag), so each is first copied above 100.
/// The copies stay open in the child too.
pub fn inherit_descriptors(
    cmd: &mut std::process::Command,
    fds: Vec<(std::os::fd::RawFd, std::os::fd::RawFd)>,
) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure calls only fcntl and dup2 between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let mut copies = Vec::new();
            for (from, _) in &fds {
                let copy = libc::fcntl(*from, libc::F_DUPFD, 100);
                if copy < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                copies.push(copy);
            }
            for (copy, (_, to)) in copies.iter().zip(&fds) {
                if libc::dup2(*copy, *to) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}
