//! `--dry-run` through the real daemon (raw JSON, private home): a request sent with `dry_run`
//! gets a plan and changes nothing. The wire types are in `oxidepm-ipc`; the plan builders are in
//! the request handler.

mod common;

use common::daemon::DaemonProc;
use serde_json::{json, Value};
use tempfile::TempDir;

fn dry(daemon: &DaemonProc, claim: Option<&str>, request: Value) -> Value {
    daemon.rpc_dry_on(&daemon.socket(), "dry", claim, request)["response"].clone()
}

// ---- 19a: the flag on the wire --------------------------------------------------------------

/// A read-only request ignores `dry_run`: the answer is the normal one.
#[test]
fn dry_run_on_a_read_request_is_ignored() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    for (request, kind) in [
        (json!({ "type": "status" }), "status"),
        (json!({ "type": "ping" }), "pong"),
        (json!({ "type": "daemon_status" }), "daemon_status"),
        (json!({ "type": "hello", "client": "t" }), "hello"),
    ] {
        let reply = dry(&daemon, None, request.clone());
        assert_eq!(reply["type"], kind, "{request}: {reply}");
    }
    // the same for an AI actor on the agent socket
    let reply = daemon.rpc_dry_on(
        &daemon.agent_socket(),
        "dry-ai",
        None,
        json!({ "type": "status" }),
    );
    assert_eq!(reply["response"]["type"], "status", "{reply}");
}

/// An old client's `ping`, `hello` and `kill` work on a protocol 2 daemon; its other requests
/// are refused with `E_PROTOCOL_VERSION`.
#[test]
fn old_clients_may_ping_and_hello_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let old = |request: Value| {
        daemon.rpc_envelope(json!({ "protocol": 1, "request_id": "old", "request": request }))
    };
    assert_eq!(old(json!({ "type": "ping" }))["response"]["type"], "pong");
    assert_eq!(
        old(json!({ "type": "hello", "client": "old" }))["response"]["type"],
        "hello"
    );
    let refused = old(json!({ "type": "status" }));
    assert_eq!(
        refused["response"]["code"], "E_PROTOCOL_VERSION",
        "{refused}"
    );
}

// ---- helpers for the plans ------------------------------------------------------------------

use oxidepm_core::{AppSpec, Runtime};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn workdir(tmp: &TempDir) -> PathBuf {
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `sleep` app as the JSON a client sends, with `extra` merged over the spec.
fn app(workdir: &Path, name: &str, extra: Value) -> Value {
    let mut spec = AppSpec::new(
        name.into(),
        Runtime::CMD,
        "sleep".into(),
        workdir.to_path_buf(),
    );
    spec.args = vec!["307.5".into()];
    let mut value = serde_json::to_value(spec).unwrap();
    if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        for (key, val) in extra {
            base.insert(key.clone(), val.clone());
        }
    }
    value
}

fn by_name(name: &str) -> Value {
    json!({ "by": "name", "value": name })
}

fn all() -> Value {
    json!({ "by": "all" })
}

fn human(daemon: &DaemonProc, request: Value) -> Value {
    daemon.rpc_on(&daemon.socket(), "req-human", None, request)["response"].clone()
}

fn ai_dry(daemon: &DaemonProc, request: Value) -> Value {
    daemon.rpc_dry_on(&daemon.socket(), "req-ai-dry", Some("ai:test"), request)["response"].clone()
}

fn start(daemon: &DaemonProc, spec: Value) -> Value {
    let reply = human(daemon, json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "started", "{reply}");
    reply
}

fn status_of(daemon: &DaemonProc, name: &str) -> Value {
    let reply = human(daemon, json!({ "type": "status" }));
    reply["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["spec"]["name"] == name)
        .unwrap_or_else(|| panic!("no app {name}: {reply}"))
        .clone()
}

fn wait_running(daemon: &DaemonProc, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while status_of(daemon, name)["state"]["status"] != "running" {
        assert!(
            Instant::now() < deadline,
            "{name} never ran: {}",
            status_of(daemon, name)
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// The plan inside a `dry_run` response (the response must be one).
fn plan_of(reply: &Value) -> &Value {
    assert_eq!(reply["type"], "dry_run", "{reply}");
    &reply["plan"]
}

fn items(plan: &Value) -> &Vec<Value> {
    plan["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items: {plan}"))
}

fn item<'a>(plan: &'a Value, name: &str) -> &'a Value {
    items(plan)
        .iter()
        .find(|i| i["name"] == name)
        .unwrap_or_else(|| panic!("no item for {name} in {plan}"))
}

fn detail(item: &Value) -> &str {
    item["detail"].as_str().unwrap_or("")
}

// ---- the snapshot: what a dry run must leave exactly as it was --------------------------------

/// Everything a dry run is not allowed to change, as plain values that can be compared.
#[derive(Debug, PartialEq)]
struct Snapshot {
    /// (id, name, status, pid, restarts) of every app, from the daemon's registry
    registry: Vec<(u64, String, String, Option<u64>, u64)>,
    /// the rows of `state.db`
    rows: Vec<String>,
    /// the daemon's child processes
    children: BTreeSet<u32>,
    /// every file of the home with its content (up to 256 KiB; larger files by size), except
    /// the database (compared by rows), the daemon's own stderr capture and the audit directory
    files: BTreeMap<String, (u64, Option<Vec<u8>>)>,
    /// the log files by size
    log_sizes: BTreeMap<String, u64>,
    /// the audit lines that are not a rehearsal's own
    audit: Vec<String>,
}

fn children_of(pid: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(child) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        // "pid (comm) S ppid ..." and comm may hold spaces and parentheses: cut after the last ')'
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok())
            == Some(pid)
        {
            out.insert(child);
        }
    }
    out
}

