//! The audit log through the real daemon (raw JSON, private home): what is recorded and what
//! never is, that a decision is on disk before an AI's action, and what happens when the log
//! cannot be written. The in-process pieces (a held writer, a test clock) are in `audit_gate.rs`.

mod common;

use common::daemon::{DaemonProc, Signal};
use oxidepm_core::{AppSpec, Runtime};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

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
    spec.args = vec!["306.5".into()];
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

fn ask(daemon: &DaemonProc, id: &str, claim: Option<&str>, request: Value) -> Value {
    daemon.rpc_on(&daemon.socket(), id, claim, request)["response"].clone()
}

fn ai(daemon: &DaemonProc, request: Value) -> Value {
    ask(daemon, "req-ai", Some("ai:test"), request)
}

fn human(daemon: &DaemonProc, request: Value) -> Value {
    ask(daemon, "req-human", None, request)
}

fn start_as_human(daemon: &DaemonProc, spec: Value) {
    let reply = human(daemon, json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "started", "{reply}");
}

fn assert_code(reply: &Value, code: &str) {
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], code, "{reply}");
}

fn pid_of(daemon: &DaemonProc, name: &str) -> Option<u64> {
    let reply = human(daemon, json!({ "type": "status" }));
    reply["apps"]
        .as_array()?
        .iter()
        .find(|a| a["spec"]["name"] == name)
        .and_then(|a| a["state"]["pid"].as_u64())
}

fn audit_dir(daemon: &DaemonProc) -> PathBuf {
    daemon.home().join("audit")
}

/// Every parsed line of the series of `file` in the audit directory, oldest first.
fn lines_of(dir: &Path, file: &str) -> Vec<Value> {
    let stem = file.trim_end_matches(".jsonl");
    let mut names: Vec<String> = (1..12).rev().map(|i| format!("{stem}.{i}.jsonl")).collect();
    names.push(file.to_string());
    let mut out = Vec::new();
    for name in names {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            out.extend(
                text.lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok()),
            );
        }
    }
    out
}

fn main_lines(daemon: &DaemonProc) -> Vec<Value> {
    lines_of(&audit_dir(daemon), "audit.jsonl")
}

fn security_lines(daemon: &DaemonProc) -> Vec<Value> {
    lines_of(&audit_dir(daemon), "audit-security.jsonl")
}

/// Wait (bounded) until `pick` finds something in the lines of `file`.
fn wait_for<T>(
    daemon: &DaemonProc,
    file: &str,
    what: &str,
    pick: impl Fn(&[Value]) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lines = lines_of(&audit_dir(daemon), file);
        if let Some(found) = pick(&lines) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no {what} in {file}; lines: {lines:#?}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn decision<'a>(lines: &'a [Value], op: &str, decision: &str) -> Option<&'a Value> {
    lines
        .iter()
        .find(|l| l["phase"] == "decision" && l["op"] == op && l["decision"] == decision)
}

fn daemon_status(daemon: &DaemonProc) -> Value {
    let reply = human(daemon, json!({ "type": "daemon_status" }));
    assert_eq!(reply["type"], "daemon_status", "{reply}");
    reply
}

/// Wait (bounded) until `DaemonStatus.audit` says `want`.
fn wait_audit_state(daemon: &DaemonProc, want: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = daemon_status(daemon);
        if status["audit"] == want {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the audit state never became {want}: {status}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// A home whose `audit` path is a file, so the log can never be written.
fn spawn_with_blocked_audit(tmp: &TempDir) -> DaemonProc {
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("audit"), b"in the way").unwrap();
    DaemonProc::spawn(&home)
}

// ---- what is recorded ----------------------------------------------------------------------

#[test]
fn ai_actions_are_recorded_with_allow_and_deny() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "open", json!({ "ai_control": true })),
    );
    start_as_human(&daemon, app(&workdir(&tmp), "closed", json!({})));

    let allowed = ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("open") }),
    );
    assert_eq!(allowed["type"], "restarted", "{allowed}");
    let denied = ai(
        &daemon,
        json!({ "type": "stop", "selector": by_name("closed") }),
    );
    assert_code(&denied, "E_POLICY_DENIED");

    for file in ["audit.jsonl", "audit-security.jsonl"] {
        let lines = wait_for(&daemon, file, "both decisions", |lines| {
            let allow = decision(lines, "restart", "allow")?;
            let deny = decision(lines, "stop", "deny")?;
            Some((allow.clone(), deny.clone()))
        });
        let (allow, deny) = lines;
        assert_eq!(allow["actor"]["class"], "ai", "{file}: {allow}");
        assert_eq!(allow["actor"]["label"], "test");
        assert_eq!(allow["actor"]["via"], "main");
        assert_eq!(allow["tier"], "caution");
        assert_eq!(allow["selector"], "open");
        assert_eq!(allow["targets"][0]["name"], "open");
        assert_eq!(allow["v"], 1);
        assert_eq!(deny["deny"]["code"], "E_POLICY_DENIED", "{deny}");
        assert!(
            deny["deny"]["why"].as_str().unwrap().contains("closed"),
            "{deny}"
        );
        assert_eq!(deny["selector"], "closed");
    }
}

