//! Test runtime adapters, missing-adapter errors and persisted launch plans in private homes.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::daemon::DaemonProc;
use common::fixture::{install, pid_alive, InstallSpec, Installed};
use common::*;
use oxidepm_core::{AppSpec, AppStatus, Runtime};
use serde_json::{json, Value};
use sqlx::Row;
use tempfile::TempDir;

/// `(name, mode column, spec_json)` of every row of `state.db`, read while no daemon writes.
async fn rows(h: &Harness) -> Vec<(String, String, String)> {
    let db = oxidepm_db::Database::new(&h.paths.db())
        .await
        .expect("open the database");
    let rows = sqlx::query("SELECT name, mode, spec_json FROM apps ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("read the rows");
    db.close().await;
    rows.iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

/// The stored bytes of a spec still use the key `mode` and the old runtime strings: a database
/// written before the `Runtime` type is read by it, and one written by it is read by an older
/// build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_json_still_uses_the_mode_key() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("sj-app", &format!("exec sleep {m}")))
        .await
        .expect("start");
    h.shutdown().await;

    let stored = rows(&h).await;
    assert_eq!(stored.len(), 1);
    let (name, column, json) = &stored[0];
    assert_eq!(name, "sj-app");
    assert_eq!(column, "cmd", "the column keeps the old string");
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(value["mode"], "cmd");
    assert!(value.get("runtime").is_none(), "no second key: {json}");
    assert!(
        json.contains("\"mode\":\"cmd\""),
        "the bytes are the old ones: {json}"
    );

    // a row written by the build before the type change (same bytes) loads, and the app is back
    h.start_daemon().await;
    let info = h
        .status()
        .await
        .into_iter()
        .find(|i| i.spec.name == "sj-app")
        .expect("loaded");
    assert_eq!(info.spec.mode.as_str(), "cmd");
    assert_eq!(
        h.sup.resurrect_report().await.expect("resurrect").started,
        1
    );
    h.pid_of("sj-app").await;
    h.stop("sj-app").await.expect("stop");
}

/// A stored spec that is not JSON stays exactly as it is: no migration touches it, the daemon
/// boots, and the other apps come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_leaves_unparsable_spec_rows_alone() {
    let mut h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("mg-good", &format!("exec sleep {m}")))
        .await
        .expect("start good");
    h.start(h.cmd("mg-bad", "exec sleep 306.2"))
        .await
        .expect("start bad");
    h.shutdown().await;

    let db = oxidepm_db::Database::new(&h.paths.db())
        .await
        .expect("open the database");
    sqlx::query("UPDATE apps SET spec_json = '{ not json' WHERE name = 'mg-bad'")
        .execute(db.pool())
        .await
        .expect("corrupt the row");
    db.close().await;
    std::fs::remove_file(h.paths.saved()).expect("remove saved.json");

    h.start_daemon().await;
    assert_eq!(h.state("mg-bad").await.status, AppStatus::Errored);
    assert_eq!(
        h.sup
            .resurrect_report()
            .await
            .expect("the boot goes on")
            .started,
        1
    );
    h.pid_of("mg-good").await;
    h.shutdown().await;

    let bad = rows(&h)
        .await
        .into_iter()
        .find(|r| r.0 == "mg-bad")
        .expect("the row is still there");
    assert_eq!(bad.2, "{ not json", "the bytes were not rewritten");
    assert_eq!(bad.1, "cmd");
}

// ---- the runtime adapter -------------------------------------------------------------

/// The runtime the fixture adapter provides. No built-in runner has this name.
const RT: &str = "fixrt";
const ADAPTER: &str = "runtime-fixture";

static MARKERS: AtomicU32 = AtomicU32::new(0);

/// A `sleep` duration that nothing else on the machine uses.
fn marker() -> String {
    format!(
        "312.{:05}{:02}",
        std::process::id() % 100_000,
        MARKERS.fetch_add(1, Ordering::SeqCst) % 100
    )
}

/// A daemon with the fixture installed as the provider of `fixrt`, its script, and a work
/// directory. Apps still running are stopped when it is dropped.
struct Rt {
    _tmp: TempDir,
    /// `None` only while a reboot replaces it.
    daemon_slot: Option<DaemonProc>,
    inst: Installed,
    home: PathBuf,
    work: PathBuf,
    env: Vec<(String, String)>,
}

fn runtime_rig(
    script: impl FnOnce(&Path) -> Value,
    programs: &[&str],
    env_set: &[&str],
    env: &[(&str, &str)],
) -> Rt {
    runtime_rig_with(script, programs, env_set, env, "")
}

/// [`runtime_rig`] with the text of the daemon's `daemon.toml`.
fn runtime_rig_with(
    script: impl FnOnce(&Path) -> Value,
    programs: &[&str],
    env_set: &[&str],
    env: &[(&str, &str)],
    daemon_toml: &str,
) -> Rt {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = std::fs::canonicalize(work).unwrap();
    let provides = json!({ "runtimes": [RT], "programs": programs, "env_set": env_set, "capture_programs": [] });
    let rt = tokio::runtime::Runtime::new().unwrap();
    let inst = rt.block_on(install(
        &home,
        InstallSpec {
            provides,
            ..InstallSpec::tool(ADAPTER, script(&work))
        },
    ));
    let mut all: Vec<(String, String)> = inst.env();
    all.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if !daemon_toml.is_empty() {
        std::fs::write(home.join("daemon.toml"), daemon_toml).unwrap();
    }
    let daemon = DaemonProc::spawn_with_env(&home, &all);
    let env = all
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Rt {
        _tmp: tmp,
        daemon_slot: Some(daemon),
        inst,
        home,
        work,
        env,
    }
}

impl Drop for Rt {
    fn drop(&mut self) {
        if let Some(daemon) = &self.daemon_slot {
            let _ = daemon.try_rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
        }
    }
}

impl Rt {
    fn spec(&self, name: &str, extra: Value) -> Value {
        let spec = AppSpec::new(
            name.into(),
            RT.parse::<Runtime>().unwrap(),
            "app.fx".into(),
            self.work.clone(),
        );
        let mut value = serde_json::to_value(spec).unwrap();
        for (k, v) in extra.as_object().unwrap() {
            value[k.as_str()] = v.clone();
        }
        value
    }

    fn start(&self, name: &str, extra: Value) -> Value {
        self.daemon()
            .rpc(json!({ "type": "start", "spec": self.spec(name, extra) }))
    }

