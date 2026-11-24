//! Lifecycle hooks: the one place that runs an `on_start`, `on_stop`, `on_restart`, `on_crash`
//! or `on_error` script.
//!
//! A hook is `sh -c <script>` in its own process group (through `oxidepm_runtime::run_script`),
//! with stdout and stderr appended to `{logs}/{name}-hooks.log`. When it runs longer than the
//! timeout the whole group is killed, so a hung `sleep 1000 &` does not leak. A hook never
//! affects the state of its app: the result is logged and that is all.

use nix::sys::signal::Signal;
use oxidepm_core::{AppSpec, HookEvent, Paths};
use oxidepm_runtime::{run_script_with, script_env, ChildEnv, ScriptOutcome, ScriptSpec};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

/// How long a hook may run before its process group is killed.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a shutdown waits for running hooks before it kills their groups.
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Test knob: the hook timeout in milliseconds.
pub const HOOK_TIMEOUT_ENV: &str = "OXIDEPM_HOOK_TIMEOUT_MS";

/// Test knob: the shutdown drain grace in milliseconds.
pub const HOOK_DRAIN_ENV: &str = "OXIDEPM_HOOK_DRAIN_MS";

/// What a hook is told about the event (`OPM_PID`, `OPM_EXIT_CODE`, `OPM_REASON`,
/// `OPM_RESTARTS`).
#[derive(Debug, Clone, Default)]
pub struct HookCtx {
    pub app_id: u32,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub restarts: u32,
}

/// Runs hooks and keeps track of the ones that are still running.
pub struct HookRunner {
    timeout: Duration,
    drain_grace: Duration,
    paths: Arc<Paths>,
    /// Number of hook tasks that have not finished.
    running: watch::Sender<usize>,
    /// Process groups of the hook scripts that are running now.
    live_groups: Mutex<HashSet<i32>>,
}

/// Counts a hook task as running for as long as it lives.
struct Running(Arc<HookRunner>);

impl Running {
    fn new(runner: &Arc<HookRunner>) -> Self {
        runner.running.send_modify(|n| *n += 1);
        Self(Arc::clone(runner))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.send_modify(|n| *n -= 1);
    }
}

fn millis_from_env(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
}

impl HookRunner {
    /// A runner with the default timeouts (or the ones the test knobs name).
    pub fn new(paths: Arc<Paths>) -> Arc<Self> {
        Self::with_timeouts(
            paths,
            millis_from_env(HOOK_TIMEOUT_ENV).unwrap_or(DEFAULT_HOOK_TIMEOUT),
            millis_from_env(HOOK_DRAIN_ENV).unwrap_or(DEFAULT_DRAIN_GRACE),
        )
    }

    pub fn with_timeouts(paths: Arc<Paths>, timeout: Duration, drain_grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            timeout,
            drain_grace,
            paths,
            running: watch::channel(0).0,
            live_groups: Mutex::new(HashSet::new()),
        })
    }

    /// How long `Supervisor::drain_hooks` waits.
    pub fn drain_grace(&self) -> Duration {
        self.drain_grace
    }

    /// The process groups of the hooks that are running now.
    pub fn live_groups(&self) -> Vec<i32> {
        self.live_groups.lock().iter().copied().collect()
    }

    /// Run the app's hook for `event`, if it has one, in the background. Never blocks and
    /// never fails: the outcome goes to the log.
    pub fn fire(self: &Arc<Self>, spec: &AppSpec, event: HookEvent, ctx: HookCtx) {
        let Some(script) = spec.hooks.get(event) else {
            return;
        };
        let script = script.to_string();
        let name = spec.name.clone();
        let event_name = event.to_string();
        let log_limit = spec
            .log_max_size
            .unwrap_or(oxidepm_core::constants::DEFAULT_LOG_MAX_SIZE);
        // base set + the app's env + OPM_*; never the daemon's own environment
        let env = script_env(spec, &hook_vars(&name, &event_name, &ctx));
        info!(
            "Running {} hook for {} (id: {})",
            event_name, name, ctx.app_id
        );

        // Counted before the task starts, so a `drain` that follows `fire` cannot miss it
        let running = Running::new(self);
        tokio::spawn(async move {
            let _ = running
                .0
                .run(script, name, event_name, env, log_limit)
                .await;
        });
    }

    /// Run one hook to its end and log how it went.
    async fn run(
        &self,
        script: String,
        name: String,
        event: String,
        env: ChildEnv,
        log_limit: u64,
    ) -> ScriptOutcome {
        let log_path = self.paths.hooks_log(&name);
        let path = log_path.clone();
        let prepared =
            tokio::task::spawn_blocking(move || prepare_hook_log(&path, log_limit)).await;
        match prepared {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Cannot prepare {}: {}", log_path.display(), e),
            Err(e) => warn!("Cannot prepare hook log: {}", e),
        }
        let spec = ScriptSpec {
            script,
            cwd: None,
            env,
            timeout: self.timeout,
            output: Some(log_path),
        };
        let mut group = None;
        let outcome = run_script_with(spec, |pgid| {
            self.live_groups.lock().insert(pgid);
            group = Some(pgid);
        })
        .await;
        if let Some(pgid) = group {
            self.live_groups.lock().remove(&pgid);
        }
        match &outcome {
            ScriptOutcome::Exited(status) if status.success() => {
                info!("Hook {} completed successfully for {}", event, name)
            }
            ScriptOutcome::Exited(status) => {
                error!("Hook {} failed for {}: {}", event, name, status)
            }
            ScriptOutcome::TimedOut => error!(
                "Hook {} for {} timed out after {:?}; its process group was killed",
                event, name, self.timeout
            ),
            ScriptOutcome::SpawnFailed(e) => error!("Hook {} failed for {}: {}", event, name, e),
        }
        outcome
    }

    /// Wait up to `grace` for the running hooks. When the time is up the process group of
    /// every hook that is still running gets SIGKILL, so no hook outlives the daemon.
    pub async fn drain(&self, grace: Duration) {
        let mut rx = self.running.subscribe();
        if tokio::time::timeout(grace, rx.wait_for(|n| *n == 0))
            .await
            .is_ok()
        {
            return;
        }
        let groups = self.live_groups();
        warn!(
            "{} hook(s) still running after {:?}: killing them",
            groups.len(),
            grace
        );
        for group in groups {
            let _ = oxidepm_runtime::kill_group(group, Signal::SIGKILL);
        }
        // the killed scripts end at once; give their tasks a moment to record it
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|n| *n == 0)).await;
    }
}

