//! Tests of the actors that need the supervisor's inside (task counters, the store, an injected
//! runner, the mailbox) and so cannot go through the daemon-level harness.
//!
//! Every test has its own home and its own working directory, unique `sleep` markers, and ends
//! when its runtime drops (an actor's armed `GroupChild` then kills whatever it still owns).

use super::handle::{CmdCtx, Command, LimitKind, RestartCause, StartOrigin};
use super::Supervisor;
use async_trait::async_trait;
use oxidepm_core::{
    AppSpec, AppStatus, Desired, Error, Paths, Result as CoreResult, RunState, Runtime, Selector,
};
use oxidepm_db::{AppRecord, Database};
use oxidepm_runtime::{CmdRunner, LaunchPlan, Origin, PrepareResult, Runner, RuntimeSource};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    sup: Arc<Supervisor>,
    paths: Arc<Paths>,
    tmp: TempDir,
}

/// A fake runner: counts `prepare` calls, can delay them, and can block them for ever.
#[derive(Default, Clone)]
struct Fake {
    prepares: Arc<AtomicUsize>,
    delay_ms: Arc<AtomicU64>,
    block: bool,
    /// Apps whose `prepare` waits until their `Notify` is notified.
    holds: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// The names whose `prepare` has been entered, in order.
    entered: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Fake {
    /// Make the `prepare` of the app called `name` wait; the returned `Notify` lets it go.
    fn hold(&self, name: &str) -> Arc<tokio::sync::Notify> {
        let release = Arc::new(tokio::sync::Notify::new());
        self.holds
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::clone(&release));
        release
    }

    fn has_entered(&self, name: &str) -> bool {
        self.entered.lock().unwrap().iter().any(|n| n == name)
    }
}

struct FakeSource(Fake);

#[async_trait]
impl RuntimeSource for FakeSource {
    async fn runner_for(&self, _runtime: &Runtime, _origin: Origin) -> CoreResult<Box<dyn Runner>> {
        Ok(Box::new(FakeRunner(self.0.clone())))
    }
}

struct FakeRunner(Fake);

#[async_trait]
impl Runner for FakeRunner {
    async fn prepare(&self, spec: &AppSpec) -> CoreResult<PrepareResult> {
        self.0.prepares.fetch_add(1, Ordering::SeqCst);
        self.0.entered.lock().unwrap().push(spec.name.clone());
        let hold = self.0.holds.lock().unwrap().get(&spec.name).cloned();
        if let Some(release) = hold {
            release.notified().await;
        }
        if self.0.block {
            std::future::pending::<()>().await;
        }
        let delay = self.0.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        CmdRunner.prepare(spec).await
    }

    fn plan(&self, spec: &AppSpec) -> CoreResult<LaunchPlan> {
        CmdRunner.plan(spec)
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        CmdRunner.command_string(spec)
    }

    fn mode_name(&self) -> &'static str {
        "fake"
    }
}

async fn fixture() -> Fixture {
    fixture_with(None).await
}

async fn fixture_with(fake: Option<Fake>) -> Fixture {
    fixture_with_config(fake, super::SupervisorConfig::default()).await
}

async fn fixture_with_config(fake: Option<Fake>, config: super::SupervisorConfig) -> Fixture {
    let tmp = TempDir::new().expect("temp dir");
    let paths = Arc::new(Paths::new(tmp.path().join("home")));
    let db = Database::new(&paths.db()).await.expect("database");
    let sup = match fake {
        None => Supervisor::with_settings(db, Arc::clone(&paths), config).await,
        Some(fake) => {
            let runtimes: super::actor::RuntimeSourceRef = Arc::new(FakeSource(fake));
            Supervisor::with_config(db, Arc::clone(&paths), runtimes, config).await
        }
    }
    .expect("supervisor");
    Fixture {
        sup: Arc::new(sup),
        paths,
        tmp,
    }
}

impl Fixture {
    fn marker(&self) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("302.{:05}{:03}", std::process::id() % 100_000, n % 1000)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn cmd(&self, name: &str, sh: &str) -> AppSpec {
        let mut spec = AppSpec::new(
            name.to_string(),
            Runtime::CMD,
            "sh".to_string(),
            self.tmp.path().to_path_buf(),
        );
        spec.args = vec!["-c".to_string(), sh.to_string()];
        spec
    }

    fn sleeper(&self, name: &str) -> (AppSpec, String) {
        let m = self.marker();
        (self.cmd(name, &format!("exec sleep {m}")), m)
    }

    async fn state(&self, name: &str) -> RunState {
        self.sup
            .show(&Selector::ByName(name.to_string()))
            .await
            .expect("show")
            .unwrap_or_else(|| panic!("no app {name}"))
            .state
    }

    async fn wait_for(&self, name: &str, what: &str, f: impl Fn(&RunState) -> bool) -> RunState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let shown = self
                .sup
                .show(&Selector::ByName(name.to_string()))
                .await
                .expect("show");
            if let Some(info) = &shown {
                if f(&info.state) {
                    return info.state.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                shown.map(|i| i.state)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn handle(&self, name: &str) -> super::handle::ProcessHandle {
        self.sup
            .registry
            .by_name(name)
            .unwrap_or_else(|| panic!("{name} is not registered"))
    }
}

fn alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s[s.rfind(')').unwrap_or(0) + 2..].starts_with('Z'))
            .unwrap_or(false)
}

fn procs_with(needle: &str) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
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
        if cmdline.contains(needle) && alive(pid) {
            count += 1;
        }
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_while_building_cancels_pipeline() {
    let fx = fixture_with(Some(Fake {
        block: true,
        ..Fake::default()
    }))
    .await;
    let (spec, _m) = fx.sleeper("ut-building");
    let start = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.start(spec).await })
    };
    fx.wait_for("ut-building", "the build", |s| {
        s.status == AppStatus::Building
    })
    .await;
    assert_eq!(fx.sup.stats().pipelines, 1);
    let id = fx.state("ut-building").await.app_id;

    assert!(
        fx.sup.stop(id).await.expect("stop"),
        "something was stopped"
    );

    let outcome = start.await.expect("task");
    assert!(matches!(outcome, Err(Error::Cancelled(_))), "{outcome:?}");
    assert_eq!(fx.state("ut-building").await.status, AppStatus::Stopped);
    assert_eq!(
        fx.sup.stats().pipelines,
        0,
        "the pipeline future was dropped"
    );
}