    fn apps(&self) -> Vec<Value> {
        self.daemon().rpc(json!({ "type": "status" }))["apps"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn app(&self, name: &str) -> Option<Value> {
        self.apps().into_iter().find(|a| a["spec"]["name"] == name)
    }

    fn status_of(&self, name: &str) -> Option<String> {
        self.app(name)
            .and_then(|a| a["state"]["status"].as_str().map(str::to_string))
    }

    fn pid_of(&self, name: &str) -> Option<u32> {
        self.app(name)
            .and_then(|a| a["state"]["pid"].as_u64())
            .map(|p| p as u32)
    }

    /// Requests of the daemon the fixture received, by method.
    fn calls(&self, method: &str) -> Vec<Value> {
        self.inst
            .report()
            .into_iter()
            .filter(|r| r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == method)
            .collect()
    }

    /// Stop the daemon the way a reboot does (SIGTERM: the apps are stopped, the desired state
    /// stays) and start it again on the same home. The boot brings the apps back.
    fn reboot(&mut self) {
        self.shutdown_daemon();
        self.boot_daemon();
    }

    fn daemon(&self) -> &DaemonProc {
        self.daemon_slot.as_ref().expect("the daemon is up")
    }

    /// SIGTERM, and wait for the exit. The old handle is dropped here, before a new daemon
    /// exists: dropping a `DaemonProc` kills every process whose directory is under the test's
    /// temporary directory, and that must not reach the next daemon's apps.
    fn shutdown_daemon(&mut self) {
        let mut old = self.daemon_slot.take().expect("the daemon is up");
        old.signal(common::daemon::Signal::SIGTERM);
        old.wait_exit();
    }

    fn boot_daemon(&mut self) {
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.daemon_slot = Some(DaemonProc::spawn_with_env(&self.home, &env));
    }

    /// Run `sql` on the app database while the daemon is down.
    fn sql(&self, db: &str, sql: &str) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let url = format!("sqlite://{}", self.home.join(db).display());
            let pool = sqlx::SqlitePool::connect(&url)
                .await
                .expect("open the database");
            sqlx::query(sql)
                .execute(&pool)
                .await
                .expect("run the statement");
            pool.close().await;
        });
    }

    /// `(app, adapter)` of every stored launch plan, read from `adapters.db` (the daemon may be up).
    fn plan_rows(&self) -> Vec<(String, String)> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(self.home.join("adapters.db"))
                .read_only(true);
            let pool = sqlx::SqlitePool::connect_with(options)
                .await
                .expect("open adapters.db");
            let rows = sqlx::query("SELECT app_name, adapter FROM plans ORDER BY app_name")
                .fetch_all(&pool)
                .await
                .expect("read the plans");
            pool.close().await;
            rows.iter().map(|r| (r.get(0), r.get(1))).collect()
        })
    }

    /// All the text of the audit log files.
    fn audit_text(&self) -> String {
        let mut text = String::new();
        if let Ok(dir) = std::fs::read_dir(self.home.join("audit")) {
            for entry in dir.flatten() {
                text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        text
    }

    /// Give the fixture a new script (a tool process that is already running keeps its own).
    fn set_script(&self, script: Value) {
        std::fs::write(&self.inst.script_path, script.to_string()).unwrap();
    }

    fn stop(&self, name: &str) -> Value {
        self.daemon()
            .rpc(json!({ "type": "stop", "selector": { "by": "name", "value": name } }))
    }
}

fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn code_of(reply: &Value) -> String {
    assert_eq!(reply["type"], "error", "expected an error, got {reply}");
    reply["code"].as_str().unwrap_or("").to_string()
}

/// A script whose `runtime.plan` answers with the given plans, one per call, in order (the last
/// one for every call after that). `runtime.prepare` returns no steps.
fn script_plans(plans: Vec<Value>) -> Value {
    let mut rules = vec![
        json!({ "on": "runtime.prepare", "do": { "reply": { "result": { "steps": [], "notes": [] } } } }),
    ];
    let last = plans.len() - 1;
    for (i, plan) in plans.into_iter().enumerate() {
        let mut rule = json!({ "on": "runtime.plan", "do": { "reply": { "result": plan } } });
        if i < last {
            rule["times"] = json!(1);
        }
        rules.push(rule);
    }
    Value::Array(rules)
}

/// A script: `runtime.prepare` returns `steps`, `runtime.plan` returns `plan`.
fn script(steps: Value, plan: Value) -> Value {
    json!([
        { "on": "runtime.prepare", "do": { "reply": { "result": { "steps": steps, "notes": [] } } } },
        { "on": "runtime.plan", "do": { "reply": { "result": plan } } }
    ])
}

fn step(id: &str, argv: Value, timeout: u64) -> Value {
    json!({ "id": id, "argv": argv, "timeout_secs": timeout, "capture": "none", "description": format!("step {id}") })
}

fn sleep_plan(work: &Path, marker: &str) -> Value {
    json!({ "program": "sleep", "args": [marker], "cwd": work.display().to_string() })
}

fn cmdline_of(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

#[test]
fn node_app_runs_through_the_runtime_adapter() {
    let m = marker();
    let rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[],
    );
    let reply = rig.start(
        "fx-app",
        json!({ "env": { "SECRET_TOKEN": "hunter2", "PATH": "bin:/usr/bin:/bin" } }),
    );
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = rig.pid_of("fx-app").expect("the app runs");
    assert!(
        wait_for(10, || cmdline_of(pid)
            .trim_end()
            .ends_with(&format!("sleep {m}"))),
        "{}",
        cmdline_of(pid)
    );
    // the child is the core's: it has no connection to the adapter's process
    let adapter_pids = rig.inst.start_pids();
    assert_eq!(adapter_pids.len(), 1);
    assert_ne!(pid, adapter_pids[0]);

    let prepare = rig.calls("runtime.prepare");
    let plan = rig.calls("runtime.plan");
    assert_eq!((prepare.len(), plan.len()), (1, 1));
    let view = &prepare[0]["frame"]["params"]["spec"];
    assert_eq!(view["runtime"], RT);
    assert_eq!(
        view["env"]["SECRET_TOKEN"], "<redacted>",
        "the adapter sees the key, not the value"
    );
    assert!(
        !prepare[0].to_string().contains("hunter2"),
        "the value never reaches the adapter"
    );
    assert_eq!(prepare[0]["frame"]["params"]["phase"], "start");
    let hint = prepare[0]["frame"]["params"]["path_hint"].as_str().unwrap();
    assert_eq!(
        std::env::split_paths(hint).next(),
        Some(rig.work.join("bin"))
    );
    assert_eq!(rig.stop("fx-app")["type"], "stopped");
}

#[test]
fn adapter_steps_run_in_the_core_in_their_own_group_and_stop_kills_them() {
    let m = marker();
    let rig = runtime_rig(
        |work| {
            let pidfile = work.join("build-child.pid");
            let build = format!("sleep {m} & echo $! > '{}'; wait", pidfile.display());
            script(
                json!([step("build", json!(["sh", "-c", build]), 120)]),
                sleep_plan(work, &m),
            )
        },
        &["sleep", "sh"],
        &[],
        &[],
    );
    let pidfile = rig.work.join("build-child.pid");
    std::thread::scope(|scope| {
        let starter = scope.spawn(|| rig.start("fx-build", json!({})));
        assert!(
            wait_for(20, || pidfile.exists()
                && rig.status_of("fx-build").as_deref() == Some("building")),
            "the build runs"
        );
        let child: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid_alive(child));
        // the core runs the step: the child is not a descendant of the adapter
        assert!(!rig.inst.start_pids().contains(&child));
        let began = Instant::now();
        let stopped = rig.stop("fx-build");
        assert_eq!(stopped["type"], "stopped", "{stopped}");
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(
            wait_for(10, || !pid_alive(child)),
            "the build's child is dead after stop"
        );
        // the start that was waiting for the build is answered, as cancelled
        let cancelled = starter.join().expect("the starting thread");
        assert_eq!(code_of(&cancelled), "E_CANCELLED", "{cancelled}");
        assert_eq!(
            rig.status_of("fx-build").as_deref(),
            Some("stopped"),
            "the cancelled start leaves the app stopped"
        );
    });
    assert_eq!(
        rig.calls("runtime.plan").len(),
        0,
        "a stopped build never reaches the plan"
    );

    // a start after that runs one build, not two
    std::thread::scope(|scope| {
        let _ = std::fs::remove_file(&pidfile);
        let starter = scope.spawn(|| rig.start("fx-build", json!({})));
        assert!(
            wait_for(20, || pidfile.exists()
                && rig.status_of("fx-build").as_deref() == Some("building")),
            "the build runs again"
        );
        std::thread::sleep(Duration::from_millis(300));
        let sleeping = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|pid| cmdline_of(pid).trim() == format!("sleep {m}"))
            })
            .count();
        assert_eq!(sleeping, 1, "one build at a time");
        assert_eq!(rig.stop("fx-build")["type"], "stopped");
        let _ = starter.join();
    });
}