fn prepare_hook_log(path: &std::path::Path, limit: u64) -> std::io::Result<()> {
    use std::io::ErrorKind;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => {
            if file.metadata()?.len() >= limit {
                file.set_len(0)?;
            }
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The `OPM_*` variables of one hook run.
fn hook_vars(name: &str, event: &str, ctx: &HookCtx) -> Vec<(String, String)> {
    let mut vars = vec![
        ("OPM_APP_ID".to_string(), ctx.app_id.to_string()),
        ("OPM_APP_NAME".to_string(), name.to_string()),
        ("OPM_EVENT".to_string(), event.to_string()),
    ];
    if let Some(pid) = ctx.pid {
        vars.push(("OPM_PID".to_string(), pid.to_string()));
    }
    if let Some(code) = ctx.exit_code {
        vars.push(("OPM_EXIT_CODE".to_string(), code.to_string()));
    }
    if let Some(reason) = &ctx.reason {
        vars.push(("OPM_REASON".to_string(), reason.clone()));
    }
    vars.push(("OPM_RESTARTS".to_string(), ctx.restarts.to_string()));
    vars
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Hooks;

    /// A throw-away OxidePM home so hook tests never write to the real one.
    struct TmpHome(tempfile::TempDir);

    impl TmpHome {
        fn new() -> Self {
            Self(tempfile::TempDir::new().unwrap())
        }

        fn paths(&self) -> Arc<Paths> {
            Arc::new(Paths::new(self.0.path()))
        }
    }

    /// Run `script` as the hook of an app and return how it ended and what it wrote to the
    /// hook log.
    async fn run_hook(
        tmp: &TmpHome,
        script: &str,
        app_id: u32,
        name: &str,
        event: &str,
        pid: Option<u32>,
        exit_code: Option<i32>,
    ) -> (ScriptOutcome, String) {
        let paths = tmp.paths();
        let runner = HookRunner::with_timeouts(
            Arc::clone(&paths),
            Duration::from_secs(30),
            Duration::from_secs(1),
        );
        let ctx = HookCtx {
            app_id,
            pid,
            exit_code,
            ..HookCtx::default()
        };
        let outcome = runner
            .run(
                script.to_string(),
                name.to_string(),
                event.to_string(),
                ChildEnv::inherit(hook_vars(name, event, &ctx)),
                oxidepm_core::constants::DEFAULT_LOG_MAX_SIZE,
            )
            .await;
        let log = std::fs::read_to_string(paths.hooks_log(name)).unwrap_or_default();
        (outcome, log)
    }

    #[tokio::test]
    async fn an_oversized_hook_log_is_trimmed_before_the_next_hook() {
        let tmp = TmpHome::new();
        let paths = tmp.paths();
        let runner = HookRunner::new(Arc::clone(&paths));
        let mut spec = AppSpec::new(
            "example".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            tmp.0.path().into(),
        );
        spec.log_max_size = Some(1024);
        spec.hooks.on_start = Some(format!("printf '%s' '{}'", "x".repeat(1200)));
        runner.fire(&spec, HookEvent::Start, HookCtx::default());
        runner.drain(Duration::from_secs(3)).await;
        assert_eq!(
            std::fs::metadata(paths.hooks_log("example")).unwrap().len(),
            1200
        );

        spec.hooks.on_start = Some("printf recent".into());
        runner.fire(&spec, HookEvent::Start, HookCtx::default());
        runner.drain(Duration::from_secs(3)).await;
        let log = std::fs::read_to_string(paths.hooks_log("example")).unwrap();
        assert_eq!(log.len(), "recent".len());
        assert_eq!(log, "recent");
    }

    #[tokio::test]
    async fn test_run_hook_script_simple() {
        let tmp = TmpHome::new();
        let (outcome, log) = run_hook(
            &tmp,
            "echo 'hello world'",
            1,
            "test-app",
            "start",
            Some(1234),
            None,
        )
        .await;
        assert!(outcome.success());
        assert_eq!(log.trim(), "hello world");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_env_vars() {
        let tmp = TmpHome::new();
        let (outcome, log) = run_hook(
            &tmp,
            "echo $OPM_APP_ID $OPM_APP_NAME $OPM_EVENT",
            42,
            "my-app",
            "stop",
            None,
            None,
        )
        .await;
        assert!(outcome.success());
        assert_eq!(log.trim(), "42 my-app stop");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_exit_code() {
        let tmp = TmpHome::new();
        let (outcome, log) = run_hook(
            &tmp,
            "echo $OPM_EXIT_CODE",
            1,
            "crashed-app",
            "crash",
            Some(5678),
            Some(137),
        )
        .await;
        assert!(outcome.success());
        assert_eq!(log.trim(), "137");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_pid() {
        let tmp = TmpHome::new();
        let (outcome, log) =
            run_hook(&tmp, "echo $OPM_PID", 1, "app", "start", Some(9999), None).await;
        assert!(outcome.success());
        assert_eq!(log.trim(), "9999");
    }

    #[tokio::test]
    async fn test_run_hook_script_failure() {
        let tmp = TmpHome::new();
        let (outcome, _) = run_hook(&tmp, "exit 1", 1, "failing-app", "start", None, None).await;
        match outcome {
            ScriptOutcome::Exited(status) => assert_eq!(status.code(), Some(1)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn test_run_hook_script_command_not_found() {
        let tmp = TmpHome::new();
        let command = "/nonexistent/command";
        let (outcome, log) = run_hook(&tmp, command, 1, "app", "start", None, None).await;
        match outcome {
            ScriptOutcome::Exited(status) => assert_eq!(status.code(), Some(127)),
            other => panic!("{other:?}"),
        }
        assert!(
            log.contains(command),
            "the shell's complaint is in the hook log: {log:?}"
        );
    }

    #[tokio::test]
    async fn test_run_hook_script_multiline_output() {
        let tmp = TmpHome::new();
        let (outcome, log) = run_hook(
            &tmp,
            "echo 'line1'; echo 'line2'; echo 'line3'",
            1,
            "app",
            "start",
            None,
            None,
        )
        .await;
        assert!(outcome.success());
        assert!(log.contains("line1"));
        assert!(log.contains("line2"));
        assert!(log.contains("line3"));
    }

    #[tokio::test]
    async fn test_run_hook_script_stderr() {
        let tmp = TmpHome::new();
        let (outcome, log) = run_hook(
            &tmp,
            "echo 'error' >&2; exit 1",
            1,
            "app",
            "start",
            None,
            None,
        )
        .await;
        assert!(!outcome.success());
        assert!(
            log.contains("error"),
            "stderr goes to the hook log: {log:?}"
        );
    }

    #[tokio::test]
    async fn fire_without_a_hook_for_the_event_runs_nothing() {
        let tmp = TmpHome::new();
        let runner =
            HookRunner::with_timeouts(tmp.paths(), Duration::from_secs(30), Duration::from_secs(1));
        let mut spec = AppSpec::new(
            "quiet".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            tmp.0.path().to_path_buf(),
        );
        spec.hooks = Hooks {
            on_stop: Some("echo stopped".into()),
            ..Hooks::default()
        };
        runner.fire(&spec, HookEvent::Start, HookCtx::default());
        runner.drain(Duration::from_secs(5)).await;
        assert!(!tmp.paths().hooks_log("quiet").exists());
        runner.fire(&spec, HookEvent::Stop, HookCtx::default());
        runner.drain(Duration::from_secs(5)).await;
        assert_eq!(
            std::fs::read_to_string(tmp.paths().hooks_log("quiet"))
                .unwrap()
                .trim(),
            "stopped"
        );
    }

    #[tokio::test]
    async fn drain_returns_at_once_when_no_hook_runs() {
        let tmp = TmpHome::new();
        let runner =
            HookRunner::with_timeouts(tmp.paths(), Duration::from_secs(30), Duration::from_secs(1));
        let started = std::time::Instant::now();
        runner.drain(Duration::from_secs(30)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn drain_waits_for_a_hook_that_finishes_within_the_grace() {
        let tmp = TmpHome::new();
        let runner =
            HookRunner::with_timeouts(tmp.paths(), Duration::from_secs(30), Duration::from_secs(1));
        let mut spec = AppSpec::new(
            "slow".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            tmp.0.path().to_path_buf(),
        );
        spec.hooks.on_start = Some("sleep 0.3; echo finished".into());
        runner.fire(&spec, HookEvent::Start, HookCtx::default());
        runner.drain(Duration::from_secs(10)).await;
        assert_eq!(
            std::fs::read_to_string(tmp.paths().hooks_log("slow"))
                .unwrap()
                .trim(),
            "finished"
        );
        assert!(runner.live_groups().is_empty());
    }

    /// Whether process group `pgid` still has a member that is not a zombie.
    fn group_has_live_member(pgid: i32) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        entries.flatten().any(|e| {
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                return false;
            };
            // "pid (comm) state ppid pgrp ..."; comm may hold spaces and parentheses
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                return false;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let pgrp = fields.nth(1).and_then(|p| p.parse::<i32>().ok());
            pgrp == Some(pgid) && state != Some("Z")
        })
    }

    #[tokio::test]
    async fn drain_kills_the_group_of_a_hook_that_outlives_the_grace() {
        let tmp = TmpHome::new();
        // a long timeout: only the drain can end the hook
        let runner = HookRunner::with_timeouts(
            tmp.paths(),
            Duration::from_secs(120),
            Duration::from_secs(1),
        );
        let mut spec = AppSpec::new(
            "long".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            tmp.0.path().to_path_buf(),
        );
        // a background child in the hook's group, with a duration literal nothing else carries
        spec.hooks.on_stop = Some("sleep 301.7791 & wait".into());
        runner.fire(&spec, HookEvent::Stop, HookCtx::default());
        let began = std::time::Instant::now();
        while runner.live_groups().is_empty() {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "the hook never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let groups = runner.live_groups();
        assert_eq!(groups.len(), 1);
        let pgid = groups[0];
        assert!(group_has_live_member(pgid));

        let started = std::time::Instant::now();
        runner.drain(Duration::from_millis(200)).await;
        let took = started.elapsed();

        // the kill ends the script at once; without it drain would sit out its 5 s wait
        assert!(took < Duration::from_secs(3), "drain took {took:?}");
        assert!(runner.live_groups().is_empty());
        // a member that was killed may need a moment to be reaped by init
        let waited = std::time::Instant::now();
        while group_has_live_member(pgid) {
            if waited.elapsed() > Duration::from_secs(2) {
                let _ = oxidepm_runtime::kill_group(pgid, Signal::SIGKILL);
                panic!("the hook's process group {pgid} survived the drain");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn hook_vars_carry_the_event() {
        let ctx = HookCtx {
            app_id: 3,
            pid: Some(10),
            exit_code: Some(2),
            reason: Some("because".into()),
            restarts: 4,
        };
        let vars: std::collections::HashMap<_, _> =
            hook_vars("web", "crash", &ctx).into_iter().collect();
        assert_eq!(vars["OPM_APP_ID"], "3");
        assert_eq!(vars["OPM_APP_NAME"], "web");
        assert_eq!(vars["OPM_EVENT"], "crash");
        assert_eq!(vars["OPM_PID"], "10");
        assert_eq!(vars["OPM_EXIT_CODE"], "2");
        assert_eq!(vars["OPM_REASON"], "because");
        assert_eq!(vars["OPM_RESTARTS"], "4");
        let bare: std::collections::HashMap<_, _> = hook_vars("web", "start", &HookCtx::default())
            .into_iter()
            .collect();
        assert!(
            !bare.contains_key("OPM_PID")
                && !bare.contains_key("OPM_EXIT_CODE")
                && !bare.contains_key("OPM_REASON")
        );
    }
}
