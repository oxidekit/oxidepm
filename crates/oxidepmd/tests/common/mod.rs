//! Spec-test harness: drives the real `Supervisor` against real processes, in a private home.
//!
//! Every `Harness` owns a temporary directory that holds the OxidePM home (database, logs,
//! `saved.json`) and the working directory of every app it starts, so tests never touch the real
//! `~/.oxidepm`. Dropping the harness SIGKILLs any process whose working directory is under that
//! directory (leak guard for failing tests); it never signals anything else.
//!
//! Each test binary uses a subset of the helpers, hence the blanket `dead_code` allowance.
#![allow(dead_code)]

pub mod adapter_kit;
pub mod daemon;
pub mod fixture;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxidepm_core::{
    AppInfo, AppSpec, AppStatus, Error, Paths, Result, RunState, Runtime, Selector,
};
use oxidepm_db::Database;
use oxidepmd::supervisor::Supervisor;
use serde_json::json;
use tempfile::TempDir;

/// Default cap for every wait helper.
pub const WAIT_CAP: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct Harness {
    pub sup: Arc<Supervisor>,
    pub paths: Arc<Paths>,
    tmp: TempDir,
}

impl Harness {
    /// Fresh home, fresh database, fresh supervisor.
    pub async fn new() -> Self {
        let tmp = TempDir::new().expect("create temp dir");
        let paths = Arc::new(Paths::new(tmp.path().join("home")));
        let sup = Arc::new(Self::build_supervisor(&paths).await);
        Self { sup, paths, tmp }
    }

    async fn build_supervisor(paths: &Arc<Paths>) -> Supervisor {
        let db = Database::new(&paths.db())
            .await
            .expect("open test database");
        let sup = Supervisor::new(db, Arc::clone(paths))
            .await
            .expect("create supervisor");
        sup.load_registry().await.expect("load the registry");
        sup
    }

    /// Facade: stop every app (as a daemon shutdown does), save, then rebuild the supervisor on
    /// the same home. Children do not survive; `resurrect` brings them back.
    pub async fn restart_daemon(&mut self) {
        self.shutdown().await;
        self.start_daemon().await;
    }

    /// The second half of [`restart_daemon`](Self::restart_daemon): a new supervisor on the
    /// same home, with its registry loaded from the database (a test can change the stored
    /// state between `shutdown` and this).
    pub async fn start_daemon(&mut self) {
        self.sup = Arc::new(Self::build_supervisor(&self.paths).await);
    }

    /// The working directory shared by every app of this harness.
    pub fn dir(&self) -> &Path {
        self.tmp.path()
    }

    /// A path inside the harness directory.
    pub fn file(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    /// A duration literal unique to this test (`300.<pid><n>`): `exec sleep <marker>` gives a
    /// process that no other test, and nothing else on the machine, can be mistaken for.
    pub fn marker(&self) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("300.{:05}{:03}", std::process::id() % 100_000, n % 1000)
    }

    // ---- spec builders ------------------------------------------------------------------

    /// `sh -c <sh>` in the harness directory, mode `cmd`, default policies.
    pub fn cmd(&self, name: &str, sh: &str) -> AppSpec {
        let mut spec = AppSpec::new(
            name.to_string(),
            Runtime::CMD,
            "sh".to_string(),
            self.tmp.path().to_path_buf(),
        );
        spec.args = vec!["-c".to_string(), sh.to_string()];
        spec
    }

    /// Like [`cmd`](Self::cmd), with a JSON object recursively merged into the spec.
    pub fn cmd_with(&self, name: &str, sh: &str, overlay: serde_json::Value) -> AppSpec {
        let mut value = serde_json::to_value(self.cmd(name, sh)).expect("spec to json");
        merge(&mut value, overlay);
        serde_json::from_value(value).expect("spec from json")
    }