#[test]
fn step_env_includes_spec_env_but_not_ld_preload() {
    let m = marker();
    let rig = runtime_rig(
        |work| {
            let seen = work.join("seen.txt");
            let body = format!(
                "printf '%s|%s' \"$APP_VAR\" \"$LD_PRELOAD\" > '{}'",
                seen.display()
            );
            let mut rules = script(
                json!([step("env", json!(["sh", "-c", body]), 30)]),
                sleep_plan(work, &m),
            );
            // the second launch gets a plan that sets LD_PRELOAD
            let evil = json!({ "program": "sleep", "args": [m.clone()], "cwd": work.display().to_string(), "extra_env": { "LD_PRELOAD": "/tmp/evil.so" } });
            rules[1]["times"] = json!(1);
            rules
                .as_array_mut()
                .unwrap()
                .push(json!({ "on": "runtime.plan", "do": { "reply": { "result": evil } } }));
            rules
        },
        &["sleep", "sh"],
        &["EXTRA_FLAG"],
        &[("LD_PRELOAD", "/nonexistent/daemon-preload.so")],
    );
    let reply = rig.start("fx-env", json!({ "env": { "APP_VAR": "from-spec" } }));
    assert_eq!(reply["type"], "started", "{reply}");
    // the app's own env reaches the step; the daemon's LD_PRELOAD does not (the base set only)
    assert_eq!(
        std::fs::read_to_string(rig.work.join("seen.txt")).unwrap(),
        "from-spec|"
    );
    assert_eq!(rig.stop("fx-env")["type"], "stopped");

    // a plan that sets LD_PRELOAD is refused, and nothing starts
    let reply = rig.start("fx-env2", json!({}));
    assert_eq!(code_of(&reply), "E_ADAPTER_INVALID", "{reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("LD_PRELOAD"),
        "{reply}"
    );
    assert!(
        rig.app("fx-env2").is_none(),
        "no row for an app that never started"
    );
}

#[test]
fn build_output_is_capped_and_tail_kept_for_the_error() {
    let m = marker();
    let rig = runtime_rig(
        |work| {
            // about 30 MB of output, then a marker and a failure
            let body = "i=0; while [ $i -lt 300 ]; do head -c 100000 /dev/zero | tr '\\\\0' x; i=$((i+1)); done; echo END-OF-BUILD-MARKER; exit 3";
            script(
                json!([step("noisy", json!(["sh", "-c", body]), 120)]),
                sleep_plan(work, &m),
            )
        },
        &["sleep", "sh"],
        &[],
        &[],
    );
    let reply = rig.start("fx-noisy", json!({}));
    assert_eq!(code_of(&reply), "E_BUILD_FAILED", "{reply}");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("END-OF-BUILD-MARKER"),
        "the tail is kept for the error"
    );
    assert!(
        message.len() < 70 * 1024,
        "the error text is bounded: {} bytes",
        message.len()
    );
    let log = rig.home.join("logs").join("fx-noisy-build.log");
    let size = std::fs::metadata(&log).expect("the build log").len();
    assert!(
        (10 * 1024 * 1024..=10 * 1024 * 1024 + 256).contains(&size),
        "the log is capped at 10 MiB: {size}"
    );
    assert!(rig.app("fx-noisy").is_none());
}

#[test]
fn build_timeout_kills_the_step() {
    let m = marker();
    let rig = runtime_rig(
        |work| {
            script(
                json!([step("slow", json!(["sleep", m]), 1)]),
                sleep_plan(work, &m),
            )
        },
        &["sleep"],
        &[],
        &[],
    );
    let began = Instant::now();
    let reply = rig.start("fx-slow", json!({}));
    assert_eq!(code_of(&reply), "E_BUILD_FAILED", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("killed after 1 s"),
        "{reply}"
    );
    assert!(began.elapsed() < Duration::from_secs(15));
    let sleeping = |m: &str| {
        std::fs::read_dir("/proc").unwrap().flatten().any(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|pid| cmdline_of(pid).trim() == format!("sleep {m}"))
        })
    };
    assert!(wait_for(10, || !sleeping(&m)), "the step's process is gone");
}