#[test]
fn human_safe_requests_are_not_recorded() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "quiet", json!({})));
    // the start is recorded; take the file as it is now and make only safe requests
    let before = wait_for(&daemon, "audit.jsonl", "the start", |l| {
        (l.len() >= 2).then_some(l.len())
    });
    for request in [
        json!({ "type": "ping" }),
        json!({ "type": "hello", "client": "t" }),
        json!({ "type": "status" }),
        json!({ "type": "show", "selector": by_name("quiet") }),
        json!({ "type": "describe", "selector": by_name("quiet") }),
        json!({ "type": "logs", "selector": by_name("quiet"), "lines": 5, "follow": false, "stdout": true, "stderr": true }),
        json!({ "type": "daemon_status" }),
    ] {
        let reply = human(&daemon, request.clone());
        assert_ne!(reply["type"], "error", "{request}: {reply}");
    }
    // let the writer's idle tick pass, then count again
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        main_lines(&daemon).len(),
        before,
        "safe requests of a person write nothing"
    );
}

#[test]
fn human_dangerous_requests_are_recorded() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "doomed", json!({})));
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("doomed") })
        )["type"],
        "stopped"
    );
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "delete", "selector": by_name("doomed") })
        )["type"],
        "deleted"
    );

    let main = wait_for(&daemon, "audit.jsonl", "the delete", |l| {
        decision(l, "delete", "allow").map(|_| l.to_vec())
    });
    for op in ["start", "stop", "delete"] {
        let d = decision(&main, op, "allow").unwrap_or_else(|| panic!("no {op}: {main:#?}"));
        assert_eq!(d["actor"]["class"], "human");
        assert_eq!(d["actor"]["label"], "cli");
    }
    // dangerous requests are in the security file, a person's caution request is not
    let security = wait_for(&daemon, "audit-security.jsonl", "the delete", |l| {
        decision(l, "delete", "allow").map(|_| l.to_vec())
    });
    assert!(
        decision(&security, "start", "allow").is_some(),
        "{security:#?}"
    );
    assert!(
        decision(&security, "stop", "allow").is_none(),
        "{security:#?}"
    );
}