/// Commands queued behind one another are handled in order: a `Stop` then a `Start` ends running.
#[tokio::test(flavor = "current_thread")]
async fn commands_are_processed_in_order() {
    let fx = fixture().await;
    let (spec, m) = fx.sleeper("ut-order");
    let id = fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-order");
    let spec = handle.snapshot().spec.clone();

    // current-thread runtime: the actor cannot run before the test awaits, so both are queued
    let (stop_reply, stopped) = oneshot::channel();
    let (start_reply, started) = oneshot::channel();
    assert!(handle.try_post(Command::Stop {
        ctx: CmdCtx::user(),
        reply: stop_reply
    }));
    assert!(handle.try_post(Command::Start {
        spec,
        origin: StartOrigin::User,
        created: false,
        ctx: CmdCtx::user(),
        reply: start_reply,
    }));

    assert!(stopped.await.unwrap().expect("stop").changed);
    started.await.unwrap().expect("start");
    let state = fx.state("ut-order").await;
    assert_eq!(
        state.status,
        AppStatus::Running,
        "the Start ran after the Stop"
    );
    assert_eq!(procs_with(&format!("sleep {m}")), 1);
    assert_eq!(state.app_id, id);
}

/// A report from a child that is gone is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_events_are_ignored() {
    let fx = fixture().await;
    let (spec, _m) = fx.sleeper("ut-stale");
    fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-stale");
    let gen = handle.snapshot().generation;
    assert!(gen >= 1);

    assert!(handle.try_post(Command::HealthFailed {
        gen: gen - 1,
        failures: 3,
        message: "stale".into()
    }));
    // a command that is answered tells the stale report was handled before it
    fx.sup
        .restart(0)
        .await
        .expect("no such app is not an error");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let state = fx.state("ut-stale").await;
    assert_eq!(
        state.status,
        AppStatus::Running,
        "the stale report changed nothing"
    );

    assert!(handle.try_post(Command::HealthFailed {
        gen,
        failures: 3,
        message: "current".into()
    }));
    let state = fx
        .wait_for("ut-stale", "the current report", |s| {
            s.status == AppStatus::Unhealthy
        })
        .await;
    assert!(state.pid.is_some(), "an unhealthy app keeps its child");
}

/// `status` and `show` read the published snapshot: a stop that waits out its kill timeout does
/// not hold them up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reads_do_not_wait_for_a_slow_stop() {
    let fx = fixture().await;
    let ready = fx.file("slow.ready");
    let mut spec = fx.cmd(
        "ut-slow-stop",
        &format!(
            "trap '' TERM; touch '{}'; while :; do sleep 0.1; done",
            ready.display()
        ),
    );
    spec.kill_timeout_ms = 2500;
    let id = fx.sup.start(spec).await.expect("start");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the app never became ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stop = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.stop(id).await })
    };
    fx.wait_for("ut-slow-stop", "stopping", |s| {
        s.status == AppStatus::Stopping
    })
    .await;

    let begun = Instant::now();
    let apps = fx.sup.status().await.expect("status");
    let shown = fx
        .sup
        .show(&Selector::ById(id))
        .await
        .expect("show")
        .expect("app");
    assert!(
        begun.elapsed() < Duration::from_millis(500),
        "reads took {:?}",
        begun.elapsed()
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(shown.state.status, AppStatus::Stopping);
    assert!(
        !stop.is_finished(),
        "the stop is still waiting for its kill timeout"
    );

    assert!(stop.await.expect("task").expect("stop"));
    assert_eq!(fx.state("ut-slow-stop").await.status, AppStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_panic_kills_process_group_and_marks_errored() {
    let fx = fixture().await;
    let (spec, m) = fx.sleeper("ut-panic");
    let id = fx.sup.start(spec.clone()).await.expect("start");
    assert_eq!(procs_with(&format!("sleep {m}")), 1);

    assert!(fx.sup.panic_actor_for_test(id));

    let state = fx
        .wait_for("ut-panic", "the panic to be turned into errored", |s| {
            s.status == AppStatus::Errored
        })
        .await;
    assert_eq!(
        state.last_error.as_deref(),
        Some("internal error: actor panicked")
    );
    assert_eq!(state.pid, None);
    let deadline = Instant::now() + Duration::from_secs(3);
    while procs_with(&format!("sleep {m}")) > 0 {
        assert!(
            Instant::now() < deadline,
            "the process group survived the panic"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // the registry slot has a working actor again
    fx.sup.start(spec).await.expect("start again");
    assert_eq!(fx.state("ut-panic").await.status, AppStatus::Running);
}

/// A crash restart reuses the plan the first launch cached: it does not prepare again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_restart_reuses_cached_plan_and_skips_prepare() {
    let fake = Fake::default();
    let fx = fixture_with(Some(fake.clone())).await;
    let m = fx.marker();
    let mut spec = fx.cmd("ut-cached", &format!("exec sleep {m}"));
    spec.restart_policy.restart_delay_ms = 20;
    fx.sup.start(spec).await.expect("start");
    assert_eq!(fake.prepares.load(Ordering::SeqCst), 1);

    let first = fx.state("ut-cached").await.pid.expect("pid");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(first as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill");
    fx.wait_for("ut-cached", "the automatic restart", |s| {
        s.status == AppStatus::Running && s.pid.map(|p| p != first).unwrap_or(false)
    })
    .await;
    assert_eq!(
        fake.prepares.load(Ordering::SeqCst),
        1,
        "the restart did not prepare"
    );

    // an operator restart does prepare
    let id = fx.state("ut-cached").await.app_id;
    assert!(fx.sup.restart(id).await.expect("restart"));
    assert_eq!(fake.prepares.load(Ordering::SeqCst), 2);
}

/// Two restarts that land while the first is still launching both get an answer, from the one
/// launch that follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_restart_while_pending_resolves_all_replies() {
    let fake = Fake::default();
    let fx = fixture_with(Some(fake.clone())).await;
    let (spec, m) = fx.sleeper("ut-two");
    let id = fx.sup.start(spec).await.expect("start");
    fake.delay_ms.store(600, Ordering::SeqCst);

    let (first, second) = {
        let (a, b) = (Arc::clone(&fx.sup), Arc::clone(&fx.sup));
        (
            tokio::spawn(async move { a.restart(id).await }),
            tokio::spawn(async move { b.restart(id).await }),
        )
    };
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert!(first.expect("first restart answered"));
    assert!(second.expect("second restart answered"));

    assert_eq!(fx.state("ut-two").await.status, AppStatus::Running);
    assert_eq!(procs_with(&format!("sleep {m}")), 1, "one child");
}

/// A delete whose row cannot be removed changes nothing: the actor stays, the app runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_db_failure_keeps_actor_and_registry_entry() {
    let fx = fixture().await;
    let (spec, m) = fx.sleeper("ut-del-fail");
    let id = fx.sup.start(spec).await.expect("start");
    assert_eq!(fx.sup.stats().actors, 1);

    fx.sup.close_db().await; // every later database call fails
    let err = fx
        .sup
        .delete(id)
        .await
        .expect_err("the row cannot be deleted");
    assert!(matches!(err, Error::DbError(_)), "{err}");

    assert!(fx.sup.registry.get(id).is_some(), "still registered");
    assert_eq!(fx.sup.stats().actors, 1, "the actor stays");
    assert_eq!(
        procs_with(&format!("sleep {m}")),
        1,
        "the app was not touched"
    );
    assert_eq!(
        fx.handle("ut-del-fail").snapshot().state.status,
        AppStatus::Running
    );
}

/// The desired state is written before the child is terminated, so a daemon killed during the
/// kill timeout cannot leave a stopped app marked as running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_persists_desired_before_terminating() {
    let fx = fixture().await;
    let ready = fx.file("persist.ready");
    let mut spec = fx.cmd(
        "ut-persist",
        &format!(
            "trap '' TERM; touch '{}'; while :; do sleep 0.1; done",
            ready.display()
        ),
    );
    spec.kill_timeout_ms = 2500;
    let id = fx.sup.start(spec).await.expect("start");
    let pid = fx.state("ut-persist").await.pid.expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the app never became ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let desired_of = |fx: &Fixture| {
        let sup = Arc::clone(&fx.sup);
        async move {
            match sup.db.apps().record_by_id(id).await.expect("record") {
                Some(AppRecord::Ok { desired, .. }) => desired,
                other => panic!("{other:?}"),
            }
        }
    };
    assert_eq!(desired_of(&fx).await, Desired::Running);

    let stop = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.stop(id).await })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while desired_of(&fx).await != Desired::Stopped {
        assert!(Instant::now() < deadline, "desired was never written");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        alive(pid),
        "desired=stopped is on disk while the child still runs"
    );
    assert!(!stop.is_finished());

    stop.await.unwrap().expect("stop");
    assert!(!alive(pid));
}