#[test]
fn plan_with_relative_program_or_nul_or_huge_args_is_invalid() {
    let m = marker();
    let rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[],
    );
    let work = rig.work.display().to_string();
    let cases = [
        (
            "relative",
            json!({ "program": "./sleep", "args": [m], "cwd": work }),
        ),
        (
            "nul",
            json!({ "program": "sleep", "args": ["a\u{0}b"], "cwd": work }),
        ),
        (
            "huge",
            json!({ "program": "sleep", "args": vec!["x"; 5000], "cwd": work }),
        ),
        (
            "unlisted",
            json!({ "program": "env", "args": [], "cwd": work }),
        ),
    ];
    let names: Vec<&str> = cases.iter().map(|c| c.0).collect();
    rig.set_script(script_plans(cases.iter().map(|c| c.1.clone()).collect()));
    // the tool is not running yet: the first call starts it with this script
    for (n, what) in names.iter().enumerate() {
        let reply = rig.start(&format!("fx-bad-{n}"), json!({}));
        assert_eq!(code_of(&reply), "E_ADAPTER_INVALID", "{what}: {reply}");
        assert!(rig.app(&format!("fx-bad-{n}")).is_none(), "{what}: no row");
    }
}

#[test]
fn crash_restart_does_not_call_the_adapter() {
    let rig = runtime_rig(
        |work| {
            let plan = json!({ "program": "sh", "args": ["-c", "sleep 0.3; exit 1"], "cwd": work.display().to_string() });
            script(json!([]), plan)
        },
        &["sh"],
        &[],
        &[],
    );
    let reply = rig.start("fx-crash", json!({ "restart_policy": { "auto_restart": true, "max_restarts": 20, "restart_delay_ms": 100, "crash_window_secs": 600, "max_restart_delay_ms": 200, "stable_uptime_secs": 60 } }));
    assert_eq!(reply["type"], "started", "{reply}");
    let restarts = |rig: &Rt| {
        rig.app("fx-crash")
            .and_then(|a| a["state"]["restarts"].as_u64())
            .unwrap_or(0)
    };
    assert!(
        wait_for(30, || restarts(&rig) >= 3),
        "the app was relaunched by the daemon three times"
    );
    // two calls in all: one prepare and one plan, for the one operator launch
    assert_eq!(rig.calls("runtime.prepare").len(), 1);
    assert_eq!(rig.calls("runtime.plan").len(), 1);
    assert_eq!(rig.stop("fx-crash")["type"], "stopped");
}

/// A script whose `runtime.prepare` hangs from the second call on.
fn hang_after_first(work: &Path, m: &str) -> Value {
    json!([
        { "on": "runtime.prepare", "times": 1, "do": { "reply": { "result": { "steps": [], "notes": [] } } } },
        { "on": "runtime.prepare", "do": "hang" },
        { "on": "runtime.plan", "do": { "reply": { "result": sleep_plan(work, m) } } }
    ])
}

/// Runtime calls end after 1.5 s in these tests (the real deadline is 10 s).
const FAST: (&str, &str) = ("OXIDEPM_TEST_RUNTIME_CALL_MS", "1500");

#[test]
fn hung_runtime_call_does_not_block_the_actor() {
    let rig = runtime_rig(
        |work| json!([{ "on": "runtime.prepare", "do": "hang" }, { "on": "runtime.plan", "do": { "reply": { "result": sleep_plan(work, "312.0") } } }]),
        &["sleep"],
        &[],
        &[("OXIDEPM_TEST_RUNTIME_CALL_MS", "8000")],
    );
    std::thread::scope(|scope| {
        let starter = scope.spawn(|| rig.start("fx-hung", json!({})));
        assert!(
            wait_for(10, || rig.status_of("fx-hung").as_deref()
                == Some("building")),
            "the app is building"
        );
        let began = Instant::now();
        assert!(rig.status_of("fx-hung").is_some(), "status answers");
        let stopped = rig.stop("fx-hung");
        assert_eq!(stopped["type"], "stopped", "{stopped}");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "stop took {:?}",
            began.elapsed()
        );
        assert_eq!(rig.status_of("fx-hung").as_deref(), Some("stopped"));
        let _ = starter.join();
    });
}