#[test]
fn request_id_links_decision_and_result() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "linked", json!({ "ai_control": true })),
    );
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "restart", "selector": by_name("linked") })
        )["type"],
        "restarted"
    );

    let (decision_line, result) = wait_for(
        &daemon,
        "audit.jsonl",
        "the result of the restart",
        |lines| {
            let d = decision(lines, "restart", "allow")?;
            let r = lines
                .iter()
                .find(|l| l["phase"] == "result" && l["req"] == d["req"])?;
            Some((d.clone(), r.clone()))
        },
    );
    assert!(decision_line["req"].as_str().unwrap().starts_with('r'));
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["changed"], 1);
    assert_eq!(result["failed"], 0);
    assert!(result["ms"].is_u64(), "{result}");
    assert!(
        result["seq"].as_u64() > decision_line["seq"].as_u64(),
        "the decision comes first"
    );
    // every sequence number is used once, in order
    let seqs: Vec<u64> = main_lines(&daemon)
        .iter()
        .map(|l| l["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
}

#[test]
fn client_request_id_cannot_collide_with_another_request() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    for name in ["one", "two"] {
        let reply = ask(
            &daemon,
            "same-id",
            None,
            json!({ "type": "start", "spec": app(&workdir(&tmp), name, json!({})) }),
        );
        assert_eq!(reply["type"], "started", "{reply}");
    }
    let lines = wait_for(&daemon, "audit.jsonl", "two results", |l| {
        (l.iter().filter(|x| x["phase"] == "result").count() >= 2).then(|| l.to_vec())
    });
    let decisions: Vec<&Value> = lines.iter().filter(|l| l["phase"] == "decision").collect();
    assert_eq!(decisions.len(), 2);
    assert!(
        decisions.iter().all(|d| d["client_req"] == "same-id"),
        "kept as a label"
    );
    assert_ne!(
        decisions[0]["req"], decisions[1]["req"],
        "the daemon's own numbers differ"
    );
    for d in decisions {
        let results = lines
            .iter()
            .filter(|l| l["phase"] == "result" && l["req"] == d["req"])
            .count();
        assert_eq!(results, 1, "one result for {}", d["req"]);
    }
    // a client id of any length or content is kept only as a short label
    let long = "x".repeat(300);
    ask(
        &daemon,
        &long,
        None,
        json!({ "type": "stop", "selector": by_name("one") }),
    );
    let lines = wait_for(&daemon, "audit.jsonl", "the stop", |l| {
        decision(l, "stop", "allow").map(|_| l.to_vec())
    });
    let stop = decision(&lines, "stop", "allow").unwrap();
    assert_eq!(stop["client_req"].as_str().unwrap().len(), 64);
}

#[test]
fn record_has_program_digest_argc_but_not_args() {
    use sha2::{Digest, Sha256};
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let mut spec = app(&workdir(&tmp), "withargs", json!({}));
    spec["args"] = json!(["306.6", "--token=ARGSECRET-8c1f", "last"]);
    start_as_human(&daemon, spec);

    let lines = wait_for(&daemon, "audit.jsonl", "the start", |l| {
        decision(l, "start", "allow").map(|_| l.to_vec())
    });
    let start = decision(&lines, "start", "allow").unwrap();
    assert_eq!(start["program"], "sleep");
    assert_eq!(start["argc"], 3);
    let mut hash = Sha256::new();
    for arg in ["306.6", "--token=ARGSECRET-8c1f", "last"] {
        hash.update(arg.as_bytes());
        hash.update([0u8]);
    }
    let expected: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(start["args_sha256"], expected.as_str());
    for file in std::fs::read_dir(audit_dir(&daemon)).unwrap() {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap();
        assert!(
            !text.contains("ARGSECRET") && !text.contains("306.6"),
            "an argument was written: {text}"
        );
    }
}