fn db_rows(home: &Path) -> Vec<String> {
    use sqlx::Row;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let url = format!("sqlite://{}?mode=ro", home.join("state.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.expect("open state.db read-only");
        let rows = sqlx::query(
            "SELECT id, name, parent_id, desired_state, spec_json, created_at, updated_at, protected, ai_control \
             FROM apps ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("read rows");
        pool.close().await;
        rows.iter()
            .map(|r| {
                format!(
                    "{}|{}|{:?}|{}|{}|{}|{}|{}|{}",
                    r.get::<i64, _>(0),
                    r.get::<String, _>(1),
                    r.get::<Option<i64>, _>(2),
                    r.get::<String, _>(3),
                    r.get::<String, _>(4),
                    r.get::<String, _>(5),
                    r.get::<String, _>(6),
                    r.get::<i64, _>(7),
                    r.get::<i64, _>(8)
                )
            })
            .collect()
    })
}

fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (u64, Option<Vec<u8>>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        // the daemon's own log gets a line per request; the database is compared by its rows
        if rel == "audit"
            || rel.starts_with("state.db")
            || rel == "daemon-test.stderr"
            || rel.starts_with("daemon.log")
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            out.insert(format!("{rel}/"), (0, None));
            walk(&path, root, out);
        } else if meta.is_file() {
            let content =
                (meta.len() <= 256 * 1024).then(|| std::fs::read(&path).unwrap_or_default());
            out.insert(rel, (meta.len(), content));
        }
    }
}

fn audit_lines_outside_rehearsals(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for file in ["audit.jsonl", "audit-security.jsonl"] {
        let Ok(text) = std::fs::read_to_string(home.join("audit").join(file)) else {
            continue;
        };
        let mut rehearsal_reqs = BTreeSet::new();
        let lines: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        for line in &lines {
            if line["dry_run"] == true {
                if let Some(req) = line["req"].as_str() {
                    rehearsal_reqs.insert(req.to_string());
                }
            }
        }
        for line in &lines {
            let mine = line["req"]
                .as_str()
                .is_some_and(|r| rehearsal_reqs.contains(r));
            if !mine {
                out.push(format!("{file}: {line}"));
            }
        }
    }
    out
}