#[test]
fn hung_runtime_adapter_does_not_delay_cmd_launches() {
    let rig = runtime_rig_with(
        |work| json!([{ "on": "runtime.prepare", "do": "hang" }, { "on": "runtime.plan", "do": { "reply": { "result": sleep_plan(work, "312.1") } } }]),
        &["sleep"],
        &[],
        &[FAST],
        "launch_concurrency = 2\n",
    );
    let codes = std::thread::scope(|scope| {
        let starters: Vec<_> = (0..20)
            .map(|i| {
                scope.spawn({
                    let rig = &rig;
                    move || rig.start(&format!("fx-h{i}"), json!({}))
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(300));
        let began = Instant::now();
        let spec = AppSpec::new(
            "plain-cmd".into(),
            Runtime::CMD,
            "sleep".into(),
            rig.work.clone(),
        )
        .with_args(vec![marker()]);
        let reply = rig.daemon().rpc(json!({ "type": "start", "spec": spec }));
        assert_eq!(reply["type"], "started", "{reply}");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "a cmd app waited {:?} behind hung runtime calls",
            began.elapsed()
        );
        starters
            .into_iter()
            .map(|s| code_of(&s.join().unwrap()))
            .collect::<Vec<_>>()
    });
    for code in &codes {
        assert!(
            code == "E_ADAPTER_TIMEOUT" || code == "E_ADAPTER_UNAVAILABLE",
            "{codes:?}"
        );
    }
    // the circuit is open now: the next app fails at once
    let began = Instant::now();
    let reply = rig.start("fx-after", json!({}));
    assert_eq!(code_of(&reply), "E_ADAPTER_UNAVAILABLE", "{reply}");
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.stop("plain-cmd")["type"], "stopped");
}

#[test]
fn cmd_apps_are_not_failed_by_a_runtime_outage() {
    let rig = runtime_rig(
        |work| json!([{ "on": "runtime.prepare", "do": "hang" }, { "on": "runtime.plan", "do": { "reply": { "result": sleep_plan(work, "312.2") } } }]),
        &["sleep"],
        &[],
        &[FAST],
    );
    std::thread::scope(|scope| {
        let hung = scope.spawn(|| rig.start("fx-down", json!({})));
        std::thread::sleep(Duration::from_millis(200));
        let sleeper = |name: &str, depends: Vec<&str>| {
            let mut spec =
                AppSpec::new(name.into(), Runtime::CMD, "sleep".into(), rig.work.clone())
                    .with_args(vec![marker()]);
            spec.depends_on = depends.into_iter().map(String::from).collect();
            spec
        };
        let reply = rig
            .daemon()
            .rpc(json!({ "type": "start", "spec": sleeper("plain-db", vec![]) }));
        assert_eq!(reply["type"], "started", "{reply}");
        let reply = rig
            .daemon()
            .rpc(json!({ "type": "start", "spec": sleeper("plain-web", vec!["plain-db"]) }));
        assert_eq!(reply["type"], "started", "{reply}");
        assert!(wait_for(10, || rig.status_of("plain-web").as_deref()
            == Some("running")));
        assert_eq!(rig.status_of("plain-db").as_deref(), Some("running"));
        let _ = hung.join();
    });
    rig.stop("plain-web");
    rig.stop("plain-db");
}

#[test]
fn restart_with_dead_runtime_adapter_leaves_the_running_app_up() {
    let m = marker();
    let rig = runtime_rig(|work| hang_after_first(work, &m), &["sleep"], &[], &[FAST]);
    let reply = rig.start("fx-up", json!({}));
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = rig.pid_of("fx-up").unwrap();

    // two apps whose launch hangs open the circuit
    std::thread::scope(|scope| {
        let a = scope.spawn(|| rig.start("fx-b1", json!({})));
        let b = scope.spawn(|| rig.start("fx-b2", json!({})));
        let _ = (a.join(), b.join());
    });
    let restart = rig
        .daemon()
        .rpc(json!({ "type": "restart", "selector": { "by": "name", "value": "fx-up" } }));
    assert_eq!(
        restart["items"][0]["result"]["code"], "E_ADAPTER_UNAVAILABLE",
        "{restart}"
    );
    assert_eq!(
        rig.pid_of("fx-up"),
        Some(pid),
        "the same process is still running"
    );
    assert!(pid_alive(pid));
    assert_eq!(rig.status_of("fx-up").as_deref(), Some("running"));
    assert_eq!(rig.stop("fx-up")["type"], "stopped");
}

// ---- missing adapters (30b) ------------------------------------------------------------------

/// A daemon with no adapter installed: it serves `cmd` and nothing else.
fn bare_daemon() -> (TempDir, DaemonProc, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = std::fs::canonicalize(work).unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    (tmp, daemon, work)
}

fn app_of(runtime: &str, name: &str, command: &str, work: &Path) -> Value {
    let spec = AppSpec::new(
        name.into(),
        runtime.parse::<Runtime>().unwrap(),
        command.into(),
        work.to_path_buf(),
    );
    serde_json::to_value(spec).unwrap()
}

fn status_apps(daemon: &DaemonProc) -> Vec<Value> {
    daemon.rpc(json!({ "type": "status" }))["apps"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn missing_runtime_adapter_start_fails_with_the_named_adapter() {
    let (_tmp, daemon, work) = bare_daemon();
    let reply =
        daemon.rpc(json!({ "type": "start", "spec": app_of("node", "mr-web", "app.js", &work) }));
    assert_eq!(code_of(&reply), "E_ADAPTER_MISSING", "{reply}");
    assert_eq!(
        reply["message"],
        "runtime 'node' is provided by the adapter 'runtime-node', which is not enabled. Run: oxidepm adapter import runtime-node --enable"
    );
    assert!(status_apps(&daemon).is_empty(), "nothing was created");

    // a name nobody knows
    let reply = daemon.rpc(json!({ "type": "start", "spec": app_of("zzz", "mr-odd", "x", &work) }));
    assert_eq!(code_of(&reply), "E_ADAPTER_MISSING", "{reply}");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.starts_with("no adapter provides runtime 'zzz'; installed runtimes: cmd"),
        "{message}"
    );
    assert!(status_apps(&daemon).is_empty());

    // a dry run stops at the same place
    let dry = daemon.rpc_dry_on(
        &daemon.socket(),
        "dry",
        None,
        json!({ "type": "start", "spec": app_of("node", "mr-dry", "app.js", &work) }),
    );
    assert!(dry.to_string().contains("E_ADAPTER_MISSING"), "{dry}");
}

#[test]
fn runtimes_that_come_from_adapters_need_them_in_an_ordinary_daemon() {
    // no test hook: the daemon as it is shipped has no runner for these
    let tmp = TempDir::new().unwrap();
    let work = std::fs::canonicalize({
        std::fs::create_dir_all(tmp.path().join("work")).unwrap();
        tmp.path().join("work")
    })
    .unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    for (runtime, adapter) in [
        ("node", "runtime-node"),
        ("npm", "runtime-node"),
        ("pnpm", "runtime-node"),
        ("yarn", "runtime-node"),
        ("python", "runtime-python"),
        ("py", "runtime-python"),
        ("go", "runtime-go"),
        ("golang", "runtime-go"),
    ] {
        let reply = daemon
            .rpc(json!({ "type": "start", "spec": app_of(runtime, "nd-app", "app.js", &work) }));
        assert_eq!(code_of(&reply), "E_ADAPTER_MISSING", "{runtime}: {reply}");
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .ends_with(&format!("Run: oxidepm adapter import {adapter} --enable")),
            "{runtime}: {reply}"
        );
    }
    assert!(status_apps(&daemon).is_empty(), "nothing was created");
}

#[test]
fn hints_table_names_the_adapter() {
    let (_tmp, daemon, work) = bare_daemon();
    for (runtime, adapter) in oxidepm_core::runtime_hints::RUNTIME_ADAPTER {
        let reply =
            daemon.rpc(json!({ "type": "start", "spec": app_of(runtime, "hint-app", "x", &work) }));
        assert_eq!(code_of(&reply), "E_ADAPTER_MISSING", "{runtime}: {reply}");
        let message = reply["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("the adapter '{adapter}'")),
            "{runtime}: {message}"
        );
        assert!(
            message.ends_with(&format!("Run: oxidepm adapter import {adapter} --enable")),
            "{runtime}: {message}"
        );
    }
}

#[test]
fn missing_runtime_in_a_config_file_fails_that_app_only() {
    let (_tmp, daemon, work) = bare_daemon();
    let marker = marker();
    let plain = |name: &str| {
        let spec = AppSpec::new(name.into(), Runtime::CMD, "sleep".into(), work.clone())
            .with_args(vec![marker.clone()]);
        serde_json::to_value(spec).unwrap()
    };
    let reply = daemon.rpc(json!({
        "type": "start_batch",
        "specs": [plain("cf-one"), app_of("node", "cf-node", "app.js", &work), plain("cf-two")]
    }));
    assert_eq!(reply["type"], "started_batch", "{reply}");
    let items = reply["items"].as_array().unwrap();
    assert_eq!(items[0]["result"]["outcome"], "ok", "{reply}");
    assert_eq!(items[1]["result"]["code"], "E_ADAPTER_MISSING", "{reply}");
    assert_eq!(items[2]["result"]["outcome"], "ok", "{reply}");
    let names: Vec<String> = status_apps(&daemon)
        .iter()
        .map(|a| a["spec"]["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"cf-one".to_string())
            && names.contains(&"cf-two".to_string())
            && !names.contains(&"cf-node".to_string()),
        "{names:?}"
    );
    daemon.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
}

#[test]
fn cmd_runtime_never_needs_an_adapter() {
    let (_tmp, daemon, work) = bare_daemon();
    let spec =
        AppSpec::new("plain".into(), Runtime::CMD, "sleep".into(), work).with_args(vec![marker()]);
    let reply = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "started", "{reply}");
    daemon.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
}

#[test]
fn status_warns_about_apps_whose_runtime_adapter_is_not_enabled() {
    let m = marker();
    let rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[],
    );
    let reply = rig.start("warn-app", json!({}));
    assert_eq!(reply["type"], "started", "{reply}");
    let warnings = |rig: &Rt| -> Vec<String> {
        let reply = rig.daemon().rpc(json!({ "type": "daemon_status" }));
        reply["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        warnings(&rig).iter().all(|w| !w.contains("warn-app")),
        "{:?}",
        warnings(&rig)
    );

    let disabled = rig
        .daemon()
        .rpc(json!({ "type": "adapter", "op": "disable", "name": ADAPTER }));
    assert_eq!(disabled["type"], "adapter", "{disabled}");
    let pid = rig
        .pid_of("warn-app")
        .expect("the running app keeps running");
    assert!(pid_alive(pid));
    assert!(
        warnings(&rig).contains(
            &"app 'warn-app' uses runtime 'fixrt' whose adapter is not enabled".to_string()
        ),
        "{:?}",
        warnings(&rig)
    );
    // the next start says what to run
    let reply = rig.start("warn-two", json!({}));
    assert_eq!(code_of(&reply), "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .ends_with(&format!("Run: oxidepm adapter enable {ADAPTER}")),
        "{reply}"
    );
    assert_eq!(rig.stop("warn-app")["type"], "stopped");
}

// ---- the persisted plan (30c) ----------------------------------------------------------------

/// A script whose one step appends a line to `built.txt` in the work directory, and whose plan
/// is a sleep.
fn building_script(work: &Path, m: &str) -> Value {
    let built = work.join("built.txt");
    let body = format!("echo built >> '{}'", built.display());
    script(
        json!([step("build", json!(["sh", "-c", body]), 30)]),
        sleep_plan(work, m),
    )
}

fn builds(rig: &Rt) -> usize {
    std::fs::read_to_string(rig.work.join("built.txt"))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

/// The adapter does not answer `runtime.prepare` (the call ends at the test deadline).
fn hanging() -> Value {
    json!([{ "on": "runtime.prepare", "do": "hang" }])
}

fn wait_running(rig: &Rt, name: &str) -> bool {
    wait_for(30, || rig.status_of(name).as_deref() == Some("running"))
}

fn last_error(rig: &Rt, name: &str) -> String {
    rig.app(name)
        .and_then(|a| a["state"]["last_error"].as_str().map(str::to_string))
        .unwrap_or_default()
}

#[test]
fn stepless_stored_plan_is_used_first_and_the_adapter_is_not_called() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("sp-app", json!({}))["type"], "started");
    assert_eq!(
        (
            rig.calls("runtime.prepare").len(),
            rig.calls("runtime.plan").len()
        ),
        (1, 1)
    );
    rig.reboot();
    assert!(
        wait_running(&rig, "sp-app"),
        "the boot brought the app back"
    );
    let pid = rig.pid_of("sp-app").unwrap_or_else(|| {
        panic!(
            "no pid: {:?} stderr {}",
            rig.app("sp-app"),
            rig.daemon().stderr()
        )
    });
    assert!(
        wait_for(10, || cmdline_of(pid)
            .trim_end()
            .ends_with(&format!("sleep {m}"))),
        "{}",
        cmdline_of(pid)
    );
    assert_eq!(
        (
            rig.calls("runtime.prepare").len(),
            rig.calls("runtime.plan").len()
        ),
        (1, 1),
        "the adapter was not called at the boot"
    );
    assert!(
        !rig.audit_text().contains("stored_plan_used"),
        "a healthy boot has nothing to warn about"
    );
    rig.stop("sp-app");
}