#[test]
fn record_never_contains_env_values_or_scripts() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let spec = app(
        &workdir(&tmp),
        "secretive",
        json!({
            "ai_control": true,
            "env": { "SECRET": "hunter2", "TOKEN": "tok-9f8e7d6c" },
            "hooks": { "on_start": "true # hook-script-xyzzy", "on_stop": "true # stop-script-plugh" },
        }),
    );
    start_as_human(&daemon, spec);
    // every way to look at it, and a change
    let reveal = daemon.rpc_with(
        json!({ "type": "show", "selector": by_name("secretive") }),
        true,
    );
    assert!(
        reveal.to_string().contains("hunter2"),
        "the person did get the value"
    );
    daemon.rpc_with(
        json!({ "type": "describe", "selector": by_name("secretive") }),
        true,
    );
    ai(
        &daemon,
        json!({ "type": "show", "selector": by_name("secretive") }),
    );
    ai(
        &daemon,
        json!({ "type": "describe", "selector": by_name("secretive") }),
    );
    ai(
        &daemon,
        json!({ "type": "logs", "selector": by_name("secretive"), "lines": 5, "follow": false, "stdout": true, "stderr": true }),
    );
    ai(
        &daemon,
        json!({ "type": "show", "selector": by_name("secretive") }),
    );
    ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("secretive") }),
    );
    human(
        &daemon,
        json!({ "type": "delete", "selector": by_name("secretive") }),
    );

    // end the daemon so that every queued line is on disk
    let mut daemon = daemon;
    human(&daemon, json!({ "type": "kill" }));
    daemon.wait_exit();
    let mut seen = 0;
    for entry in std::fs::read_dir(audit_dir(&daemon)).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        seen += text.lines().count();
        for secret in [
            "hunter2",
            "tok-9f8e7d6c",
            "hook-script-xyzzy",
            "stop-script-plugh",
        ] {
            assert!(!text.contains(secret), "{secret} was written: {text}");
        }
    }
    assert!(
        seen >= 12,
        "the log has the requests ({seen} lines), the test is not vacuous"
    );
    // the whole home outside the database and the app's own files: the audit log is the point
    let names = main_lines(&daemon);
    assert!(
        decision(&names, "show --reveal", "allow").is_some(),
        "the reveal is recorded: {names:#?}"
    );
}

#[test]
fn ai_logs_reads_are_recorded_one_line_each() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "readme", json!({})));
    for _ in 0..3 {
        let reply = ai(
            &daemon,
            json!({ "type": "logs", "selector": by_name("readme"), "lines": 5, "follow": false, "stdout": true, "stderr": true }),
        );
        assert_eq!(reply["type"], "log_lines", "{reply}");
    }
    let lines = wait_for(&daemon, "audit.jsonl", "three reads", |l| {
        (l.iter()
            .filter(|x| x["phase"] == "decision" && x["op"] == "logs")
            .count()
            >= 3)
            .then(|| l.to_vec())
    });
    let reads: Vec<&Value> = lines.iter().filter(|l| l["op"] == "logs").collect();
    assert_eq!(reads.len(), 3, "one line each: {reads:#?}");
    assert!(reads
        .iter()
        .all(|r| r["selector"] == "readme" && r["tier"] == "safe" && r["actor"]["class"] == "ai"));
    assert!(
        security_lines(&daemon).iter().all(|l| l["op"] != "logs"),
        "reads stay out of the security file"
    );
}

#[test]
fn kill_is_recorded_with_its_result_before_the_daemon_exits() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "victim", json!({})));
    assert_eq!(human(&daemon, json!({ "type": "kill" }))["type"], "ok");
    daemon.wait_exit();
    for lines in [main_lines(&daemon), security_lines(&daemon)] {
        let kill = decision(&lines, "kill", "allow").unwrap_or_else(|| panic!("{lines:#?}"));
        let result = lines
            .iter()
            .find(|l| l["phase"] == "result" && l["req"] == kill["req"]);
        assert_eq!(result.map(|r| &r["ok"]), Some(&json!(true)), "{lines:#?}");
    }
}

/// Repeated polls of a non-human actor are held back to one line a minute; the daemon writes the
/// line when it goes, so nothing counted is lost.
#[test]
fn shutdown_writes_the_folded_polling_lines() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    for _ in 0..3 {
        assert_eq!(ai(&daemon, json!({ "type": "ping" }))["type"], "pong");
    }
    for _ in 0..4 {
        assert_eq!(ai(&daemon, json!({ "type": "status" }))["type"], "status");
    }
    // folded, not written one by one
    std::thread::sleep(Duration::from_millis(1500));
    assert!(main_lines(&daemon).is_empty(), "{:#?}", main_lines(&daemon));
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    let lines = main_lines(&daemon);
    let count = |op: &str| {
        lines
            .iter()
            .find(|l| l["op"] == op)
            .map(|l| l["count"].as_u64())
    };
    assert_eq!(count("ping"), Some(Some(3)), "{lines:#?}");
    assert_eq!(count("status"), Some(Some(4)), "{lines:#?}");
    assert!(lines
        .iter()
        .all(|l| l["actor"]["class"] == "ai" && l["actor"]["via"] == "main"));
}