fn snapshot(daemon: &DaemonProc) -> Snapshot {
    let reply = human(daemon, json!({ "type": "status" }));
    let registry = reply["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["spec"]["id"].as_u64().unwrap(),
                a["spec"]["name"].as_str().unwrap().to_string(),
                a["state"]["status"].as_str().unwrap().to_string(),
                a["state"]["pid"].as_u64(),
                a["state"]["restarts"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    let mut files = BTreeMap::new();
    walk(daemon.home(), daemon.home(), &mut files);
    let log_sizes = files
        .iter()
        .filter(|(path, _)| path.starts_with("logs/"))
        .map(|(path, (len, _))| (path.clone(), *len))
        .collect();
    Snapshot {
        registry,
        rows: db_rows(daemon.home()),
        children: children_of(daemon.pid()),
        files,
        log_sizes,
        audit: audit_lines_outside_rehearsals(daemon.home()),
    }
}

/// Wait (bounded) until the audit file of `daemon` has a line that `pick` accepts.
fn wait_audit<T>(daemon: &DaemonProc, file: &str, pick: impl Fn(&Value) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text =
            std::fs::read_to_string(daemon.home().join("audit").join(file)).unwrap_or_default();
        if let Some(found) = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|l| pick(&l))
        {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no matching line in {file}: {text}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// A home with a running chain (db, web depending on db), a cluster, a stopped app and an app
/// open to AI control. Returns the daemon.
fn busy_daemon(tmp: &TempDir) -> DaemonProc {
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let dir = workdir(tmp);
    start(&daemon, app(&dir, "db", json!({ "tags": ["data"] })));
    wait_running(&daemon, "db");
    start(
        &daemon,
        app(
            &dir,
            "web",
            json!({ "depends_on": ["db"], "tags": ["front"] }),
        ),
    );
    wait_running(&daemon, "web");
    start(&daemon, app(&dir, "clu", json!({ "instances": 2 })));
    wait_running(&daemon, "clu-0");
    wait_running(&daemon, "clu-1");
    start(&daemon, app(&dir, "idle", json!({})));
    wait_running(&daemon, "idle");
    let stopped = human(
        &daemon,
        json!({ "type": "stop", "selector": by_name("idle") }),
    );
    assert_eq!(stopped["type"], "stopped", "{stopped}");
    start(&daemon, app(&dir, "open", json!({ "ai_control": true })));
    wait_running(&daemon, "open");
    // a log file with something in it
    std::fs::create_dir_all(daemon.home().join("logs")).unwrap();
    daemon
}

// ---- 19b (i): selector-based plans ------------------------------------------------------------

#[test]
fn dry_run_stop_lists_dependents_first_order() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    let reply = dry(&daemon, None, json!({ "type": "stop", "selector": all() }));
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "stop");
    assert_eq!(plan["decision"], json!({ "decision": "allow" }));

    // web depends on db: web stops in an earlier layer than db
    let (web, db) = (item(plan, "web"), item(plan, "db"));
    assert_eq!(
        (web["action"].as_str(), db["action"].as_str()),
        (Some("stop"), Some("stop"))
    );
    assert!(
        web["order"].as_u64().unwrap() < db["order"].as_u64().unwrap(),
        "{plan}"
    );
    // an app at rest has nothing to stop
    assert_eq!(item(plan, "idle")["action"], "noop", "{plan}");
    // the instances of the cluster stop in layers, the cluster itself after all of them
    let clu = item(plan, "clu");
    let last_instance = ["clu-0", "clu-1"]
        .iter()
        .map(|n| item(plan, n)["order"].as_u64().unwrap())
        .max()
        .unwrap();
    assert!(clu["order"].as_u64().unwrap() > last_instance, "{plan}");
    // every app that runs is listed once, with its id
    let names: Vec<&str> = items(plan)
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    for name in ["db", "web", "clu", "clu-0", "clu-1", "idle", "open"] {
        assert_eq!(
            names.iter().filter(|n| **n == name).count(),
            1,
            "{name} in {names:?}"
        );
    }
    assert!(items(plan).iter().all(|i| i["id"].is_u64()), "{plan}");
    // and nothing stopped
    assert_eq!(status_of(&daemon, "web")["state"]["status"], "running");
}

#[test]
fn dry_run_delete_of_a_cluster_lists_the_instances() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    // some log output to remain
    std::fs::write(daemon.home().join("logs/clu-0-out.log"), b"hello\n").unwrap();
    std::fs::write(daemon.home().join("logs/clu-0-out.log.1"), b"older\n").unwrap();

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "delete", "selector": by_name("clu") }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "delete");
    let names: Vec<&str> = items(plan)
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["clu", "clu-0", "clu-1"],
        "parent first, then its instances: {plan}"
    );
    assert!(
        items(plan).iter().all(|i| i["action"] == "delete"),
        "{plan}"
    );
    assert!(
        detail(item(plan, "clu")).contains("cluster of 2 instance(s)"),
        "{plan}"
    );
    // the log files that would stay are listed with their sizes
    let first = detail(item(plan, "clu-0"));
    assert!(
        first.contains("clu-0-out.log (6 B)") && first.contains("clu-0-out.log.1 (6 B)"),
        "{first}"
    );
    assert!(
        detail(item(plan, "clu-1")).contains("clu-1-out.log (0 B)"),
        "{plan}"
    );
    // nothing was deleted
    assert_eq!(status_of(&daemon, "clu")["state"]["status"], "running");
    assert!(daemon.home().join("logs/clu-0-out.log").exists());
}

#[test]
fn dry_run_restart_notes_that_the_gate_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "restart", "selector": by_name("web") }),
    );
    let plan = plan_of(&reply);
    let web = item(plan, "web");
    assert_eq!(web["action"], "restart", "{plan}");
    assert!(detail(web).contains("dependency gate skipped"), "{plan}");

    // an app at rest is started, and that does wait for its dependencies
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "restart", "selector": by_name("idle") }),
    );
    let idle = item(plan_of(&reply), "idle").clone();
    assert_eq!(idle["action"], "start", "{reply}");
    assert!(
        detail(&idle).contains("waits for its dependencies"),
        "{idle}"
    );

    // a reload is a restart and the plan says so
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "reload", "selector": by_name("db") }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "reload");
    assert_eq!(item(plan, "db")["action"], "restart");
    assert!(
        detail(item(plan, "db")).contains("a reload is a restart"),
        "{plan}"
    );

    // the app was not restarted: same pid
    let pid = status_of(&daemon, "web")["state"]["pid"].clone();
    assert!(pid.is_u64());
    dry(
        &daemon,
        None,
        json!({ "type": "restart", "selector": by_name("web") }),
    );
    assert_eq!(status_of(&daemon, "web")["state"]["pid"], pid);
}

#[test]
fn dry_run_flush_lists_files_and_leaves_them() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    let out = daemon.home().join("logs/db-out.log");
    let err = daemon.home().join("logs/db-err.log");
    std::fs::write(&out, b"0123456789").unwrap();
    std::fs::write(&err, b"abc").unwrap();

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "flush", "selector": by_name("db") }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "flush");
    let db = detail(item(plan, "db"));
    assert!(
        db.contains("db-out.log (10 B)") && db.contains("db-err.log (3 B)"),
        "{db}"
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"0123456789");
    assert_eq!(std::fs::read(&err).unwrap(), b"abc");

    // a cluster flushes through its instances
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "flush", "selector": by_name("clu") }),
    );
    let plan = plan_of(&reply);
    assert!(
        detail(item(plan, "clu")).contains("2 instance(s)"),
        "{plan}"
    );
    assert!(
        items(plan).iter().any(|i| i["name"] == "clu-0")
            && items(plan).iter().any(|i| i["name"] == "clu-1")
    );
}