#[test]
fn reboot_with_adapter_down_uses_the_persisted_plan_for_an_unchanged_spec() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| building_script(work, &m),
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("rb-app", json!({}))["type"], "started");
    assert_eq!(builds(&rig), 1);
    rig.set_script(hanging());
    rig.reboot();
    assert!(
        wait_running(&rig, "rb-app"),
        "the app runs on the plan stored by the earlier launch"
    );
    assert_eq!(builds(&rig), 1, "the build steps were skipped");
    assert_eq!(
        rig.calls("runtime.prepare").len(),
        2,
        "the adapter was asked first"
    );
    let audit = rig.audit_text();
    assert!(
        audit.contains("stored_plan_used"),
        "the audit log says so: {audit}"
    );
    let stderr = rig.daemon().stderr();
    assert!(
        stderr.contains("started from the launch plan it made earlier"),
        "the warning is logged"
    );
    rig.stop("rb-app");
}

#[test]
fn the_built_program_runs_and_can_be_reused_after_boot() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| {
            let artifact = work.join("app");
            let build = format!(
                "printf '#!/bin/sh\\nexec sleep \"$@\"\\n' > '{0}' && chmod 700 '{0}'",
                artifact.display()
            );
            let plan = json!({
                "program": artifact,
                "args": [m],
                "cwd": work,
                "binary_path": artifact,
            });
            script(json!([step("build", json!(["sh", "-c", build]), 30)]), plan)
        },
        &["sh"],
        &[],
        &[FAST],
    );
    let name = "pinned-app";
    assert_eq!(rig.start(name, json!({}))["type"], "started");
    let pid = rig.pid_of(name).expect("a pid");
    assert!(wait_for(10, || cmdline_of(pid).contains(&m)));
    rig.set_script(hanging());
    rig.reboot();

    assert!(wait_running(&rig, name), "{}", last_error(&rig, name));
    let pid = rig.pid_of(name).expect("a pid after reboot");
    assert!(wait_for(10, || cmdline_of(pid).contains(&m)));
    assert!(rig.audit_text().contains("stored_plan_used"));
    assert_eq!(rig.stop(name)["type"], "stopped");
}