#[test]
fn denials_are_recorded_on_the_agent_socket_too() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "target", json!({})));
    let reply = daemon.rpc_on(
        &daemon.agent_socket(),
        "agent-del",
        None,
        json!({ "type": "delete", "selector": by_name("target") }),
    )["response"]
        .clone();
    assert_code(&reply, "E_POLICY_DENIED");
    let line = wait_for(&daemon, "audit-security.jsonl", "the denial", |l| {
        decision(l, "delete", "deny").cloned()
    });
    assert_eq!(line["actor"]["via"], "agent");
    assert_eq!(line["actor"]["label"], "agent");
    assert_eq!(line["client_req"], "agent-del");
}

// ---- when the log cannot be written --------------------------------------------------------

#[test]
fn daemon_boots_when_audit_path_is_a_file() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_with_blocked_audit(&tmp);
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    let status = daemon_status(&daemon);
    assert_eq!(status["audit"], "degraded", "{status}");
    let warnings = status["warnings"].to_string();
    assert!(warnings.contains("audit directory"), "{warnings}");
    assert_eq!(
        std::fs::read(daemon.home().join("audit")).unwrap(),
        b"in the way",
        "left alone"
    );
}

#[test]
fn ai_caution_is_refused_when_audit_is_unwritable() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_with_blocked_audit(&tmp);
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "steady", json!({ "ai_control": true })),
    );
    let pid = pid_of(&daemon, "steady").expect("running");

    let reply = ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("steady") }),
    );
    assert_code(&reply, "E_AUDIT_UNAVAILABLE");
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": by_name("steady") }),
    );
    assert_code(&reply, "E_AUDIT_UNAVAILABLE");
    assert_eq!(pid_of(&daemon, "steady"), Some(pid), "nothing ran");
    // a refusal by the policy is still the policy's answer
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "delete", "selector": by_name("steady") }),
        ),
        "E_POLICY_DENIED",
    );
}

#[test]
fn ai_safe_read_proceeds_when_audit_is_unwritable_and_is_counted() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_with_blocked_audit(&tmp);
    start_as_human(&daemon, app(&workdir(&tmp), "watched", json!({})));
    let reply = ai(
        &daemon,
        json!({ "type": "show", "selector": by_name("watched") }),
    );
    assert_eq!(reply["type"], "show", "{reply}");
    let reply = ai(&daemon, json!({ "type": "status" }));
    assert_eq!(reply["type"], "status", "{reply}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lost = daemon_status(&daemon)["audit_lost"].as_u64().unwrap();
        if lost >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the lost line was never counted");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn human_stop_works_when_audit_is_unwritable_and_status_says_degraded() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_with_blocked_audit(&tmp);
    start_as_human(&daemon, app(&workdir(&tmp), "incident", json!({})));
    let started = Instant::now();
    let reply = human(
        &daemon,
        json!({ "type": "stop", "selector": by_name("incident") }),
    );
    assert_eq!(reply["type"], "stopped", "{reply}");
    assert_eq!(reply["count"], 1);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a person is not held up by the audit"
    );
    let status = wait_audit_state(&daemon, "degraded");
    assert!(
        !status["warnings"].as_array().unwrap().is_empty(),
        "{status}"
    );
}

#[test]
fn shutdown_completes_within_budget_with_audit_writer_blocked() {
    use nix::sys::stat::Mode;
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    // a named pipe nobody reads: opening it for writing blocks the writer thread for good
    std::fs::create_dir_all(home.join("audit")).unwrap();
    nix::unistd::mkfifo(
        &home.join("audit/audit.jsonl"),
        Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "stuck", json!({ "ai_control": true })),
    );
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "show", "selector": by_name("stuck") })
        )["type"],
        "show"
    );
    // a waited-for record times out after its two seconds instead of hanging the request
    let started = Instant::now();
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": by_name("stuck") }),
    );
    assert_code(&reply, "E_AUDIT_UNAVAILABLE");
    assert!(
        started.elapsed() >= Duration::from_secs(1) && started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        pid_of(&daemon, "stuck").is_some(),
        "the app was not stopped"
    );

    let started = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit_for(Duration::from_secs(15));
    assert!(status.success(), "{status}");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the drain gave up after its second: {:?}",
        started.elapsed()
    );
}