/// The parent of a cluster publishes a new snapshot every time one of its children changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_snapshot_changes_when_child_changes() {
    let fx = fixture().await;
    let m = fx.marker();
    let mut spec = fx.cmd("ut-group", &format!("exec sleep {m}"));
    spec.instances = 2;
    fx.sup.start(spec).await.expect("start cluster");
    let group = fx.handle("ut-group");
    assert_eq!(group.snapshot().state.status, AppStatus::Running);
    assert_eq!(group.snapshot().children.len(), 2);

    let mut seen = group.subscribe();
    seen.borrow_and_update();
    let child = fx.handle("ut-group-0");
    fx.sup.stop(child.id).await.expect("stop one instance");

    tokio::time::timeout(Duration::from_secs(3), seen.changed())
        .await
        .expect("the group published after a child changed")
        .expect("open");
    // one of two instances is stopped: the parent is no longer running
    let deadline = Instant::now() + Duration::from_secs(3);
    while group.snapshot().state.status == AppStatus::Running {
        assert!(
            Instant::now() < deadline,
            "the derived status never changed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Commands behind a `Shutdown` are answered `ShuttingDown`, the mailbox is drained, and a
/// `Start` after it is refused.
#[tokio::test(flavor = "current_thread")]
async fn start_after_shutdown_seen_is_rejected_and_queue_is_drained() {
    let fx = fixture().await;
    let (spec, _m) = fx.sleeper("ut-drain");
    fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-drain");
    let spec = handle.snapshot().spec.clone();

    let (shutdown_reply, shutdown) = oneshot::channel();
    let (start_reply, start) = oneshot::channel();
    let (restart_reply, restart) = oneshot::channel();
    let (stop_reply, stop) = oneshot::channel();
    assert!(handle.try_post(Command::Shutdown {
        kill_cap: Duration::from_secs(2),
        reply: shutdown_reply
    }));
    assert!(handle.try_post(Command::Start {
        spec,
        origin: StartOrigin::User,
        created: false,
        ctx: CmdCtx::user(),
        reply: start_reply,
    }));
    assert!(handle.try_post(Command::Restart {
        cause: RestartCause::User,
        ctx: CmdCtx::user(),
        reply: restart_reply
    }));
    assert!(handle.try_post(Command::Stop {
        ctx: CmdCtx::user(),
        reply: stop_reply
    }));

    assert!(
        shutdown.await.unwrap().expect("shutdown"),
        "it stopped the running app"
    );
    assert!(matches!(start.await.unwrap(), Err(Error::ShuttingDown)));
    assert!(matches!(restart.await.unwrap(), Err(Error::ShuttingDown)));
    assert!(matches!(stop.await.unwrap(), Err(Error::ShuttingDown)));
    assert!(
        !handle.try_post(Command::TestPanic),
        "the mailbox is closed"
    );
}

/// `reload` is a restart: same name, id and row, and the outcome says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_single_reports_restart_not_zero_downtime() {
    let fx = fixture().await;
    let (spec, _m) = fx.sleeper("ut-reload");
    let id = fx.sup.start(spec).await.expect("start");

    let outcome = fx
        .sup
        .reload_outcome(id)
        .await
        .expect("reload")
        .expect("registered");

    assert_eq!(outcome.kind, super::handle::RestartKind::Restart);
    assert_eq!(outcome.status, AppStatus::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_cluster_restarts_every_child_and_reports_restart() {
    let fx = fixture().await;
    let m = fx.marker();
    let mut spec = fx.cmd("ut-reload-cl", &format!("exec sleep {m}"));
    spec.instances = 3;
    let parent = fx.sup.start(spec).await.expect("start cluster");
    let children = fx.sup.child_ids(parent);
    assert_eq!(children.len(), 3);
    let mut before = Vec::new();
    for child in &children {
        before.push(
            fx.sup
                .show(&Selector::ById(*child))
                .await
                .unwrap()
                .unwrap()
                .state
                .pid
                .unwrap(),
        );
    }
    let rows = fx.sup.status().await.unwrap().len();

    let outcome = fx
        .sup
        .reload_outcome(parent)
        .await
        .expect("reload")
        .expect("registered");

    assert_eq!(outcome.kind, super::handle::RestartKind::Restart);
    for (child, old) in children.iter().zip(before) {
        let state = fx
            .sup
            .show(&Selector::ById(*child))
            .await
            .unwrap()
            .unwrap()
            .state;
        assert_eq!(state.status, AppStatus::Running);
        assert_ne!(state.pid, Some(old), "child {child} has a new pid");
    }
    assert_eq!(fx.sup.child_ids(parent), children, "ids unchanged");
    assert_eq!(fx.sup.status().await.unwrap().len(), rows, "rows unchanged");
    assert_eq!(procs_with(&format!("sleep {m}")), 3);
}

/// The task counters see what the supervisor owns and show it gone after stop and delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_count_the_tasks_an_app_owns() {
    let fx = fixture().await;
    let (mut spec, _m) = fx.sleeper("ut-stats");
    spec.restart_cron = Some("0 4 * * *".to_string());
    let id = fx.sup.start(spec).await.expect("start");
    let stats = fx.sup.stats();
    assert_eq!(
        (
            stats.actors,
            stats.cron_tasks,
            stats.pipelines,
            stats.capture_tasks
        ),
        (1, 1, 0, 2)
    );

    fx.sup.stop(id).await.expect("stop");
    let stats = fx.sup.stats();
    assert_eq!(
        (stats.actors, stats.cron_tasks, stats.capture_tasks),
        (1, 0, 0)
    );

    fx.sup.delete(id).await.expect("delete");
    let deadline = Instant::now() + Duration::from_secs(2);
    while fx.sup.stats().actors > 0 {
        assert!(Instant::now() < deadline, "the actor did not end");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(fx.paths.logs_dir().exists());
}

/// Five restarts leave one health loop and one cron timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_five_times_keeps_one_task_of_each_kind() {
    let fx = fixture().await;
    let (mut spec, _m) = fx.sleeper("ut-five");
    spec.restart_cron = Some("0 4 * * *".to_string());
    spec.health_check = Some(oxidepm_core::HealthCheck {
        interval_secs: 1,
        timeout_secs: 1,
        retries: 1,
        start_period_secs: 0,
        ..oxidepm_core::HealthCheck::script("true")
    });
    let id = fx.sup.start(spec).await.expect("start");
    for _ in 0..5 {
        assert!(fx.sup.restart(id).await.expect("restart"));
    }
    let stats = fx.sup.stats();
    assert_eq!(stats.actors, 1);
    assert_eq!(stats.health_tasks, 1);
    assert_eq!(stats.cron_tasks, 1);
    assert_eq!(stats.pipelines, 0);
    assert_eq!(stats.capture_tasks, 2);

    fx.sup.stop(id).await.expect("stop");
    let stats = fx.sup.stats();
    assert_eq!(
        (stats.health_tasks, stats.cron_tasks, stats.capture_tasks),
        (0, 0, 0)
    );

    fx.sup.delete(id).await.expect("delete");
    let deadline = Instant::now() + Duration::from_secs(2);
    while fx.sup.stats().actors > 0 {
        assert!(Instant::now() < deadline, "the actor did not end");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The hard-kill token (the second signal, the shutdown's fallback) cuts a stop that is waiting
/// out its kill timeout short: the actor kills its own group at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_token_cuts_a_slow_stop_short() {
    let fx = fixture().await;
    let ready = fx.file("hard.ready");
    let mut spec = fx.cmd(
        "ut-hard",
        &format!(
            "trap '' TERM; touch '{}'; while :; do sleep 0.1; done",
            ready.display()
        ),
    );
    spec.kill_timeout_ms = 30_000;
    let id = fx.sup.start(spec).await.expect("start");
    let pid = fx.state("ut-hard").await.pid.expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the app never became ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stop = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.stop(id).await })
    };
    fx.wait_for("ut-hard", "stopping", |s| s.status == AppStatus::Stopping)
        .await;
    assert!(alive(pid));

    let begun = Instant::now();
    fx.sup.kill_all_now();

    tokio::time::timeout(Duration::from_secs(5), stop)
        .await
        .expect("the stop ended long before its 30 s kill timeout")
        .expect("task")
        .expect("stop");
    assert!(begun.elapsed() < Duration::from_secs(5));
    assert!(!alive(pid));
    assert_eq!(fx.state("ut-hard").await.status, AppStatus::Stopped);
}

fn memory_limit(gen: u64) -> Command {
    Command::LimitExceeded {
        gen,
        kind: LimitKind::Memory {
            observed_mb: 80,
            limit_mb: 20,
        },
    }
}

/// A user `Stop` that is processed between the sampler's snapshot and its limit restart wins:
/// the late `LimitExceeded` finds a stopped app and does nothing (it never starts one).
#[tokio::test(flavor = "current_thread")]
async fn stop_wins_over_a_pending_limit_restart() {
    let fx = fixture().await;
    let (spec, m) = fx.sleeper("ut-stopwins");
    fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-stopwins");
    let gen = handle.live().proc().expect("a live child").gen;
    let (stop_reply, stop) = oneshot::channel();

    // current-thread runtime: the actor cannot run before the test awaits, so both are queued
    assert!(handle.try_post(Command::Stop {
        ctx: CmdCtx::user(),
        reply: stop_reply
    }));
    assert!(handle.try_post(memory_limit(gen)));
    assert!(stop.await.unwrap().expect("stop").changed);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let state = fx.state("ut-stopwins").await;
    assert_eq!(state.status, AppStatus::Stopped, "{state:?}");
    assert_eq!(state.restarts, 0);
    assert_eq!(procs_with(&format!("sleep {m}")), 0, "nothing was started");

    // and a report that arrives much later still starts nothing
    assert!(handle.try_post(memory_limit(gen)));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fx.state("ut-stopwins").await.status, AppStatus::Stopped);
    assert_eq!(procs_with(&format!("sleep {m}")), 0);
}