#[test]
fn dry_run_denied_by_policy_reports_the_denial() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);

    // an AI actor may not stop an app that is not open to it: the refusal is the plan
    let reply = ai_dry(
        &daemon,
        json!({ "type": "stop", "selector": by_name("db") }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "stop");
    assert_eq!(plan["decision"]["decision"], "deny", "{plan}");
    assert_eq!(plan["decision"]["code"], "E_POLICY_DENIED", "{plan}");
    assert!(
        plan["decision"]["message"].as_str().unwrap().contains("db"),
        "{plan}"
    );
    assert!(items(plan).is_empty());

    // a dangerous request is refused before anything is read
    let reply = ai_dry(
        &daemon,
        json!({ "type": "delete", "selector": by_name("open") }),
    );
    assert_eq!(
        plan_of(&reply)["decision"]["code"],
        "E_POLICY_DENIED",
        "{reply}"
    );
    // 'all' is refused for an AI whatever it is
    let reply = ai_dry(&daemon, json!({ "type": "restart", "selector": all() }));
    assert_eq!(plan_of(&reply)["decision"]["decision"], "deny", "{reply}");

    // an app that is open to it gets a plan
    let reply = ai_dry(
        &daemon,
        json!({ "type": "stop", "selector": by_name("open") }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["decision"], json!({ "decision": "allow" }), "{plan}");
    assert_eq!(item(plan, "open")["action"], "stop");
    assert_eq!(status_of(&daemon, "open")["state"]["status"], "running");
}

/// A rehearsal does not use up the allowance of the real request: an AI actor may restart an app
/// once every 10 seconds and a rehearsal in between must not start that clock.
#[test]
fn dry_run_restart_does_not_use_up_the_restart_allowance() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    for _ in 0..3 {
        let reply = ai_dry(
            &daemon,
            json!({ "type": "restart", "selector": by_name("open") }),
        );
        assert_eq!(
            plan_of(&reply)["decision"],
            json!({ "decision": "allow" }),
            "{reply}"
        );
    }
    // the real restart still goes through, and now the clock runs: the next one is refused
    let real = daemon.rpc_on(
        &daemon.socket(),
        "real",
        Some("ai:test"),
        json!({ "type": "restart", "selector": by_name("open") }),
    );
    assert_eq!(real["response"]["type"], "restarted", "{real}");
    let again = ai_dry(
        &daemon,
        json!({ "type": "restart", "selector": by_name("open") }),
    );
    let plan = plan_of(&again);
    assert_eq!(plan["decision"]["decision"], "deny", "{plan}");
    assert_eq!(plan["decision"]["code"], "E_RATE_LIMITED", "{plan}");
}

#[test]
fn dry_run_is_recorded_as_dry_run_in_the_audit_file() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);

    // an AI actor's rehearsal is recorded on disk with dry_run true, and so is its result
    let reply = ai_dry(
        &daemon,
        json!({ "type": "stop", "selector": by_name("open") }),
    );
    assert_eq!(plan_of(&reply)["decision"]["decision"], "allow");
    let decision = wait_audit(&daemon, "audit.jsonl", |l| {
        (l["phase"] == "decision" && l["op"] == "stop" && l["dry_run"] == true).then(|| l.clone())
    });
    assert_eq!(decision["actor"]["class"], "ai");
    assert_eq!(decision["decision"], "allow");
    let req = decision["req"].as_str().unwrap().to_string();
    let result = wait_audit(&daemon, "audit.jsonl", |l| {
        (l["phase"] == "result" && l["req"] == req.as_str()).then(|| l.clone())
    });
    assert_eq!(
        (
            result["ok"].clone(),
            result["dry_run"].clone(),
            result["changed"].clone()
        ),
        (json!(true), json!(true), json!(0))
    );

    // a person's rehearsal of a dangerous request is in the security file; the real ones are not
    // mixed up with it
    dry(
        &daemon,
        None,
        json!({ "type": "delete", "selector": by_name("open") }),
    );
    let line = wait_audit(&daemon, "audit-security.jsonl", |l| {
        (l["phase"] == "decision" && l["op"] == "delete" && l["dry_run"] == true).then(|| l.clone())
    });
    assert_eq!(line["actor"]["class"], "human");
    // the real stop that follows is recorded as a real one
    let real = daemon.rpc_on(
        &daemon.socket(),
        "real-stop",
        Some("ai:test"),
        json!({ "type": "stop", "selector": by_name("open") }),
    );
    assert_eq!(real["response"]["type"], "stopped", "{real}");
    wait_audit(&daemon, "audit.jsonl", |l| {
        (l["phase"] == "decision" && l["op"] == "stop" && l["dry_run"] == false).then_some(())
    });
}

// ---- 19b (ii): start and batch ----------------------------------------------------------------