#[test]
fn changed_spec_never_uses_the_persisted_plan() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| building_script(work, &m),
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("cs-app", json!({}))["type"], "started");
    rig.set_script(hanging());
    rig.shutdown_daemon();
    rig.sql("state.db", "UPDATE apps SET spec_json = json_set(spec_json, '$.command', 'changed.fx') WHERE name = 'cs-app'");
    rig.boot_daemon();
    assert!(
        wait_for(30, || rig.status_of("cs-app").as_deref() == Some("errored")),
        "fails closed: {:?}",
        rig.status_of("cs-app")
    );
    assert!(
        last_error(&rig, "cs-app").contains("E_ADAPTER")
            || last_error(&rig, "cs-app")
                .to_lowercase()
                .contains("adapter"),
        "{}",
        last_error(&rig, "cs-app")
    );
    assert!(!rig.audit_text().contains("stored_plan_used"));
}

/// The plan was made by one adapter, that adapter is gone, and another adapter (here: the same
/// files under another name, so the executable hash is the same) provides the runtime. The plan
/// row is still in `adapters.db`, as it would be after a restored backup. The new provider is
/// asked and the old plan is not used, whatever it says.
#[test]
fn removed_adapter_never_uses_the_persisted_plan() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("rm-app", json!({}))["type"], "started");
    assert_eq!(
        rig.plan_rows(),
        [("rm-app".to_string(), ADAPTER.to_string())]
    );
    rig.shutdown_daemon();
    let installed = rig.home.join("adapters").join("installed");
    std::fs::rename(installed.join(ADAPTER), installed.join("other-provider")).unwrap();
    rig.sql("adapters.db", &format!("UPDATE adapters SET name = 'other-provider', manifest_json = json_set(manifest_json, '$.name', 'other-provider') WHERE name = '{ADAPTER}'"));
    // the adapter that is there now does not answer: a stored plan would start the app
    rig.set_script(hanging());
    rig.boot_daemon();
    assert!(
        wait_for(30, || rig.status_of("rm-app").as_deref() == Some("errored")),
        "{:?}: {}",
        rig.status_of("rm-app"),
        last_error(&rig, "rm-app")
    );
    assert_eq!(
        rig.calls("runtime.prepare").len(),
        2,
        "the new provider was asked: {}",
        last_error(&rig, "rm-app")
    );
    assert!(
        last_error(&rig, "rm-app").contains("E_ADAPTER_TIMEOUT")
            || last_error(&rig, "rm-app")
                .to_lowercase()
                .contains("adapter"),
        "{}",
        last_error(&rig, "rm-app")
    );
    assert!(!rig.audit_text().contains("stored_plan_used"));
    assert_eq!(
        rig.plan_rows().first().map(|r| r.1.clone()).as_deref(),
        Some(ADAPTER),
        "the old row is still the old adapter's"
    );
}

#[test]
fn stored_plan_is_refused_when_the_exe_hash_changed() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| building_script(work, &m),
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("eh-app", json!({}))["type"], "started");
    rig.set_script(hanging());
    rig.shutdown_daemon();
    rig.sql(
        "adapters.db",
        &format!("UPDATE plans SET exe_sha256 = '{}'", "0".repeat(64)),
    );
    rig.boot_daemon();
    assert!(
        wait_for(30, || rig.status_of("eh-app").as_deref() == Some("errored")),
        "{:?}",
        rig.status_of("eh-app")
    );
    assert_eq!(
        rig.calls("runtime.prepare").len(),
        2,
        "the adapter was asked"
    );
    assert!(!rig.audit_text().contains("stored_plan_used"));
    assert_eq!(builds(&rig), 1);
}

#[test]
fn operator_restart_never_uses_the_stored_plan() {
    let m = marker();
    let rig = runtime_rig(
        |work| building_script(work, &m),
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("or-app", json!({}))["type"], "started");
    assert_eq!(builds(&rig), 1);
    let restart = rig
        .daemon()
        .rpc(json!({ "type": "restart", "selector": { "by": "name", "value": "or-app" } }));
    assert_eq!(restart["type"], "restarted", "{restart}");
    assert_eq!(
        builds(&rig),
        2,
        "a restart is how the user asks for a rebuild"
    );
    assert_eq!(rig.calls("runtime.prepare").len(), 2);
    assert_eq!(
        rig.calls("runtime.prepare")[1]["frame"]["params"]["phase"],
        "restart"
    );
    assert_eq!(rig.stop("or-app")["type"], "stopped");
}

#[test]
fn reuse_plan_false_turns_the_stored_plan_off() {
    let m = marker();
    let mut rig = runtime_rig_with(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[FAST],
        "reuse_plan = false\n",
    );
    assert_eq!(rig.start("rp-app", json!({}))["type"], "started");
    rig.reboot();
    assert!(wait_running(&rig, "rp-app"));
    assert_eq!(
        rig.calls("runtime.prepare").len(),
        2,
        "with reuse_plan = false the adapter is asked at the boot"
    );
    rig.stop("rp-app");
}

#[test]
fn a_persons_start_or_restart_never_falls_back_on_the_stored_plan() {
    let m = marker();
    // the first `runtime.prepare` is answered (one build step), every later one hangs
    let rig = runtime_rig(
        |work| {
            let built = work.join("built.txt");
            let body = format!("echo built >> '{}'", built.display());
            json!([
                { "on": "runtime.prepare", "times": 1, "do": { "reply": { "result": { "steps": [step("build", json!(["sh", "-c", body]), 30)], "notes": [] } } } },
                { "on": "runtime.prepare", "do": "hang" },
                { "on": "runtime.plan", "do": { "reply": { "result": sleep_plan(work, &m) } } }
            ])
        },
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("pf-app", json!({}))["type"], "started");
    // a restart: the adapter is asked, does not answer, and the app is not started from the
    // plan stored by the first launch
    let restart = rig
        .daemon()
        .rpc(json!({ "type": "restart", "selector": { "by": "name", "value": "pf-app" } }));
    let code = restart["items"][0]["result"]["code"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        code == "E_ADAPTER_TIMEOUT" || code == "E_ADAPTER_UNAVAILABLE",
        "{restart}"
    );
    assert!(
        wait_for(10, || rig.status_of("pf-app").as_deref() == Some("errored")),
        "{:?}",
        rig.status_of("pf-app")
    );
    // and so does a start of the stopped app
    let reply = rig.start("pf-app", json!({}));
    let code = code_of(&reply);
    assert!(
        code == "E_ADAPTER_TIMEOUT" || code == "E_ADAPTER_UNAVAILABLE",
        "{reply}"
    );
    assert_eq!(builds(&rig), 1);
    assert!(!rig.audit_text().contains("stored_plan_used"));
}