/// A limit report for the current child replaces it (a new pid, one more restart); one that
/// names an older child is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_report_restarts_only_the_child_it_names() {
    let fx = fixture().await;
    let (spec, _m) = fx.sleeper("ut-gen");
    fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-gen");
    let first = handle.live().proc().expect("a live child");

    assert!(handle.try_post(memory_limit(first.gen + 7)));
    tokio::time::sleep(Duration::from_millis(700)).await;
    let state = fx.state("ut-gen").await;
    assert_eq!(
        state.pid,
        Some(first.pid),
        "a report for another generation is ignored"
    );
    assert_eq!(state.restarts, 0);

    assert!(handle.try_post(memory_limit(first.gen)));
    let state = fx
        .wait_for("ut-gen", "the limit restart", |s| {
            s.status == AppStatus::Running && s.pid.is_some() && s.pid != Some(first.pid)
        })
        .await;
    assert_eq!(state.restarts, 1);
    assert!(!alive(first.pid));
}

/// The launch records the leader's start time next to its pid, the real probe measures that
/// child, and the actor clears the record when it reaps it (a stop, or an exit by itself), so
/// the sampler has nothing to read for a dead pid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_proc_is_recorded_at_launch_and_cleared_after_reap() {
    use super::sampler::{start_ticks, Sampler, SysProbe};

    let fx = fixture().await;
    let (spec, _m) = fx.sleeper("ut-live");
    fx.sup.start(spec).await.expect("start");
    let handle = fx.handle("ut-live");
    let proc = handle.live().proc().expect("a live child");
    assert_ne!(proc.start_ticks, 0);
    assert_eq!(start_ticks(proc.pid), Some(proc.start_ticks));

    let sampler = Sampler::new(SysProbe::default());
    sampler.tick(std::slice::from_ref(&handle)).await;
    assert!(
        handle.live().mem_bytes() > 0,
        "the sleeper's resident memory was read"
    );

    assert!(fx.sup.stop(handle.id).await.expect("stop"));
    assert_eq!(handle.live().proc(), None);
    assert_eq!(handle.live().mem_bytes(), 0);

    // an exit by itself: no restart, so the app ends errored with nothing alive
    let mut spec = fx.cmd("ut-live-exit", "exit 3");
    spec.restart_policy.auto_restart = false;
    fx.sup.start(spec).await.expect("start");
    fx.wait_for("ut-live-exit", "the exit", |s| {
        s.status == AppStatus::Errored
    })
    .await;
    assert_eq!(fx.handle("ut-live-exit").live().proc(), None);
}