/// A spec whose program leaves `marker` behind if it ever runs.
fn touching(workdir: &Path, name: &str, marker: &Path, extra: Value) -> Value {
    let mut value = app(workdir, name, extra);
    value["command"] = json!("sh");
    value["args"] = json!([
        "-c",
        format!("touch {}; exec sleep 307.5", marker.display())
    ]);
    value
}

#[test]
fn dry_run_start_invalid_spec_returns_the_real_error() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let dir = workdir(&tmp);

    // a working directory that is not one
    let mut spec = app(&dir, "bad", json!({}));
    spec["cwd"] = json!(tmp.path().join("no-such-dir"));
    let reply = dry(&daemon, None, json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "E_INVALID_SPEC", "{reply}");
    // a name that is not a name
    let mut spec = app(&dir, "bad", json!({}));
    spec["name"] = json!("bad name!");
    let reply = dry(&daemon, None, json!({ "type": "start", "spec": spec }));
    assert!(
        matches!(
            reply["code"].as_str(),
            Some("E_INVALID_SPEC" | "E_INVALID_NAME")
        ),
        "{reply}"
    );
    // a dependency that does not exist
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "needy", json!({ "depends_on": ["ghost"] })) }),
    );
    assert_eq!(reply["code"], "E_DEPENDENCY_MISSING", "{reply}");
    assert!(db_rows(daemon.home()).is_empty(), "no row");
    assert_eq!(
        human(&daemon, json!({ "type": "status" }))["apps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn dry_run_start_valid_spec_creates_no_row_and_no_process() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let dir = workdir(&tmp);
    let before = snapshot(&daemon);

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "fresh", json!({ "port": 4711, "tags": ["x"] })) }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "start");
    assert_eq!(plan["decision"], json!({ "decision": "allow" }));
    assert_eq!(items(plan).len(), 1, "{plan}");
    let fresh = item(plan, "fresh");
    assert_eq!(fresh["action"], "would_create", "{plan}");
    assert!(fresh["id"].is_null(), "{plan}");
    let text = detail(fresh);
    assert!(
        text.contains("runtime cmd")
            && text.contains("program sleep")
            && text.contains("port 4711"),
        "{text}"
    );
    assert!(
        plan["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("known only when the app starts")),
        "{plan}"
    );

    assert_eq!(
        human(&daemon, json!({ "type": "status" }))["apps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let after = snapshot(&daemon);
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.children, after.children);
    assert!(after.rows.is_empty());
    // the same app can then really be started: the name was never taken
    start(&daemon, app(&dir, "fresh", json!({})));
}

#[test]
fn dry_run_start_spawns_nothing_and_calls_no_adapter() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let dir = workdir(&tmp);
    let marker = tmp.path().join("ran");
    let before = children_of(daemon.pid());

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": touching(&dir, "toucher", &marker, json!({ "instances": 2 })) }),
    );
    let plan = plan_of(&reply);
    assert_eq!(item(plan, "toucher")["action"], "would_create");
    assert!(
        detail(item(plan, "toucher")).contains("cluster of 2 instance(s)"),
        "{plan}"
    );
    let batch = dry(
        &daemon,
        None,
        json!({ "type": "start_batch", "specs": [touching(&dir, "t2", &marker, json!({}))] }),
    );
    assert_eq!(
        plan_of(&batch)["items"][0]["action"],
        "would_create",
        "{batch}"
    );

    std::thread::sleep(Duration::from_millis(400));
    assert!(!marker.exists(), "the program ran");
    assert_eq!(children_of(daemon.pid()), before, "no process was spawned");
    assert!(
        !daemon.home().join("logs").join("toucher-out.log").exists(),
        "no log file"
    );
}

#[test]
fn dry_run_start_of_a_running_app_says_app_exists() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    let dir = workdir(&tmp);

    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "db", json!({})) }),
    );
    let plan = plan_of(&reply);
    let db = item(plan, "db");
    assert_eq!(db["action"], "refuse", "{plan}");
    assert!(detail(db).starts_with("E_APP_EXISTS:"), "{plan}");
    assert!(db["id"].is_u64());

    // a stopped app of that name would be started again
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "idle", json!({})) }),
    );
    let idle = item(plan_of(&reply), "idle").clone();
    assert_eq!(idle["action"], "would_start", "{reply}");
    assert!(detail(&idle).contains("the app exists (stopped)"), "{idle}");
    // the count of a cluster cannot change, up or not: the real error
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "clu", json!({ "instances": 3 })) }),
    );
    assert_eq!(reply["code"], "E_INVALID_SPEC", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("has 2 instances"),
        "{reply}"
    );
    // the same count: the cluster is up
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start", "spec": app(&dir, "clu", json!({ "instances": 2 })) }),
    );
    assert_eq!(item(plan_of(&reply), "clu")["action"], "refuse", "{reply}");
    // in a batch it is that app's refusal
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start_batch", "specs": [app(&dir, "clu", json!({ "instances": 5 }))] }),
    );
    let clu = item(plan_of(&reply), "clu").clone();
    assert_eq!(
        (
            clu["action"].as_str(),
            detail(&clu).starts_with("E_INVALID_SPEC")
        ),
        (Some("refuse"), true),
        "{clu}"
    );
}