// ---- rates ---------------------------------------------------------------------------------

#[test]
fn looping_agent_is_rate_limited_and_cannot_rotate_the_security_file() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "target", json!({})));

    // the evidence: an early denial
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("target") }),
        ),
        "E_POLICY_DENIED",
    );
    // a loop of reads is limited by the general rate
    let mut limited_reads = 0;
    for _ in 0..400 {
        let reply = ai(
            &daemon,
            json!({ "type": "show", "selector": by_name("target") }),
        );
        if reply["code"] == "E_RATE_LIMITED" {
            limited_reads += 1;
        }
    }
    assert!(limited_reads > 0, "400 reads in a row went through");
    // and a loop of refused changes by the slower one for requests that change something
    let mut limited_changes = 0;
    for _ in 0..60 {
        let reply = ai(
            &daemon,
            json!({ "type": "delete", "selector": by_name("target") }),
        );
        if reply["code"] == "E_RATE_LIMITED" {
            limited_changes += 1;
        }
    }
    assert!(
        limited_changes >= 40,
        "{limited_changes} of 60 were limited"
    );

    let mut daemon = daemon;
    human(&daemon, json!({ "type": "kill" }));
    daemon.wait_exit();
    let security = security_lines(&daemon);
    let first = security.first().expect("the security file has lines");
    assert_eq!(
        first["op"], "start",
        "the person's start came first: {first}"
    );
    assert!(
        decision(&security, "stop", "deny").is_some(),
        "the early denial is still there: {security:#?}"
    );
    assert!(
        security.len() < 40,
        "the loop did not flood the security file: {} lines",
        security.len()
    );
    assert!(
        !audit_dir(&daemon).join("audit-security.1.jsonl").exists(),
        "it did not rotate"
    );
    // the refusals are counted in one line per actor per minute
    let main = main_lines(&daemon);
    let limited = main
        .iter()
        .find(|l| l["phase"] == "rate_limited")
        .unwrap_or_else(|| panic!("{main:#?}"));
    assert!(limited["count"].as_u64().unwrap() >= 40, "{limited}");
    assert_eq!(limited["actor"]["class"], "ai");
}

// ---- DaemonStatus --------------------------------------------------------------------------

#[test]
fn daemon_status_reports_audit_and_agent_socket() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let status = daemon_status(&daemon);
    assert_eq!(status["audit"], "ok", "{status}");
    assert_eq!(status["agent_socket"], true);
    assert_eq!(status["warnings"], json!([]));
    assert_eq!(status["audit_lost"], 0);
    // a safe request, so an AI may ask too
    let reply = daemon.rpc_on(
        &daemon.agent_socket(),
        "agent-ds",
        None,
        json!({ "type": "daemon_status" }),
    );
    assert_eq!(reply["response"]["type"], "daemon_status", "{reply}");
    // asking for the status does not create the audit directory for a person
    assert!(!audit_dir(&daemon).exists());
}

#[test]
fn daemon_status_says_when_the_agent_socket_could_not_be_bound() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("agent.sock"), b"not a socket").unwrap();
    let daemon = DaemonProc::spawn(&home);
    let status = daemon_status(&daemon);
    assert_eq!(status["agent_socket"], false, "{status}");
    assert!(
        status["warnings"].to_string().contains("agent socket"),
        "{status}"
    );
    assert_eq!(status["audit"], "ok");

    // switched off on purpose: not bound, and nothing to warn about
    let home2 = tmp.path().join("home2");
    std::fs::create_dir_all(&home2).unwrap();
    std::fs::write(home2.join("daemon.toml"), "agent_socket = false\n").unwrap();
    let daemon2 = DaemonProc::spawn(&home2);
    let status = daemon_status(&daemon2);
    assert_eq!(status["agent_socket"], false);
    assert_eq!(status["warnings"], json!([]), "{status}");
}