#[tokio::test]
async fn samples_cpu_and_resident_memory_of_children_and_grandchildren() {
    use super::sampler::{Sampler, SysProbe};

    let fx = fixture().await;
    let spec = fx.cmd(
        "tree",
        r#"
        sh -c 'sleep 30 & echo "$!" > grandchild.pid; wait' &
        parent=$!
        sleep 30 &
        sleeper=$!
        sh -c 'printf "\377)x" > /proc/self/comm; : > worker.ready; while :; do :; done' &
        worker=$!
        printf '%s\n' "$$" "$parent" "$sleeper" "$worker" > tree.pids
        wait
        "#,
    );
    let id = fx.sup.start(spec).await.unwrap();
    let handle = fx.handle("tree");
    let pids = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pids: Vec<u32> = ["tree.pids", "grandchild.pid"]
                .iter()
                .flat_map(|file| {
                    std::fs::read_to_string(fx.file(file))
                        .unwrap_or_default()
                        .split_whitespace()
                        .filter_map(|pid| pid.parse().ok())
                        .collect::<Vec<_>>()
                })
                .collect();
            if pids.len() == 5 && fx.file("worker.ready").exists() {
                break pids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture reports all five pids");
    let sampler = Sampler::new(SysProbe::default());
    sampler.tick(std::slice::from_ref(&handle)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    sampler.tick(std::slice::from_ref(&handle)).await;
    let tree = fx.sup.app_stats(id).unwrap().tree;
    let leader_cpu = handle.run_state().cpu_percent;
    let rss_pages: u64 = pids
        .into_iter()
        .map(|pid| {
            let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
            let end_of_name = stat.iter().rposition(|&byte| byte == b')').unwrap();
            std::str::from_utf8(&stat[end_of_name + 1..])
                .unwrap()
                .split_whitespace()
                .nth(24 - 3)
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .sum();
    let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .unwrap()
        .unwrap() as u64;
    fx.sup.stop(id).await.unwrap();

    assert_eq!(tree.process_count, 5);
    let expected_rss = rss_pages * page_size;
    assert!(
        tree.memory_bytes.abs_diff(expected_rss) <= 32 * page_size,
        "sampled {} bytes, expected {expected_rss}",
        tree.memory_bytes
    );
    assert!(
        tree.cpu_percent > leader_cpu,
        "{tree:?}, leader CPU {leader_cpu}"
    );
    assert_eq!(fx.sup.app_stats(id).unwrap().tree, Default::default());
}

#[tokio::test]
async fn samples_fifty_apps_within_the_interval_budget() {
    use super::sampler::{Sampler, SysProbe};

    let fx = fixture().await;
    let mut handles = Vec::new();
    for index in 0..50 {
        let name = format!("sample-{index}");
        let (spec, _) = fx.sleeper(&name);
        fx.sup.start(spec).await.unwrap();
        handles.push(fx.handle(&name));
    }

    let sampler = Sampler::new(SysProbe::default());
    let start = Instant::now();
    sampler.tick(&handles).await;
    let elapsed = start.elapsed();
    eprintln!("Sampled 50 live apps in {elapsed:?}");
    assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
    for handle in handles {
        assert_eq!(fx.sup.app_stats(handle.id).unwrap().tree.process_count, 1);
    }
    fx.sup.shutdown(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stdout_and_stderr_bytes_reset_the_output_age() {
    let fx = fixture().await;
    let silent = fx.sup.start(fx.sleeper("silent").0).await.unwrap();
    let chatty = fx
        .sup
        .start(fx.cmd(
            "chatty",
            r#"
            while [ ! -e say-out ]; do sleep .02; done
            printf x
            while [ ! -e say-err ]; do sleep .02; done
            printf y >&2
            exec sleep 30
            "#,
        ))
        .await
        .unwrap();
    let stats = |id| fx.sup.app_stats(id).unwrap().output;
    let initial_age = stats(silent).seconds_since_output.expect("capture age");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let silent_stats = stats(silent);
    assert!(silent_stats.seconds_since_output.unwrap() > initial_age);
    assert_eq!(silent_stats.last_output_at, None);
    assert_eq!(stats(chatty).last_output_at, None);
    assert!(stats(chatty).seconds_since_output.unwrap() >= 1);

    let wait_for_output = |previous| async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let output = stats(chatty);
                if output.last_output_at > previous {
                    break output;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the capture records the bytes")
    };
    std::fs::write(fx.file("say-out"), "").unwrap();
    let stdout = wait_for_output(None).await;
    assert!(stdout.seconds_since_output.unwrap() < stats(silent).seconds_since_output.unwrap());
    std::fs::write(fx.file("say-err"), "").unwrap();
    let stderr = wait_for_output(stdout.last_output_at).await;
    assert!(stderr.seconds_since_output.unwrap() < stats(silent).seconds_since_output.unwrap());
    for stream in ["out", "err"] {
        let path = fx.paths.log_path("chatty", stream);
        assert_eq!(path.metadata().unwrap().len(), 0, "no complete line yet");
    }
    fx.sup.stop(chatty).await.unwrap();
    assert_eq!(stats(chatty), Default::default());
    fx.sup.stop(silent).await.unwrap();
}

/// A stop cancels the dependency wait: the pipeline future is dropped (no task is left waiting
/// on the dependency's snapshot) and the app is stopped, not errored by a later timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deps_wait_is_cancelled_by_stop() {
    let fx = fixture().await;
    // the batch starts both; the dependency is held in its startup delay
    let mut slow_db = fx.cmd("ut-dep-slow", "exec sleep 306.1");
    slow_db.startup_delay_ms = Some(30_000);
    let mut web = fx.cmd("ut-dep-web", "exec sleep 306.2");
    web.depends_on = vec!["ut-dep-slow".to_string()];
    web.depends_timeout_secs = 2;
    let sup = Arc::clone(&fx.sup);
    let batch = tokio::spawn(async move { sup.start_batch(vec![slow_db, web]).await });
    fx.wait_for("ut-dep-web", "the dependent to wait", |s| {
        s.status == AppStatus::Waiting
    })
    .await;
    assert!(fx.sup.stats().pipelines >= 1);
    let id = fx.state("ut-dep-web").await.app_id;

    assert!(fx.sup.stop(id).await.expect("stop the waiting app"));

    assert_eq!(fx.state("ut-dep-web").await.status, AppStatus::Stopped);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // only the db's own pipeline (its delay) is left
    assert_eq!(
        fx.sup.stats().pipelines,
        1,
        "the dependent's pipeline was dropped"
    );
    // the timeout it had would have errored it by now if the wait had survived
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fx.state("ut-dep-web").await.status, AppStatus::Stopped);
    let db_id = fx.state("ut-dep-slow").await.app_id;
    fx.sup.stop(db_id).await.expect("stop db");
    let _ = batch.await;
}

/// A dependent that waits holds no launch slot. With one slot, the slot is taken by an app that
/// is being prepared; the dependent is then started first and its dependency second, so both
/// are in line when the slot is given back. Were the dependent to take its place in the line for
/// the slot before waiting for its dependency, it would get the slot first and the dependency
/// would never launch. Nothing here depends on how fast the actors run: the order of the two
/// starts is fixed by the test, and the line is formed while the slot is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_dependent_does_not_hold_the_only_launch_slot() {
    let fake = Fake::default();
    let release_hog = fake.hold("ut-slot-hog");
    let fx = fixture_with_config(
        Some(fake.clone()),
        super::SupervisorConfig {
            launch_concurrency: 1,
        },
    )
    .await;
    let (hog, _) = fx.sleeper("ut-slot-hog");
    let mut web = fx.cmd("ut-slot-web", "exec sleep 306.3");
    web.depends_on = vec!["ut-slot-db".to_string()];
    // a failure of the slot rule shows within seconds instead of after the default minute
    web.depends_timeout_secs = 5;
    let db = fx.cmd("ut-slot-db", "exec sleep 306.4");

    // the hog holds the only slot while it is prepared
    let hog_start = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.start(hog).await })
    };
    while !fake.has_entered("ut-slot-hog") {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // both apps exist before either starts, as in a batch
    let web = fx
        .sup
        .register_for_start(web)
        .await
        .expect("register the dependent");
    let db = fx
        .sup
        .register_for_start(db)
        .await
        .expect("register the dependency");

    // the dependent first: it is told to wait, and its pipeline is in the gate or in the line
    let outcome = fx
        .sup
        .send_first_start(web)
        .await
        .expect("the dependent is accepted");
    assert_eq!(outcome.status, AppStatus::Waiting);
    // the dependency second: it can only be in the line
    let db_start = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.send_first_start(db).await })
    };
    fx.wait_for("ut-slot-db", "the dependency to be building", |s| {
        s.status == AppStatus::Building
    })
    .await;
    assert_eq!(
        fx.state("ut-slot-web").await.status,
        AppStatus::Waiting,
        "the dependent waits outside the line"
    );

    release_hog.notify_one();

    db_start.await.unwrap().expect("the dependency launched");
    hog_start.await.unwrap().expect("the hog launched");
    fx.wait_for("ut-slot-web", "the dependent to run", |s| {
        s.status == AppStatus::Running
    })
    .await;
    fx.wait_for("ut-slot-db", "the dependency to run", |s| {
        s.status == AppStatus::Running
    })
    .await;
    for name in ["ut-slot-web", "ut-slot-db", "ut-slot-hog"] {
        fx.sup
            .stop(fx.state(name).await.app_id)
            .await
            .expect("stop");
    }
}