#[test]
fn dry_run_start_batch_lists_layers_and_refusals() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    let dir = workdir(&tmp);

    let specs = vec![
        app(&dir, "api", json!({ "depends_on": ["cache"] })),
        app(&dir, "cache", json!({})),
        app(&dir, "worker", json!({ "depends_on": ["api", "db"] })),
        app(&dir, "db", json!({})),
        app(&dir, "ghosty", json!({ "depends_on": ["nobody"] })),
        app(&dir, "after-ghost", json!({ "depends_on": ["ghosty"] })),
        app(&dir, "ping", json!({ "depends_on": ["pong"] })),
        app(&dir, "pong", json!({ "depends_on": ["ping"] })),
    ];
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "start_batch", "specs": specs }),
    );
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "start_batch");
    let names: Vec<&str> = items(plan)
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "api",
            "cache",
            "worker",
            "db",
            "ghosty",
            "after-ghost",
            "ping",
            "pong"
        ],
        "request order: {plan}"
    );

    // layers: cache before api before worker
    let layer = |n: &str| item(plan, n)["order"].as_u64().unwrap();
    assert!(
        layer("cache") < layer("api") && layer("api") < layer("worker"),
        "{plan}"
    );
    assert_eq!(item(plan, "cache")["action"], "would_create");
    // db runs already: refused as in the real answer
    assert_eq!(item(plan, "db")["action"], "refuse");
    assert!(
        detail(item(plan, "db")).starts_with("E_APP_EXISTS"),
        "{plan}"
    );
    // an unknown dependency, its dependents, and a cycle
    assert!(
        detail(item(plan, "ghosty")).starts_with("E_DEPENDENCY_MISSING"),
        "{plan}"
    );
    assert!(
        detail(item(plan, "after-ghost")).starts_with("E_DEPENDENCY_FAILED"),
        "{plan}"
    );
    assert!(
        detail(item(plan, "ping")).starts_with("E_DEPENDENCY_CYCLE"),
        "{plan}"
    );
    assert!(
        detail(item(plan, "pong")).starts_with("E_DEPENDENCY_CYCLE"),
        "{plan}"
    );
    assert!(
        detail(item(plan, "worker")).contains("api (in this batch)"),
        "{plan}"
    );

    // the real batch gives the same refusals (the plan is what would happen)
    let specs = vec![
        app(&dir, "ghosty", json!({ "depends_on": ["nobody"] })),
        app(&dir, "fine", json!({})),
    ];
    let real = human(&daemon, json!({ "type": "start_batch", "specs": specs }));
    assert_eq!(
        real["items"][0]["result"]["code"], "E_DEPENDENCY_MISSING",
        "{real}"
    );

    // a name twice is the request's error, in a rehearsal too
    let twice = dry(
        &daemon,
        None,
        json!({ "type": "start_batch", "specs": [app(&dir, "same", json!({})), app(&dir, "same", json!({}))] }),
    );
    assert_eq!(twice["code"], "E_INVALID_SPEC", "{twice}");
}

// ---- 19b (iii): save, resurrect, kill, set-policy ------------------------------------------------

#[test]
fn dry_run_save_reports_would_empty() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let dir = workdir(&tmp);
    start(&daemon, app(&dir, "keeper", json!({})));
    wait_running(&daemon, "keeper");

    // with an app that is meant to run: the file and the apps in it
    let reply = dry(&daemon, None, json!({ "type": "save", "force": false }));
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "save");
    let file = &items(plan)[0];
    assert_eq!(file["action"], "save", "{plan}");
    assert_eq!(file["name"], "saved.json");
    assert!(
        detail(file).contains("writes 1 app(s)") && detail(file).contains("keeper"),
        "{plan}"
    );
    assert!(
        !daemon.home().join("saved.json").exists(),
        "nothing was written"
    );

    // a saved file that lists an app, and nothing meant to run: the refusal a real save meets
    let saved = human(&daemon, json!({ "type": "save", "force": false }));
    assert_eq!(saved["type"], "saved", "{saved}");
    let stopped = human(
        &daemon,
        json!({ "type": "stop", "selector": by_name("keeper") }),
    );
    assert_eq!(stopped["type"], "stopped");
    let before = std::fs::read(daemon.home().join("saved.json")).unwrap();
    let reply = dry(&daemon, None, json!({ "type": "save", "force": false }));
    let file = items(plan_of(&reply))[0].clone();
    assert_eq!(file["action"], "refuse", "{reply}");
    assert!(detail(&file).starts_with("E_SAVE_WOULD_EMPTY:"), "{file}");
    // the real save agrees, and forcing turns the plan into a save
    assert_eq!(
        human(&daemon, json!({ "type": "save", "force": false }))["code"],
        "E_SAVE_WOULD_EMPTY"
    );
    let reply = dry(&daemon, None, json!({ "type": "save", "force": true }));
    assert_eq!(items(plan_of(&reply))[0]["action"], "save", "{reply}");
    assert_eq!(
        std::fs::read(daemon.home().join("saved.json")).unwrap(),
        before,
        "the file is as it was"
    );

    // an AI actor may not save, rehearsal or not
    let reply = ai_dry(&daemon, json!({ "type": "save", "force": false }));
    assert_eq!(
        plan_of(&reply)["decision"]["code"],
        "E_POLICY_DENIED",
        "{reply}"
    );
}