#[test]
fn deleting_an_app_drops_its_stored_plan() {
    let m = marker();
    let rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("dp-gone", json!({}))["type"], "started");
    assert_eq!(rig.start("dp-kept", json!({}))["type"], "started");
    let names = |rig: &Rt| {
        rig.plan_rows()
            .into_iter()
            .map(|(app, _)| app)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&rig),
        ["dp-gone", "dp-kept"],
        "a launch stores its plan"
    );
    let deleted = rig
        .daemon()
        .rpc(json!({ "type": "delete", "selector": { "by": "name", "value": "dp-gone" } }));
    assert_eq!(deleted["type"], "deleted", "{deleted}");
    assert_eq!(
        names(&rig),
        ["dp-kept"],
        "the plan of a deleted app goes with it, the others stay"
    );
    assert_eq!(rig.stop("dp-kept")["type"], "stopped");
}

#[test]
fn removing_an_adapter_drops_the_plans_it_made() {
    let m = marker();
    let rig = runtime_rig(
        |work| script(json!([]), sleep_plan(work, &m)),
        &["sleep"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("ra-app", json!({}))["type"], "started");
    assert_eq!(
        rig.plan_rows(),
        [("ra-app".to_string(), ADAPTER.to_string())]
    );
    assert_eq!(rig.stop("ra-app")["type"], "stopped");
    let removed = rig
        .daemon()
        .rpc(json!({ "type": "adapter", "op": "remove", "name": ADAPTER, "purge": false }));
    assert_eq!(
        (removed["type"].as_str(), removed["kind"].as_str()),
        (Some("adapter"), Some("done")),
        "{removed}"
    );
    assert_eq!(
        rig.plan_rows(),
        [],
        "a plan outlives neither its app nor its adapter"
    );
}

/// The adapter answers `runtime.prepare` with one build step and then does not answer
/// `runtime.plan`: the steps ran, and the plan cannot be asked for.
fn plan_hangs_after_building(work: &Path) -> Value {
    let built = work.join("built.txt");
    let body = format!("echo built >> '{}'", built.display());
    json!([
        { "on": "runtime.prepare", "do": { "reply": { "result": { "steps": [step("build", json!(["sh", "-c", body]), 30)], "notes": [] } } } },
        { "on": "runtime.plan", "do": "hang" }
    ])
}

#[test]
fn reboot_after_the_build_steps_ran_and_the_plan_cannot_be_asked_uses_the_stored_plan() {
    let m = marker();
    let mut rig = runtime_rig(
        |work| building_script(work, &m),
        &["sleep", "sh"],
        &[],
        &[FAST],
    );
    assert_eq!(rig.start("bp-app", json!({}))["type"], "started");
    assert_eq!(builds(&rig), 1);
    // the plan of the first launch had a build step, so the boot asks the adapter; the adapter
    // answers `runtime.prepare`, the step runs again, and `runtime.plan` hangs
    rig.set_script(plan_hangs_after_building(&rig.work));
    rig.reboot();
    assert!(
        wait_running(&rig, "bp-app"),
        "the app runs on the plan stored by the first launch: {}",
        last_error(&rig, "bp-app")
    );
    assert_eq!(builds(&rig), 2, "the build steps of this boot did run");
    assert_eq!(
        (
            rig.calls("runtime.prepare").len(),
            rig.calls("runtime.plan").len()
        ),
        (2, 2),
        "the adapter was asked for both"
    );
    let pid = rig.pid_of("bp-app").expect("a pid");
    assert!(
        wait_for(10, || cmdline_of(pid)
            .trim_end()
            .ends_with(&format!("sleep {m}"))),
        "{}",
        cmdline_of(pid)
    );
    assert!(
        rig.audit_text().contains("stored_plan_used"),
        "the audit log says so"
    );
    assert!(
        rig.daemon()
            .stderr()
            .contains("started from the launch plan it made earlier"),
        "the warning is logged"
    );
    rig.stop("bp-app");
}

/// A saved plan is discarded if its program is no longer on the app’s search path.
#[test]
fn stored_plan_whose_program_is_gone_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let (m1, m2) = (marker(), marker());
    let mut rig = runtime_rig(
        |work| {
            let tool = work.join("mysleep");
            std::fs::write(&tool, "#!/bin/sh\nexec sleep \"$@\"\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            let plan = json!({ "program": tool.display().to_string(), "args": [m1], "cwd": work.display().to_string() });
            script(json!([]), plan)
        },
        &["mysleep", "sleep"],
        &[],
        &[FAST],
    );
    assert_eq!(
        rig.start("pg-app", json!({ "env": { "PATH": ".:/usr/bin:/bin" } }))["type"],
        "started"
    );
    let pid = rig.pid_of("pg-app").expect("a pid");
    assert!(
        wait_for(10, || cmdline_of(pid).contains(&m1)),
        "{}",
        cmdline_of(pid)
    );
    // the daemon goes down (the app is to come back at the boot); the program goes, and the
    // adapter now plans with a program that is there
    rig.shutdown_daemon();
    std::fs::remove_file(rig.work.join("mysleep")).unwrap();
    rig.set_script(script(json!([]), sleep_plan(&rig.work, &m2)));
    rig.boot_daemon();
    assert!(
        wait_running(&rig, "pg-app"),
        "{}",
        last_error(&rig, "pg-app")
    );
    assert_eq!(
        rig.calls("runtime.prepare").len(),
        2,
        "the stored plan was not used: the adapter was asked"
    );
    let pid = rig.pid_of("pg-app").expect("a pid");
    assert!(
        wait_for(10, || cmdline_of(pid)
            .trim_end()
            .ends_with(&format!("sleep {m2}"))),
        "{}",
        cmdline_of(pid)
    );
    assert!(!rig.audit_text().contains("stored_plan_used"));
    rig.stop("pg-app");
}

#[test]
fn an_app_whose_first_start_failed_leaves_no_stored_plan() {
    // The plan is stored after validation. Its missing interpreter then prevents spawn.
    let rig = runtime_rig(
        |work| {
            use std::os::unix::fs::PermissionsExt;
            let tool = work.join("missing-interpreter");
            let interpreter = work.join("interpreter");
            std::fs::write(&tool, format!("#!{}\n", interpreter.display())).unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            let plan = json!({ "program": tool.display().to_string(), "args": [], "cwd": work.display().to_string() });
            script(json!([]), plan)
        },
        &["missing-interpreter"],
        &[],
        &[FAST],
    );
    let reply = rig.start("ff-app", json!({ "env": { "PATH": ".:/usr/bin:/bin" } }));
    assert_eq!(reply["type"], "error", "{reply}");
    assert!(rig.app("ff-app").is_none(), "no row is left");
    assert_eq!(rig.plan_rows(), [], "and no plan");
}