    /// Write an executable shell script into the harness directory.
    pub fn script(&self, file: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.tmp.path().join(file);
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    // ---- facade over the supervisor ------------------------------------------------------

    pub async fn start(&self, spec: AppSpec) -> Result<u32> {
        self.sup.start(spec).await
    }

    pub async fn stop(&self, sel: &str) -> Result<usize> {
        let ids = self.sup.resolve_selector(&Selector::parse(sel)).await?;
        let mut count = 0;
        for (_, stopped) in self.sup.stop_many(&ids).await {
            if stopped? {
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn restart(&self, sel: &str) -> Result<usize> {
        let mut count = 0;
        for id in self.sup.resolve_selector(&Selector::parse(sel)).await? {
            if self.sup.restart(id).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn reload(&self, sel: &str) -> Result<usize> {
        let mut count = 0;
        for id in self.sup.resolve_selector(&Selector::parse(sel)).await? {
            if self.sup.reload(id).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn delete(&self, sel: &str) -> Result<usize> {
        let mut count = 0;
        for id in self.sup.resolve_selector(&Selector::parse(sel)).await? {
            if self.sup.delete(id).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Flush the logs of every app the selector matches: one result per app, its name and
    /// whether the flush worked (an error is that app's failure, not the call's).
    pub async fn flush(&self, sel: &str) -> Vec<(String, Result<bool>)> {
        let mut results = Vec::new();
        for id in self
            .sup
            .resolve_selector(&Selector::parse(sel))
            .await
            .expect("resolve")
        {
            let name = self
                .sup
                .show(&Selector::ById(id))
                .await
                .expect("show")
                .map(|info| info.spec.name)
                .unwrap_or_else(|| id.to_string());
            results.push((name, self.sup.flush(id).await));
        }
        results
    }

    pub async fn status(&self) -> Vec<AppInfo> {
        self.sup.status().await.expect("status")
    }

    /// Run state of the app called `name`; panics if there is no such app.
    pub async fn state(&self, name: &str) -> RunState {
        self.sup
            .show(&Selector::ByName(name.to_string()))
            .await
            .expect("show")
            .unwrap_or_else(|| panic!("no app named {name}"))
            .state
    }

    pub async fn save(&self, force: bool) -> Result<usize> {
        self.sup.save(force).await
    }

    pub async fn resurrect(&self) -> Result<usize> {
        self.sup.resurrect().await
    }

    /// What a daemon does on `Kill`: close, stop everything within a budget, then write the
    /// saved file from the registry.
    pub async fn shutdown(&self) {
        self.sup.shutdown(Duration::from_secs(60)).await;
        self.sup.save(true).await.expect("write saved.json");
    }

    /// An unhealthy app has its own status and can still have a live child.
    pub fn is_unhealthy(&self, s: &RunState) -> bool {
        s.status == AppStatus::Unhealthy
    }

    // ---- waiting and probing -------------------------------------------------------------

    /// Wait until an app called `name` is registered (a start running in another task has
    /// reserved it).
    pub async fn wait_registered(&self, name: &str) {
        let name = name.to_string();
        let deadline = Instant::now() + WAIT_CAP;
        loop {
            let shown = self
                .sup
                .show(&Selector::ByName(name.clone()))
                .await
                .expect("show");
            if shown.is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {name} to be registered"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    pub async fn wait_status(&self, name: &str, want: AppStatus) -> RunState {
        self.wait_until(name, &format!("status {want:?}"), |s| s.status == want)
            .await
    }

    pub async fn wait_until<F: Fn(&RunState) -> bool>(
        &self,
        name: &str,
        what: &str,
        f: F,
    ) -> RunState {
        self.wait_until_for(name, what, WAIT_CAP, f).await
    }

    /// `wait_until` with an explicit cap, for conditions that legitimately take longer.
    pub async fn wait_until_for<F: Fn(&RunState) -> bool>(
        &self,
        name: &str,
        what: &str,
        cap: Duration,
        f: F,
    ) -> RunState {
        let deadline = Instant::now() + cap;
        loop {
            let state = self.state(name).await;
            if f(&state) {
                return state;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {cap:?} waiting for {what} on {name}; last state: {state:?}"
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait until `f` holds; panic with `what` after `cap`.
    pub async fn eventually<F: FnMut() -> bool>(&self, what: &str, cap: Duration, mut f: F) {
        let deadline = Instant::now() + cap;
        loop {
            if f() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out after {cap:?} waiting for {what}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait for `path` to exist and return its content.
    pub async fn wait_file(&self, path: &Path) -> String {
        let deadline = Instant::now() + WAIT_CAP;
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                return content;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for file {}", path.display());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait until the log of `name`/`stream` contains `needle`; returns the whole log.
    pub async fn wait_log(&self, name: &str, stream: &str, needle: &str) -> String {
        let deadline = Instant::now() + WAIT_CAP;
        loop {
            let log = self.read_log(name, stream);
            if log.contains(needle) {
                return log;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?} in {name} {stream} log; log so far: {log:?}"
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Number of non-empty lines in `path` (0 if it does not exist). Tests use append-only
    /// "launch" files, one line per launch.
    pub fn line_count(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|c| c.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0)
    }

    /// `kill(pid, 0)` succeeds and `/proc/<pid>/stat` does not say zombie.
    pub fn pid_alive(pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        if kill(Pid::from_raw(pid as i32), None).is_err() {
            return false;
        }
        match stat_fields(pid) {
            Some(fields) => fields.first().map(|s| s != "Z").unwrap_or(false),
            None => false,
        }
    }

    /// Process group of `pid` (field 5 of `/proc/<pid>/stat`).
    pub fn pgid_of(pid: u32) -> Option<u32> {
        stat_fields(pid)?.get(2)?.parse().ok()
    }

    /// Live (non-zombie) processes whose command line, arguments joined by spaces, contains
    /// `needle`. Tests use unique markers such as `sleep 300.12345007`.
    pub fn procs_with_cmdline(needle: &str) -> Vec<u32> {
        let me = std::process::id();
        let mut found = Vec::new();
        for pid in proc_pids() {
            if pid == me {
                continue;
            }
            let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let cmdline: String = String::from_utf8_lossy(&raw)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if cmdline.contains(needle) && Self::pid_alive(pid) {
                found.push(pid);
            }
        }
        found.sort_unstable();
        found
    }

    pub fn read_log(&self, name: &str, stream: &str) -> String {
        std::fs::read(self.paths.log_path(name, stream))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    /// Pid of the app's live child, panicking if it has none.
    pub async fn pid_of(&self, name: &str) -> u32 {
        self.state(name)
            .await
            .pid
            .unwrap_or_else(|| panic!("{name} has no pid"))
    }
}

impl Drop for Harness {
    /// Leak guard: SIGKILL every process whose working directory is under the harness directory.
    fn drop(&mut self) {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let root = match self.tmp.path().canonicalize() {
            Ok(p) => p,
            Err(_) => return,
        };
        let me = std::process::id();
        for pid in proc_pids() {
            if pid == me {
                continue;
            }
            if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                if cwd.starts_with(&root) {
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                }
            }
        }
    }
}

/// Merge `overlay` into `base` (objects recursively, everything else replaced).
fn merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn proc_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// `/proc/<pid>/stat` fields after the command name: `[state, ppid, pgrp, ...]`.
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may contain spaces or parentheses: split after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// The text of a saved file (version 1) that lists `specs`.
pub fn saved_json(specs: &[AppSpec]) -> String {
    serde_json::to_string_pretty(
        &json!({ "version": 1, "saved_at": "2026-01-01T00:00:00Z", "apps": specs }),
    )
    .expect("saved file to json")
}

/// The app entries of the text of a saved file; panics if it is not a version 1 file.
pub fn saved_entries(text: &str) -> Vec<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(text).expect("saved file is json");
    assert_eq!(value["version"], 1, "{value}");
    value["apps"].as_array().expect("apps list").clone()
}

/// A script probe that always fails, checked every two seconds, one failure is enough (the
/// timeout may not exceed the interval).
pub fn failing_probe() -> serde_json::Value {
    json!({ "health_check": {
        "script": "exit 1",
        "expected_status": [200],
        "interval_secs": 2,
        "timeout_secs": 1,
        "retries": 1
    } })
}

/// True when the tests run as root (permission-based tests cannot fail there and skip).
pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|euid| euid == "0"))
        })
        .unwrap_or(false)
}

/// Skip a test with a printed reason when the machine cannot run it.
pub fn skip(reason: &str) {
    eprintln!("SKIPPED: {reason}");
}

/// Convenience for asserting on `Error` variants without importing them everywhere.
pub fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::AppNotFound(_))
}

pub fn is_already_exists(e: &Error) -> bool {
    matches!(e, Error::AppAlreadyExists(_))
}

pub fn is_shutting_down(e: &Error) -> bool {
    matches!(e, Error::ShuttingDown)
}