#[test]
fn dry_run_resurrect_lists_targets_and_starts_nothing() {
    use serde_json::Value as V;
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let dir = workdir(&tmp);
    std::fs::create_dir_all(&home).unwrap();
    let spec = |name: &str, extra: V| -> AppSpec {
        serde_json::from_value(app(&dir, name, extra)).unwrap()
    };
    // the boot resurrect starts these: one that runs and one whose program does not exist, which
    // stays registered, errored, meant to run
    let mut broken = spec("broken-prog", json!({}));
    broken.command = "/no/such/program".into();
    std::fs::write(
        home.join("saved.json"),
        common::saved_json(&[spec("alive", json!({})), broken]),
    )
    .unwrap();
    let daemon = DaemonProc::spawn(&home);
    daemon.wait_for_boot();
    wait_running(&daemon, "alive");
    let deadline = Instant::now() + Duration::from_secs(10);
    while status_of(&daemon, "broken-prog")["state"]["status"] != "errored" {
        assert!(
            Instant::now() < deadline,
            "{}",
            status_of(&daemon, "broken-prog")
        );
        std::thread::sleep(Duration::from_millis(30));
    }

    // the saved file now also lists a newcomer, one that depends on it, one that cannot be a spec
    // and an app that is up already
    let mut text: V = serde_json::from_str(&common::saved_json(&[
        spec("alive", json!({})),
        spec("newcomer", json!({})),
        spec("follower", json!({ "depends_on": ["newcomer"] })),
    ]))
    .unwrap();
    text["apps"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": 5 }));
    std::fs::write(home.join("saved.json"), text.to_string()).unwrap();
    let before = snapshot(&daemon);

    let reply = dry(&daemon, None, json!({ "type": "resurrect" }));
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "resurrect");
    assert_eq!(item(plan, "newcomer")["action"], "would_import", "{plan}");
    assert_eq!(item(plan, "follower")["action"], "would_import", "{plan}");
    assert!(
        item(plan, "follower")["order"].as_u64().unwrap()
            > item(plan, "newcomer")["order"].as_u64().unwrap(),
        "{plan}"
    );
    assert_eq!(item(plan, "broken-prog")["action"], "would_start", "{plan}");
    assert!(
        detail(item(plan, "broken-prog")).contains("errored"),
        "{plan}"
    );
    assert!(
        items(plan).iter().all(|i| i["name"] != "alive"),
        "an app that is up is not a target: {plan}"
    );
    let bad = items(plan)
        .iter()
        .find(|i| i["action"] == "refuse")
        .unwrap_or_else(|| panic!("{plan}"));
    assert!(detail(bad).starts_with("E_STATE_INCOMPATIBLE:"), "{plan}");

    // nothing was imported, nothing started
    let after = snapshot(&daemon);
    assert_eq!(before.registry, after.registry);
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.children, after.children);
    assert_eq!(
        status_of(&daemon, "broken-prog")["state"]["status"],
        "errored"
    );

    // a saved file that cannot be used is the real error
    std::fs::write(home.join("saved.json"), "this is not json").unwrap();
    let reply = dry(&daemon, None, json!({ "type": "resurrect" }));
    assert_eq!(
        (reply["type"].clone(), reply["code"].clone()),
        (json!("error"), json!("E_STATE_INCOMPATIBLE")),
        "{reply}"
    );
}

#[test]
fn dry_run_kill_lists_apps_and_the_daemon_stays_up() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = busy_daemon(&tmp);
    let reply = dry(&daemon, None, json!({ "type": "kill" }));
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "kill");
    assert!(
        plan["warnings"][0]
            .as_str()
            .unwrap()
            .contains("the daemon exits"),
        "{plan}"
    );
    // the order of the shutdown: web before db
    assert!(
        item(plan, "web")["order"].as_u64().unwrap() < item(plan, "db")["order"].as_u64().unwrap(),
        "{plan}"
    );
    for name in ["db", "web", "clu", "clu-0", "clu-1", "idle", "open"] {
        assert!(
            items(plan).iter().any(|i| i["name"] == name),
            "{name} in {plan}"
        );
    }
    // the daemon answers, nothing stopped, and it did not start to shut down
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    assert_eq!(status_of(&daemon, "db")["state"]["status"], "running");
    assert!(daemon.try_exit().is_none());
    // an AI actor's rehearsal of a kill is refused like the real one
    let reply = ai_dry(&daemon, json!({ "type": "kill" }));
    assert_eq!(
        plan_of(&reply)["decision"]["code"],
        "E_POLICY_DENIED",
        "{reply}"
    );
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    // and the real kill still works after the rehearsals
    let real = daemon.rpc(json!({ "type": "kill" }));
    assert_eq!(real["type"], "ok", "{real}");
    daemon.wait_exit();
}