/// The last look at the dependencies, immediately before the spawn: a dependency that stops
/// while the dependent is being prepared sends the dependent back to waiting and nothing is
/// launched; when the dependency is running again the dependent goes on without preparing
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_that_leaves_running_during_prepare_sends_the_dependent_back_to_waiting() {
    let fake = Fake::default();
    let release_web = fake.hold("ut-last-web");
    let fx = fixture_with(Some(fake.clone())).await;
    let (db, _) = fx.sleeper("ut-last-db");
    let (mut web, web_marker) = fx.sleeper("ut-last-web");
    web.depends_on = vec!["ut-last-db".to_string()];

    let db_id = fx.sup.start(db.clone()).await.expect("dependency");
    fx.wait_for("ut-last-db", "the dependency to run", |s| {
        s.status == AppStatus::Running
    })
    .await;
    // the dependency runs, so the dependent is let through the gate and is prepared
    let web_start = {
        let sup = Arc::clone(&fx.sup);
        tokio::spawn(async move { sup.start(web).await })
    };
    fx.wait_for("ut-last-web", "the dependent to be building", |s| {
        s.status == AppStatus::Building
    })
    .await;
    assert!(fake.has_entered("ut-last-web"));

    // the dependency leaves running during the slow prepare
    assert!(fx.sup.stop(db_id).await.expect("stop the dependency"));
    release_web.notify_one();

    let state = fx
        .wait_for("ut-last-web", "the dependent to wait again", |s| {
            s.status == AppStatus::Waiting
        })
        .await;
    assert_eq!(state.pid, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fx.state("ut-last-web").await.status,
        AppStatus::Waiting,
        "still waiting"
    );
    assert_eq!(
        procs_with(&format!("sleep {web_marker}")),
        0,
        "nothing was launched"
    );
    assert!(
        !web_start.is_finished(),
        "the start is still waiting for its launch"
    );

    // the dependency comes back: the dependent launches, from the plan it already has
    fx.sup.start(db).await.expect("dependency again");
    web_start.await.unwrap().expect("the dependent launched");
    fx.wait_for("ut-last-web", "the dependent to run", |s| {
        s.status == AppStatus::Running
    })
    .await;
    assert_eq!(procs_with(&format!("sleep {web_marker}")), 1);
    assert_eq!(
        fake.prepares.load(Ordering::SeqCst),
        3,
        "dependency twice and the dependent once"
    );
    fx.sup
        .stop(fx.state("ut-last-web").await.app_id)
        .await
        .expect("stop");
    fx.sup.stop(db_id).await.expect("stop");
}