#[test]
fn dry_run_set_policy_lists_the_marker_changes() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    let markers = |name: &str| {
        let app = status_of(&daemon, name);
        (
            app["spec"]["protected"].clone(),
            app["spec"]["ai_control"].clone(),
        )
    };
    assert_eq!(markers("web"), (json!(false), json!(false)));

    let request = json!({ "type": "set_policy", "selector": by_name("web"), "protected": true });
    let reply = dry(&daemon, None, request.clone());
    let plan = plan_of(&reply);
    assert_eq!(plan["op"], "set_policy");
    let web = item(plan, "web");
    assert_eq!(web["action"], "set_policy", "{plan}");
    assert!(
        detail(web).contains("protected false -> true")
            && detail(web).contains("ai_control false -> false"),
        "{plan}"
    );
    assert_eq!(
        markers("web"),
        (json!(false), json!(false)),
        "nothing changed"
    );

    // a marker that already has the value is a noop; a cluster carries its instances
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "set_policy", "selector": by_name("open"), "ai_control": true }),
    );
    assert_eq!(item(plan_of(&reply), "open")["action"], "noop", "{reply}");
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "set_policy", "selector": by_name("clu"), "protected": true }),
    );
    assert!(
        detail(item(plan_of(&reply), "clu")).contains("2 instance(s) follow"),
        "{reply}"
    );
    // naming no marker is the real error
    let reply = dry(
        &daemon,
        None,
        json!({ "type": "set_policy", "selector": by_name("web") }),
    );
    assert_eq!(reply["code"], "E_INVALID_SPEC", "{reply}");
    // only a person may set a marker, rehearsal or not
    let reply = ai_dry(&daemon, request);
    assert_eq!(
        plan_of(&reply)["decision"]["code"],
        "E_POLICY_DENIED",
        "{reply}"
    );
}

/// The key property: whatever the rehearsals are, the registry, the database, the processes, the
/// files of the home and the audit lines of real requests are exactly as they were.
#[test]
fn dry_run_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let daemon = busy_daemon(&tmp);
    std::fs::write(daemon.home().join("logs/db-out.log"), b"log text\n").unwrap();
    std::fs::write(daemon.home().join("logs/web-err.log"), b"more log text\n").unwrap();
    // let the sampler and the logs settle, then look
    std::thread::sleep(Duration::from_millis(300));
    let before = snapshot(&daemon);
    assert!(before.children.len() >= 5, "the apps run: {before:?}");

    let mut catalogue: Vec<(Option<&str>, Value)> = Vec::new();
    for selector in [
        all(),
        by_name("web"),
        by_name("clu"),
        by_name("idle"),
        json!({ "by": "tag", "value": "data" }),
    ] {
        for kind in ["stop", "restart", "reload", "delete", "flush"] {
            catalogue.push((None, json!({ "type": kind, "selector": selector })));
        }
    }
    let dir = workdir(&tmp);
    catalogue.push((
        None,
        json!({ "type": "start", "spec": app(&dir, "fresh", json!({})) }),
    ));
    catalogue.push((
        None,
        json!({ "type": "start", "spec": app(&dir, "db", json!({})) }),
    ));
    catalogue.push((
        None,
        json!({ "type": "start", "spec": app(&dir, "idle", json!({})) }),
    ));
    catalogue.push((None, json!({ "type": "start_batch", "specs": [app(&dir, "b1", json!({})), app(&dir, "b2", json!({ "depends_on": ["b1"] }))] })));
    catalogue.push((None, json!({ "type": "save", "force": false })));
    catalogue.push((None, json!({ "type": "save", "force": true })));
    catalogue.push((None, json!({ "type": "resurrect" })));
    catalogue.push((None, json!({ "type": "kill" })));
    catalogue.push((None, json!({ "type": "set_policy", "selector": by_name("web"), "protected": true, "ai_control": true })));
    catalogue.push((
        None,
        json!({ "type": "set_policy", "selector": by_name("clu"), "protected": true }),
    ));
    catalogue.push((Some("ai:test"), json!({ "type": "save", "force": false })));
    catalogue.push((Some("ai:test"), json!({ "type": "kill" })));
    catalogue.push((
        Some("ai:test"),
        json!({ "type": "stop", "selector": by_name("open") }),
    ));
    catalogue.push((
        Some("ai:test"),
        json!({ "type": "restart", "selector": by_name("open") }),
    ));
    catalogue.push((
        Some("ai:test"),
        json!({ "type": "stop", "selector": by_name("db") }),
    ));
    for (claim, request) in catalogue {
        let reply = daemon.rpc_dry_on(&daemon.socket(), "dry-all", claim, request.clone())
            ["response"]
            .clone();
        assert_eq!(reply["type"], "dry_run", "{request}: {reply}");
    }

    let after = snapshot(&daemon);
    assert_eq!(before.registry, after.registry);
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.children, after.children);
    assert_eq!(before.log_sizes, after.log_sizes);
    assert_eq!(before.audit, after.audit);
    let differing: Vec<&String> = before
        .files
        .keys()
        .chain(after.files.keys())
        .filter(|path| before.files.get(*path) != after.files.get(*path))
        .collect();
    assert!(differing.is_empty(), "files that changed: {differing:?}");
}