/// a resurrect `Start` that arrives after a stop is answered "nothing to do" and starts
/// nothing, however late it is (the targets of a resurrect are chosen before the starts go out).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_resurrect_start_does_not_override_a_stop() {
    let fx = fixture().await;
    let (spec, m) = fx.sleeper("ut-late");
    let id = fx.sup.start(spec).await.expect("start");
    fx.sup.stop(id).await.expect("stop");
    let handle = fx.handle("ut-late");
    let spec = handle.snapshot().spec.clone();

    let outcome = handle
        .request(|reply| Command::Start {
            spec,
            origin: StartOrigin::Resurrect,
            created: false,
            ctx: CmdCtx::resurrect(),
            reply,
        })
        .await
        .expect("answered");

    assert!(!outcome.changed);
    assert_eq!(fx.state("ut-late").await.status, AppStatus::Stopped);
    assert_eq!(procs_with(&format!("sleep {m}")), 0);
}

// ---- actor identity ----------------------------------------------------------

use super::events::tests::Recorder;
use super::events::{LifecycleEvent, LifecycleKind};
use super::handle::{Cause, CURRENT_ACTOR};

/// A fixture whose lifecycle events are also collected by a recorder.
async fn recorded_fixture() -> (Fixture, Arc<Recorder>) {
    let tmp = TempDir::new().expect("temp dir");
    let paths = Arc::new(Paths::new(tmp.path().join("home")));
    let db = Database::new(&paths.db()).await.expect("database");
    let recorder = Arc::new(Recorder::default());
    let runtimes = super::actor::ActorCtx::default_runtimes();
    let sup = Supervisor::with_event_sink(db, Arc::clone(&paths), runtimes, recorder.clone())
        .await
        .expect("supervisor");
    let fx = Fixture {
        sup: Arc::new(sup),
        paths,
        tmp,
    };
    (fx, recorder)
}

fn events_of(recorder: &Recorder, app: &str) -> Vec<LifecycleEvent> {
    recorder
        .events
        .lock()
        .iter()
        .filter(|e| &*e.app == app)
        .cloned()
        .collect()
}

fn actor_of(event: &LifecycleEvent) -> Option<&str> {
    event.actor.as_deref()
}

async fn as_actor<F: std::future::Future>(wire: &str, f: F) -> F::Output {
    CURRENT_ACTOR.scope(Arc::from(wire), f).await
}

/// A stop made under the actor `ai:test` yields a `Stopped` event whose actor is `ai:test`; a
/// start under `human:cli` yields a `Started` event with that actor; a call with no actor in
/// scope (a test calling the supervisor directly) yields none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_event_carries_actor_of_the_request() {
    let (fx, recorder) = recorded_fixture().await;
    let (spec, _m) = fx.sleeper("ut-actor");
    let id = as_actor("human:cli", fx.sup.start(spec))
        .await
        .expect("start");

    assert!(as_actor("ai:test", fx.sup.stop(id)).await.expect("stop"));

    let events = events_of(&recorder, "ut-actor");
    let started = events
        .iter()
        .find(|e| e.kind == LifecycleKind::Started)
        .expect("started event");
    assert_eq!(actor_of(started), Some("human:cli"), "{started:?}");
    let stopped = events
        .iter()
        .find(|e| e.kind == LifecycleKind::Stopped)
        .expect("stopped event");
    assert_eq!(actor_of(stopped), Some("ai:test"), "{stopped:?}");
    assert_eq!(stopped.cause, Cause::User);

    // no actor in scope: none recorded
    assert!(fx.sup.restart(id).await.expect("restart"));
    let events = events_of(&recorder, "ut-actor");
    let last = events
        .iter()
        .rev()
        .find(|e| e.kind == LifecycleKind::Started || e.kind == LifecycleKind::Restarted);
    assert_eq!(actor_of(last.expect("an event after the restart")), None);
    fx.sup.stop(id).await.expect("stop");
}

/// A crash restart is the daemon's own work: its events say `system:crash`, not the actor that
/// started the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_restart_event_actor_is_system() {
    let (fx, recorder) = recorded_fixture().await;
    let m = fx.marker();
    let mut spec = fx.cmd("ut-sys", &format!("exec sleep {m}"));
    spec.restart_policy.restart_delay_ms = 20;
    as_actor("ai:test", fx.sup.start(spec))
        .await
        .expect("start");

    let first = fx.state("ut-sys").await.pid.expect("pid");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(first as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill");
    fx.wait_for("ut-sys", "the automatic restart", |s| {
        s.status == AppStatus::Running && s.pid.map(|p| p != first).unwrap_or(false)
    })
    .await;

    let events = events_of(&recorder, "ut-sys");
    let started = events
        .iter()
        .find(|e| e.kind == LifecycleKind::Started)
        .expect("started");
    assert_eq!(actor_of(started), Some("ai:test"));
    let crashed = events
        .iter()
        .find(|e| e.kind == LifecycleKind::Crashed)
        .expect("crashed");
    assert_eq!(
        (crashed.cause, actor_of(crashed)),
        (Cause::Crash, Some("system:crash")),
        "{crashed:?}"
    );
    let restarted = events
        .iter()
        .find(|e| e.kind == LifecycleKind::Restarted)
        .expect("restarted");
    assert_eq!(
        (restarted.cause, actor_of(restarted)),
        (Cause::Crash, Some("system:crash")),
        "{restarted:?}"
    );

    let id = fx.state("ut-sys").await.app_id;
    fx.sup.stop(id).await.expect("stop");
}

/// Work that crosses a task boundary keeps the actor: the instances of a cluster are started,
/// restarted and stopped by the group's own task, and a stop of several apps runs its layers
/// concurrently; every event still names the requester.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_spawned_by_the_facade_keeps_the_actor() {
    let (fx, recorder) = recorded_fixture().await;
    let m = fx.marker();
    let mut spec = fx.cmd("ut-tk", &format!("exec sleep {m}"));
    spec.instances = 2;
    let parent = as_actor("ai:starter", fx.sup.start(spec))
        .await
        .expect("start cluster");

    for child in ["ut-tk-0", "ut-tk-1"] {
        let started: Vec<_> = events_of(&recorder, child)
            .into_iter()
            .filter(|e| e.kind == LifecycleKind::Started)
            .collect();
        assert_eq!(started.len(), 1, "{child}");
        assert_eq!(
            actor_of(&started[0]),
            Some("ai:starter"),
            "{child}: {:?}",
            started[0]
        );
    }

    as_actor("human:ops", fx.sup.restart(parent))
        .await
        .expect("restart cluster");
    for child in ["ut-tk-0", "ut-tk-1"] {
        let events = events_of(&recorder, child);
        let restarted = events
            .iter()
            .rev()
            .find(|e| e.kind == LifecycleKind::Restarted)
            .expect("restarted");
        assert_eq!(
            actor_of(restarted),
            Some("human:ops"),
            "{child}: {restarted:?}"
        );
    }

    let ids = fx
        .sup
        .resolve_selector(&Selector::All)
        .await
        .expect("resolve");
    for (_, result) in as_actor("ai:stopper", fx.sup.stop_many(&ids)).await {
        result.expect("stop");
    }
    for child in ["ut-tk-0", "ut-tk-1"] {
        let events = events_of(&recorder, child);
        let stopped = events
            .iter()
            .rev()
            .find(|e| e.kind == LifecycleKind::Stopped)
            .expect("stopped");
        assert_eq!(
            actor_of(stopped),
            Some("ai:stopper"),
            "{child}: {stopped:?}"
        );
    }
}

/// The boot resurrect is the daemon's own work, whatever request is in scope around it.
#[tokio::test(flavor = "current_thread")]
async fn resurrect_context_says_system() {
    let ctx = as_actor("ai:test", async { CmdCtx::resurrect() }).await;
    assert_eq!(
        (ctx.actor.as_deref(), ctx.cause),
        (Some("system:resurrect"), Cause::Resurrect)
    );
    // and the context of a request outside any scope names nobody
    assert_eq!(CmdCtx::user().actor, None);
}

#[tokio::test]
async fn saved_cluster_refuses_a_standalone_instance_name() {
    let f = fixture().await;
    f.sup.import_spec(f.cmd("web-0", "true")).await.unwrap();
    let standalone = f.handle("web-0");
    let mut cluster = f.cmd("web", "true");
    cluster.instances = 2;

    let result = f.sup.import_spec(cluster).await;
    assert!(matches!(result, Err(Error::AppAlreadyExists(name)) if name == "web-0"));
    let records = f.sup.store().records().await.unwrap();
    assert!(
        matches!(&records[..], [AppRecord::Ok { spec, parent_id: None, .. }] if spec.id == standalone.id)
    );
    assert_eq!(f.handle("web-0").id, standalone.id);
    assert!(f.sup.registry.by_name("web").is_none());
}

#[tokio::test]
async fn shutdown_rolls_back_a_new_app_whose_first_start_is_pending() {
    let fake = Fake::default();
    let _release = fake.hold("pending");
    let f = fixture_with(Some(fake.clone())).await;
    let spec = f.cmd("pending", "true");
    let supervisor = Arc::clone(&f.sup);
    let start = tokio::spawn(async move { supervisor.start(spec).await });

    tokio::time::timeout(Duration::from_secs(5), async {
        while !fake.has_entered("pending") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.sup.store().records().await.unwrap().len(), 1);
    f.sup.shutdown(Duration::from_secs(2)).await;

    assert!(matches!(start.await.unwrap(), Err(Error::ShuttingDown)));
    assert!(f.sup.store().records().await.unwrap().is_empty());
}

#[tokio::test]
async fn replacing_a_child_updates_the_group_and_keeps_it_subscribed() {
    use super::handle::{HandleKind, ProcessHandle, Snapshot};

    let f = fixture().await;
    let mut cluster = f.cmd("web", "true");
    cluster.instances = 2;
    f.sup.import_spec(cluster).await.unwrap();
    let old = f.handle("web-0");

    let mut snapshot = (*old.snapshot()).clone();
    snapshot.state.status = AppStatus::Errored;
    let (current, parts) = ProcessHandle::channel(old.id, HandleKind::Process, snapshot);
    f.sup.registry.replace(current);
    old.abort();
    f.wait_for("web", "replacement child snapshot", |state| {
        state.status == AppStatus::Unhealthy
    })
    .await;

    let mut snapshot: Snapshot = (**parts.snap_tx.borrow()).clone();
    snapshot.state.status = AppStatus::Stopped;
    parts.snap_tx.send_replace(Arc::new(snapshot));
    f.wait_for("web", "updated child snapshot", |state| {
        state.status == AppStatus::Stopped
    })
    .await;
}

#[tokio::test]
async fn log_replies_keep_a_bounded_tail_from_each_stream() {
    let f = fixture().await;
    f.paths.ensure_dirs().unwrap();
    f.sup.import_spec(f.cmd("web", "true")).await.unwrap();
    for stream in ["out", "err"] {
        let line = format!("[2026-01-01] {}\n", "x".repeat(1024));
        std::fs::write(
            f.paths.log_path("web", stream),
            line.repeat(1700) + stream + "\n",
        )
        .unwrap();
    }
    let lines = f
        .sup
        .logs(&Selector::ByName("web".into()), 100_000, true, true)
        .await
        .unwrap();
    assert!(lines.iter().map(|line| line.len() + 1).sum::<usize>() <= 2 * 1024 * 1024);
    assert!(lines.iter().any(|line| line == "out"));
    assert!(lines.iter().any(|line| line == "err"));
}
