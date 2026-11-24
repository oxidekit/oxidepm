//! Who is asking: the class the daemon gives a request, whatever the client wrote in the
//! envelope. Real daemon, raw JSON, private home.

mod common;

use common::daemon::DaemonProc;
use serde_json::json;
use tempfile::TempDir;

fn stop_nothing() -> serde_json::Value {
    json!({ "type": "stop", "selector": { "by": "name", "value": "nobody-here" } })
}

#[test]
fn no_claim_is_human_cli() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    daemon.rpc_on(&daemon.socket(), "req-human-1", None, stop_nothing());
    let line = daemon.wait_stderr_line(&["req-human-1"]);
    assert!(line.contains("actor=human:cli"), "{line}");
    assert!(line.contains("class=human"), "{line}");
    assert!(line.contains("via=main"), "{line}");

    // a human claim keeps the class and names the caller
    daemon.rpc_on(
        &daemon.socket(),
        "req-human-2",
        Some("human:alice"),
        stop_nothing(),
    );
    let line = daemon.wait_stderr_line(&["req-human-2"]);
    assert!(
        line.contains("actor=human:alice") && line.contains("class=human"),
        "{line}"
    );
}

#[test]
fn ai_claim_is_ai_class() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    daemon.rpc_on(
        &daemon.socket(),
        "req-ai-1",
        Some("ai:test"),
        stop_nothing(),
    );
    let line = daemon.wait_stderr_line(&["req-ai-1"]);
    assert!(
        line.contains("actor=ai:test") && line.contains("class=ai") && line.contains("via=main"),
        "{line}"
    );

    // a claim that is not human: or ai: (an adapter or system claim, a bad label) is stricter,
    // never looser
    for (id, claim) in [
        ("req-bad-1", "adapter:ui-web"),
        ("req-bad-2", "system:cron"),
        ("req-bad-3", "ai:has space"),
        ("req-bad-4", "root"),
    ] {
        daemon.rpc_on(&daemon.socket(), id, Some(claim), stop_nothing());
        let line = daemon.wait_stderr_line(&[id]);
        assert!(
            line.contains("actor=ai:invalid-claim") && line.contains("class=ai"),
            "{claim}: {line}"
        );
    }
}

/// The class the daemon decided reaches the supervisor and the lifecycle event, through the
/// real request path (the event line of the debug log names the actor).
#[test]
fn request_actor_reaches_the_lifecycle_event() {
    use oxidepm_core::{AppSpec, Runtime};

    let tmp = TempDir::new().unwrap();
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let daemon =
        DaemonProc::spawn_with_env(&tmp.path().join("home"), &[("RUST_LOG", "oxidepmd=debug")]);

    let mut spec = AppSpec::new("actor-app".into(), Runtime::CMD, "sleep".into(), workdir);
    spec.args = vec!["304.1".into()];
    // an ai actor may stop only what is marked for it
    spec.ai_control = true;
    let started = daemon.rpc_on(
        &daemon.socket(),
        "req-start",
        Some("human:alice"),
        json!({ "type": "start", "spec": spec }),
    );
    assert_eq!(started["response"]["type"], "started", "{started}");
    let stopped = daemon.rpc_on(
        &daemon.socket(),
        "req-stop",
        Some("ai:test"),
        json!({ "type": "stop", "selector": { "by": "name", "value": "actor-app" } }),
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
    assert_eq!(stopped["response"]["type"], "stopped", "{stopped}");

    let line = daemon.wait_stderr_line(&["event #", "Started", "actor-app"]);
    assert!(line.contains("human:alice"), "{line}");
    let line = daemon.wait_stderr_line(&["event #", "Stopped", "actor-app"]);
    assert!(line.contains("ai:test"), "{line}");
}

// ---- the agent socket (16b) ------------------------------------------------------------

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn ping_line(id: &str) -> String {
    format!("{{\"protocol\":2,\"request_id\":\"{id}\",\"request\":{{\"type\":\"ping\"}}}}\n")
}

#[test]
fn agent_socket_exists_with_mode_0600() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let meta = std::fs::metadata(daemon.agent_socket()).expect("agent.sock exists");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert!(std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()));
    let reply = daemon.rpc_on(
        &daemon.agent_socket(),
        "agent-ping",
        None,
        json!({ "type": "ping" }),
    );
    assert_eq!(reply["response"]["type"], "pong", "{reply}");
}

#[test]
fn agent_socket_connection_is_ai_class_without_claim() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    daemon.rpc_on(&daemon.agent_socket(), "req-agent-1", None, stop_nothing());
    let line = daemon.wait_stderr_line(&["req-agent-1"]);
    assert!(line.contains("actor=ai:agent"), "{line}");
    assert!(
        line.contains("class=ai") && line.contains("via=agent"),
        "{line}"
    );
}

#[test]
fn agent_socket_ignores_human_claim() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    for (id, claim, actor) in [
        ("req-agent-h1", "human:alice", "actor=ai:agent"),
        ("req-agent-h2", "human:cli", "actor=ai:agent"),
        ("req-agent-h3", "system:cron", "actor=ai:agent"),
        ("req-agent-a1", "ai:other-agent", "actor=ai:other-agent"),
    ] {
        daemon.rpc_on(&daemon.agent_socket(), id, Some(claim), stop_nothing());
        let line = daemon.wait_stderr_line(&[id]);
        assert!(
            line.contains(actor) && line.contains("class=ai"),
            "{claim}: {line}"
        );
        assert!(!line.contains("class=human"), "{claim}: {line}");
    }
}

#[test]
fn main_socket_connection_is_still_human() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    daemon.rpc_on(&daemon.socket(), "req-main-1", None, stop_nothing());
    let line = daemon.wait_stderr_line(&["req-main-1"]);
    assert!(
        line.contains("actor=human:cli")
            && line.contains("class=human")
            && line.contains("via=main"),
        "{line}"
    );
}

#[test]
fn agent_socket_disabled_by_config_is_not_bound() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("daemon.toml"), "agent_socket = false\n").unwrap();
    let daemon = DaemonProc::spawn(&home);
    let reply = daemon.rpc(json!({ "type": "ping" }));
    assert_eq!(reply["type"], "pong");
    assert!(!daemon.agent_socket().exists(), "agent.sock must not exist");
}

#[test]
fn agent_socket_bind_failure_does_not_stop_the_daemon() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // a file that is not a socket is in the way
    std::fs::write(home.join("agent.sock"), b"not a socket").unwrap();
    let daemon = DaemonProc::spawn(&home);

    let reply = daemon.rpc(json!({ "type": "ping" }));
    assert_eq!(reply["type"], "pong", "the main socket works");
    let line = daemon.wait_stderr_line(&["agent socket"]);
    assert!(
        line.contains("not available") || line.contains("unavailable"),
        "{line}"
    );
    assert_eq!(
        std::fs::read(home.join("agent.sock")).unwrap(),
        b"not a socket",
        "the file is left alone"
    );
}

/// A request line of 2 MiB is refused on the agent socket (its limit is 1 MiB) and still fine on
/// the main one; the 33rd concurrent connection is closed at once.
#[test]
fn agent_listener_caps_line_length_and_connections() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let mut long = vec![b'x'; 2 * 1024 * 1024];
    long.push(b'\n');
    let (replies, closed) = daemon.raw_exchange_on(&daemon.agent_socket(), long.clone(), 1);
    assert_eq!(
        replies[0]["response"]["code"], "E_REQUEST_TOO_LARGE",
        "{replies:?}"
    );
    assert!(
        replies[0]["response"]["message"]
            .as_str()
            .unwrap()
            .contains("1 MiB"),
        "{replies:?}"
    );
    assert!(
        closed,
        "the connection is closed after a line that is too long"
    );
    // the main socket takes the same line (it is not JSON, so it is a bad request, not too large)
    let mut stream = UnixStream::connect(daemon.socket()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(&long).unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).expect("read");
    assert!(line.contains("E_BAD_REQUEST"), "{line}");

    // 32 connections that are served and kept open
    let mut open = Vec::new();
    for i in 0..32 {
        let mut stream = UnixStream::connect(daemon.agent_socket()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(ping_line(&format!("held-{i}")).as_bytes())
            .unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        assert!(line.contains("pong"), "{line}");
        open.push(stream);
    }
    // the 33rd is accepted by the kernel and closed by the daemon without an answer
    let mut extra = UnixStream::connect(daemon.agent_socket()).expect("connect");
    extra
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _ = extra.write_all(ping_line("extra").as_bytes());
    let mut line = String::new();
    let read = BufReader::new(&extra).read_line(&mut line);
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the 33rd connection got an answer: {line:?}"
    );
    // the main socket is not affected by the agent's cap
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    // and a freed slot can be used again
    drop(open.pop());
    let mut again = None;
    for _ in 0..50 {
        let mut stream = UnixStream::connect(daemon.agent_socket()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = stream.write_all(ping_line("again").as_bytes());
        let mut line = String::new();
        if matches!(BufReader::new(&stream).read_line(&mut line), Ok(n) if n > 0) {
            again = Some(line);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(again.expect("a slot was freed").contains("pong"));
}

/// An agent connection that sends nothing for 60 seconds is closed.
#[cfg(feature = "slow-tests")]
#[test]
fn agent_connection_idle_timeout() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let started = std::time::Instant::now();
    let stream = UnixStream::connect(daemon.agent_socket()).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .unwrap();
    let mut line = String::new();
    let read = BufReader::new(&stream).read_line(&mut line);
    let waited = started.elapsed();
    assert!(matches!(read, Ok(0) | Err(_)), "{line:?}");
    assert!(
        waited >= Duration::from_secs(59) && waited < Duration::from_secs(75),
        "closed after {waited:?}, expected about 60 s"
    );
    // a main-socket connection is not closed by then (its idle timeout is 300 s)
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
}

/// The socket is the daemon's: it goes with a clean exit, and a daemon that was killed leaves a
/// stale one that the next daemon replaces.
#[test]
fn agent_socket_is_removed_on_exit_and_a_stale_one_is_replaced() {
    use common::daemon::Signal;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);
    assert!(daemon.agent_socket().exists());
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    assert!(
        !home.join("agent.sock").exists(),
        "a clean exit removes agent.sock"
    );

    let mut daemon = DaemonProc::spawn(&home);
    daemon.sigkill();
    daemon.wait_exit();
    assert!(
        home.join("agent.sock").exists(),
        "a killed daemon leaves its socket file behind"
    );

    let daemon = DaemonProc::spawn(&home);
    let reply = daemon.rpc_on(
        &daemon.agent_socket(),
        "agent-after-kill",
        None,
        json!({ "type": "ping" }),
    );
    assert_eq!(reply["response"]["type"], "pong", "{reply}");
}

// ---- tiers, markers and the gate (17) ----------------------------------------------------

use oxidepm_core::{AppSpec, Runtime};
use std::path::Path;

/// A `sleep` app as the JSON a client sends, with `extra` merged over the spec (the markers, a
/// tag, a cluster size).
fn app(workdir: &Path, name: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut spec = AppSpec::new(
        name.into(),
        Runtime::CMD,
        "sleep".into(),
        workdir.to_path_buf(),
    );
    spec.args = vec!["305.5".into()];
    let mut value = serde_json::to_value(spec).unwrap();
    if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        for (key, val) in extra {
            base.insert(key.clone(), val.clone());
        }
    }
    value
}

fn by_name(name: &str) -> serde_json::Value {
    json!({ "by": "name", "value": name })
}

/// The answer to `request` from `claim` (none: a human without a claim) on the main socket.
fn ask(daemon: &DaemonProc, claim: Option<&str>, request: serde_json::Value) -> serde_json::Value {
    daemon.rpc_on(&daemon.socket(), "req-gate", claim, request)["response"].clone()
}

fn ai(daemon: &DaemonProc, request: serde_json::Value) -> serde_json::Value {
    ask(daemon, Some("ai:test"), request)
}

fn human(daemon: &DaemonProc, request: serde_json::Value) -> serde_json::Value {
    ask(daemon, None, request)
}

fn start_as_human(daemon: &DaemonProc, spec: serde_json::Value) {
    let reply = human(daemon, json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "started", "{reply}");
}

/// `(name, status, pid)` of every app, from a human `status`.
fn state(daemon: &DaemonProc) -> Vec<(String, String, Option<u64>)> {
    let reply = human(daemon, json!({ "type": "status" }));
    assert_eq!(reply["type"], "status", "{reply}");
    let mut out: Vec<_> = reply["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["spec"]["name"].as_str().unwrap().to_string(),
                a["state"]["status"].as_str().unwrap().to_string(),
                a["state"]["pid"].as_u64(),
            )
        })
        .collect();
    out.sort();
    out
}

fn assert_code(reply: &serde_json::Value, code: &str) {
    assert_eq!(reply["type"], "error", "expected {code}, got {reply}");
    assert_eq!(reply["code"], code, "{reply}");
}

fn workdir(tmp: &TempDir) -> std::path::PathBuf {
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn ai_cannot_delete_flush_or_kill() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    // even with every marker an AI could want
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "keep", json!({ "ai_control": true })),
    );
    let before = state(&daemon);
    assert!(before[0].2.is_some(), "the app runs: {before:?}");

    for socket in [daemon.socket(), daemon.agent_socket()] {
        for request in [
            json!({ "type": "delete", "selector": by_name("keep") }),
            json!({ "type": "flush", "selector": by_name("keep") }),
            json!({ "type": "kill" }),
        ] {
            let reply = daemon.rpc_on(&socket, "req-deny", Some("ai:test"), request.clone())
                ["response"]
                .clone();
            assert_code(&reply, "E_POLICY_DENIED");
            let message = reply["message"].as_str().unwrap();
            assert!(
                message.contains("dangerous") && message.contains("ai:test"),
                "{message}"
            );
        }
    }
    assert_eq!(state(&daemon), before, "nothing changed");
    assert_eq!(
        human(&daemon, json!({ "type": "ping" }))["type"],
        "pong",
        "the daemon is up"
    );
}

#[test]
fn ai_cannot_reveal() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(
            &workdir(&tmp),
            "secret",
            json!({ "env": { "TOKEN": "hunter2" } }),
        ),
    );

    let reveal = |claim: Option<&str>, request: serde_json::Value| {
        let mut envelope = json!({
            "protocol": 2, "request_id": "req-reveal", "reveal": true, "request": request,
        });
        if let Some(claim) = claim {
            envelope["actor"] = json!(claim);
        }
        daemon.rpc_envelope(envelope)["response"].clone()
    };
    for request in [
        json!({ "type": "show", "selector": by_name("secret") }),
        json!({ "type": "describe", "selector": by_name("secret") }),
    ] {
        let denied = reveal(Some("ai:test"), request.clone());
        assert_code(&denied, "E_POLICY_DENIED");
        assert!(!denied.to_string().contains("hunter2"), "{denied}");
        // a human gets the value
        let shown = reveal(None, request);
        assert!(shown.to_string().contains("hunter2"), "{shown}");
    }
    // without reveal an AI reads the app, values redacted
    let plain = ai(
        &daemon,
        json!({ "type": "show", "selector": by_name("secret") }),
    );
    assert_eq!(plain["type"], "show", "{plain}");
    assert!(!plain.to_string().contains("hunter2"), "{plain}");
}

#[test]
fn ai_cannot_save_or_resurrect() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let daemon = DaemonProc::spawn(&home);
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "kept", json!({ "ai_control": true })),
    );
    let before = state(&daemon);

    for request in [
        json!({ "type": "save", "force": false }),
        json!({ "type": "save", "force": true }),
        json!({ "type": "resurrect" }),
    ] {
        assert_code(&ai(&daemon, request), "E_POLICY_DENIED");
    }
    assert!(!home.join("saved.json").exists(), "nothing was saved");
    assert_eq!(state(&daemon), before);
    // a human saves
    assert_eq!(
        human(&daemon, json!({ "type": "save", "force": false }))["type"],
        "saved"
    );
    assert!(home.join("saved.json").exists());
}

#[test]
fn ai_start_and_start_batch_are_always_denied() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    // an app that exists, opted in and unprotected: a start still carries a spec, and a spec is
    // arbitrary code
    start_as_human(&daemon, app(&work, "stored", json!({ "ai_control": true })));
    let stored = human(
        &daemon,
        json!({ "type": "show", "selector": by_name("stored") }),
    );
    let before = state(&daemon);

    let mut changed = app(&work, "stored", json!({ "ai_control": true }));
    changed["command"] = json!("touch");
    changed["args"] = json!([tmp.path().join("pwned").to_string_lossy()]);
    for socket in [daemon.socket(), daemon.agent_socket()] {
        for request in [
            json!({ "type": "start", "spec": changed }),
            json!({ "type": "start", "spec": app(&work, "brand-new", json!({})) }),
            json!({ "type": "start_batch", "specs": [changed, app(&work, "brand-new", json!({}))] }),
            json!({ "type": "start_batch", "specs": [] }),
        ] {
            let reply =
                daemon.rpc_on(&socket, "req-start", Some("ai:test"), request)["response"].clone();
            assert_code(&reply, "E_POLICY_DENIED");
        }
    }
    assert_eq!(state(&daemon), before, "no app was created or changed");
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "show", "selector": by_name("stored") })
        ),
        stored
    );
    assert!(!tmp.path().join("pwned").exists());
}

#[test]
fn ai_cannot_start_or_start_batch() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    // the error says what the rule is and who is refused
    let reply = ai(
        &daemon,
        json!({ "type": "start", "spec": app(&work, "x", json!({})) }),
    );
    assert_code(&reply, "E_POLICY_DENIED");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("start") && message.contains("dangerous") && message.contains("ai:test"),
        "{message}"
    );
    assert!(state(&daemon).is_empty());
    // the same spec from a human is fine
    start_as_human(&daemon, app(&work, "x", json!({})));
    assert_eq!(state(&daemon).len(), 1);
}

#[test]
fn ai_read_requests_work() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    // an app the AI has no rights over: reading it is still allowed (even when protected)
    start_as_human(&daemon, app(&workdir(&tmp), "plain", json!({})));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "guarded", json!({ "protected": true })),
    );

    for socket in [daemon.socket(), daemon.agent_socket()] {
        for (request, want) in [
            (json!({ "type": "ping" }), "pong"),
            (json!({ "type": "hello", "client": "test" }), "hello"),
            (json!({ "type": "status" }), "status"),
            (
                json!({ "type": "show", "selector": by_name("plain") }),
                "show",
            ),
            (
                json!({ "type": "show", "selector": by_name("guarded") }),
                "show",
            ),
            (
                json!({ "type": "describe", "selector": by_name("guarded") }),
                "described",
            ),
            (
                json!({ "type": "logs", "selector": by_name("plain"), "lines": 5, "follow": false,
                        "stdout": true, "stderr": true }),
                "log_lines",
            ),
        ] {
            let reply = daemon.rpc_on(&socket, "req-read", Some("ai:test"), request.clone())
                ["response"]
                .clone();
            assert_eq!(reply["type"], want, "{request}: {reply}");
        }
    }
}

#[test]
fn human_can_do_everything_without_claim() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let daemon = DaemonProc::spawn(&home);
    let work = workdir(&tmp);

    for claim in [None, Some("human:alice")] {
        let go = |request: serde_json::Value| ask(&daemon, claim, request);
        let name = format!("all-{}", claim.map_or("none", |_| "alice"));
        let reply = go(json!({ "type": "start", "spec": app(&work, &name, json!({})) }));
        assert_eq!(reply["type"], "started", "{reply}");
        let batch = go(
            json!({ "type": "start_batch", "specs": [app(&work, &format!("{name}-b"), json!({}))] }),
        );
        assert_eq!(batch["type"], "started_batch", "{batch}");
        for request in [
            json!({ "type": "status" }),
            json!({ "type": "show", "selector": by_name(&name) }),
            json!({ "type": "describe", "selector": by_name(&name) }),
            json!({ "type": "logs", "selector": by_name(&name), "lines": 5, "follow": false,
                    "stdout": true, "stderr": true }),
            json!({ "type": "restart", "selector": by_name(&name) }),
            json!({ "type": "reload", "selector": by_name(&name) }),
            json!({ "type": "flush", "selector": by_name(&name) }),
            json!({ "type": "stop", "selector": by_name(&name) }),
            json!({ "type": "save", "force": false }),
            json!({ "type": "resurrect" }),
            json!({ "type": "delete", "selector": by_name(&name) }),
            json!({ "type": "delete", "selector": { "by": "all" } }),
        ] {
            let reply = go(request.clone());
            assert_ne!(reply["type"], "error", "{claim:?} {request}: {reply}");
        }
    }
    // the human also revealed values and kills the daemon
    let mut daemon = daemon;
    let reply = human(&daemon, json!({ "type": "kill" }));
    assert_ne!(reply["type"], "error", "{reply}");
    assert!(daemon.wait_exit().success());
}

#[test]
fn unclassified_claim_prefix_is_treated_as_ai() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, app(&workdir(&tmp), "target", json!({})));
    let before = state(&daemon);
    for claim in [
        "robot:x",
        "adapter:ui-web",
        "system:cron",
        "root",
        "ai:has space",
        "human:",
        "HUMAN:me",
    ] {
        let reply = ask(
            &daemon,
            Some(claim),
            json!({ "type": "delete", "selector": by_name("target") }),
        );
        assert_code(&reply, "E_POLICY_DENIED");
    }
    assert_eq!(state(&daemon), before);
    // and the reads of such a claim work, as an AI's do
    let reply = ask(&daemon, Some("robot:x"), json!({ "type": "status" }));
    assert_eq!(reply["type"], "status");
}

// ---- the markers decide an ai actor's caution requests (17b) -----------------------------

fn pid_of(daemon: &DaemonProc, name: &str) -> Option<u64> {
    state(daemon)
        .into_iter()
        .find(|(n, ..)| n == name)
        .and_then(|(_, _, pid)| pid)
}

fn status_of(daemon: &DaemonProc, name: &str) -> String {
    state(daemon)
        .into_iter()
        .find(|(n, ..)| n == name)
        .map(|(_, s, _)| s)
        .unwrap_or_default()
}

/// Wait (bounded) until the app has a pid that is not `old`.
fn wait_new_pid(daemon: &DaemonProc, name: &str, old: Option<u64>) -> u64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = pid_of(daemon, name) {
            if Some(pid) != old {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{name} never got a new pid (was {old:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn ai_stop_without_ai_control_is_denied_and_nothing_stops() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "closed", json!({})));
    start_as_human(&daemon, app(&work, "open", json!({ "ai_control": true })));
    let before = state(&daemon);

    for request in [
        json!({ "type": "stop", "selector": by_name("closed") }),
        json!({ "type": "restart", "selector": by_name("closed") }),
        json!({ "type": "reload", "selector": by_name("closed") }),
    ] {
        let reply = ai(&daemon, request);
        assert_code(&reply, "E_POLICY_DENIED");
        let message = reply["message"].as_str().unwrap();
        assert!(
            message.contains("closed") && message.contains("ai_control"),
            "{message}"
        );
        assert!(
            !message.contains("open"),
            "only the denied app is named: {message}"
        );
    }
    // by id too
    let id = {
        let shown = human(
            &daemon,
            json!({ "type": "show", "selector": by_name("closed") }),
        );
        shown["app"]["spec"]["id"].as_u64().unwrap()
    };
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": { "by": "id", "value": id } }),
        ),
        "E_POLICY_DENIED",
    );
    assert_eq!(state(&daemon), before, "nothing stopped or restarted");
}

#[test]
fn ai_restart_with_ai_control_works() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "opted", json!({ "ai_control": true })),
    );
    let old = pid_of(&daemon, "opted");
    assert!(old.is_some());

    let reply = ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("opted") }),
    );
    assert_eq!(reply["type"], "restarted", "{reply}");
    assert_eq!(reply["count"], 1, "{reply}");
    wait_new_pid(&daemon, "opted", old);

    // stop and reload work too (on the agent socket as well)
    let reply = daemon.rpc_on(
        &daemon.agent_socket(),
        "r",
        None,
        json!({ "type": "stop", "selector": by_name("opted") }),
    );
    assert_eq!(reply["response"]["type"], "stopped", "{reply}");
    assert_eq!(status_of(&daemon, "opted"), "stopped");
}

#[test]
fn ai_restart_of_a_stopped_app_with_ai_control_starts_it() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "napping", json!({ "ai_control": true })),
    );
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("napping") })
        )["type"],
        "stopped"
    );
    assert_eq!(status_of(&daemon, "napping"), "stopped");

    let reply = ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("napping") }),
    );
    assert_eq!(reply["type"], "restarted", "{reply}");
    wait_new_pid(&daemon, "napping", None);
    assert_eq!(status_of(&daemon, "napping"), "running");
}

#[test]
fn protected_app_is_read_only_for_ai() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "vault", json!({ "protected": true })),
    );
    let before = state(&daemon);

    for request in [
        json!({ "type": "stop", "selector": by_name("vault") }),
        json!({ "type": "restart", "selector": by_name("vault") }),
        json!({ "type": "reload", "selector": by_name("vault") }),
    ] {
        let reply = ai(&daemon, request);
        assert_code(&reply, "E_PROTECTED");
        assert!(
            reply["message"].as_str().unwrap().contains("vault"),
            "{reply}"
        );
    }
    // it can still be read
    for request in [
        json!({ "type": "show", "selector": by_name("vault") }),
        json!({ "type": "status" }),
    ] {
        assert_ne!(ai(&daemon, request)["type"], "error");
    }
    assert_eq!(state(&daemon), before);
}

#[test]
fn protected_beats_ai_control() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(
            &workdir(&tmp),
            "both",
            json!({ "protected": true, "ai_control": true }),
        ),
    );
    let before = state(&daemon);
    for socket in [daemon.socket(), daemon.agent_socket()] {
        let reply = daemon.rpc_on(
            &socket,
            "r",
            Some("ai:test"),
            json!({ "type": "stop", "selector": by_name("both") }),
        )["response"]
            .clone();
        assert_code(&reply, "E_PROTECTED");
    }
    assert_eq!(state(&daemon), before);
}

#[test]
fn protected_app_can_be_stopped_by_human() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(
            &workdir(&tmp),
            "vault",
            json!({ "protected": true, "ai_control": true }),
        ),
    );
    let old = pid_of(&daemon, "vault");
    for claim in [None, Some("human:alice")] {
        let reply = ask(
            &daemon,
            claim,
            json!({ "type": "restart", "selector": by_name("vault") }),
        );
        assert_eq!(reply["type"], "restarted", "{reply}");
        wait_new_pid(&daemon, "vault", old);
    }
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("vault") })
        )["type"],
        "stopped"
    );
    assert_eq!(status_of(&daemon, "vault"), "stopped");
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "delete", "selector": by_name("vault") })
        )["type"],
        "deleted"
    );
}

#[test]
fn selector_all_and_tags_by_ai_are_refused_and_named_apps_are_all_or_nothing() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    start_as_human(
        &daemon,
        app(&work, "one", json!({ "ai_control": true, "tags": ["grp"] })),
    );
    start_as_human(
        &daemon,
        app(&work, "two", json!({ "ai_control": true, "tags": ["grp"] })),
    );
    start_as_human(&daemon, app(&work, "three", json!({ "tags": ["grp"] })));
    let before = state(&daemon);

    for verb in ["stop", "restart", "reload"] {
        for selector in [
            json!({ "by": "all" }),
            json!({ "by": "tag", "value": "grp" }),
        ] {
            let reply = ai(&daemon, json!({ "type": verb, "selector": selector }));
            assert_code(&reply, "E_POLICY_DENIED");
            assert!(
                reply["message"].as_str().unwrap().contains("name the apps"),
                "{reply}"
            );
        }
    }
    assert_eq!(state(&daemon), before, "nothing ran");

    // a named app that is not opted in is refused, and the opted-in ones are untouched
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("three") }),
        ),
        "E_POLICY_DENIED",
    );
    assert_eq!(state(&daemon), before);
    // the same selectors from a human work
    let reply = human(
        &daemon,
        json!({ "type": "stop", "selector": { "by": "tag", "value": "grp" } }),
    );
    assert_eq!(reply["count"], 3, "{reply}");
}

#[test]
fn tag_selector_by_ai_is_refused() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(
            &workdir(&tmp),
            "tagged",
            json!({ "ai_control": true, "tags": ["web"] }),
        ),
    );
    let before = state(&daemon);
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": { "by": "tag", "value": "web" } }),
    );
    assert_code(&reply, "E_POLICY_DENIED");
    // a tag that matches nothing is refused the same way, and does not say whether it exists
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": { "by": "tag", "value": "nothing-has-it" } }),
    );
    assert_code(&reply, "E_POLICY_DENIED");
    assert_eq!(state(&daemon), before);
}

/// The rows of `state.db` as `(name, desired_state, spec_json)`, read while the daemon runs.
fn db_rows(home: &Path) -> Vec<(String, String, String)> {
    use sqlx::Row;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let url = format!("sqlite://{}?mode=ro", home.join("state.db").display());
        let pool = sqlx::SqlitePool::connect(&url)
            .await
            .expect("open state.db read-only");
        let rows = sqlx::query("SELECT name, desired_state, spec_json FROM apps ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("read rows");
        pool.close().await;
        rows.iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    })
}

#[test]
fn denied_request_changes_no_state() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let daemon = DaemonProc::spawn(&home);
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "plain", json!({})));
    start_as_human(
        &daemon,
        app(
            &work,
            "vault",
            json!({ "protected": true, "ai_control": true }),
        ),
    );
    start_as_human(&daemon, app(&work, "open", json!({ "ai_control": true })));
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("open") })
        )["type"],
        "stopped"
    );

    let strip = |mut v: serde_json::Value| {
        // uptime and the live numbers move by themselves
        for app in v["apps"].as_array_mut().unwrap() {
            let s = app["state"].as_object_mut().unwrap();
            for key in ["uptime_secs", "cpu_percent", "memory_bytes"] {
                s.remove(key);
            }
        }
        v
    };
    let before = (
        strip(human(&daemon, json!({ "type": "status" }))),
        db_rows(&home),
    );

    let denied = [
        json!({ "type": "stop", "selector": by_name("plain") }),
        json!({ "type": "restart", "selector": by_name("plain") }),
        json!({ "type": "reload", "selector": by_name("vault") }),
        json!({ "type": "delete", "selector": by_name("vault") }),
        json!({ "type": "flush", "selector": by_name("plain") }),
        json!({ "type": "stop", "selector": { "by": "all" } }),
        json!({ "type": "save", "force": true }),
        json!({ "type": "resurrect" }),
        json!({ "type": "start", "spec": app(&work, "open", json!({ "ai_control": true })) }),
        json!({ "type": "start", "spec": app(&work, "newcomer", json!({})) }),
    ];
    for request in denied {
        let reply = ai(&daemon, request.clone());
        assert_eq!(reply["type"], "error", "{request}: {reply}");
        assert!(
            reply["code"] == "E_POLICY_DENIED" || reply["code"] == "E_PROTECTED",
            "{request}: {reply}"
        );
    }
    let after = (
        strip(human(&daemon, json!({ "type": "status" }))),
        db_rows(&home),
    );
    assert_eq!(after.0, before.0, "status is identical");
    assert_eq!(after.1, before.1, "the database rows are identical");
    assert!(!home.join("saved.json").exists());
}

#[test]
fn kill_by_ai_is_denied_and_the_daemon_stays_up() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "survivor", json!({ "ai_control": true })),
    );
    let before = state(&daemon);
    let pid = daemon.pid();
    for socket in [daemon.socket(), daemon.agent_socket()] {
        for claim in [Some("ai:test"), Some("robot:x"), None] {
            if claim.is_none() && socket == daemon.socket() {
                continue; // that one is a person
            }
            let reply = daemon.rpc_on(&socket, "kill", claim, json!({ "type": "kill" }))
                ["response"]
                .clone();
            assert_code(&reply, "E_POLICY_DENIED");
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(daemon.pid(), pid);
    assert_eq!(human(&daemon, json!({ "type": "ping" }))["type"], "pong");
    assert_eq!(state(&daemon), before, "the app is still running");
}

#[test]
fn restart_by_ai_has_a_per_app_cooldown() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "looped", json!({ "ai_control": true })));
    start_as_human(&daemon, app(&work, "other", json!({ "ai_control": true })));

    let first = ai(
        &daemon,
        json!({ "type": "restart", "selector": by_name("looped") }),
    );
    assert_eq!(first["type"], "restarted", "{first}");
    let pid = pid_of(&daemon, "looped");
    // inside the cooldown: restart and reload are refused, whoever of the ai class asks
    for request in [
        json!({ "type": "restart", "selector": by_name("looped") }),
        json!({ "type": "reload", "selector": by_name("looped") }),
    ] {
        for socket in [daemon.socket(), daemon.agent_socket()] {
            let reply = daemon.rpc_on(&socket, "again", Some("ai:other"), request.clone())
                ["response"]
                .clone();
            assert_code(&reply, "E_RATE_LIMITED");
            assert!(
                reply["message"].as_str().unwrap().contains("looped"),
                "{reply}"
            );
        }
    }
    assert_eq!(
        pid_of(&daemon, "looped"),
        pid,
        "the refused restarts did not happen"
    );
    // another app is not held back, a stop is not limited, a person is not limited
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "restart", "selector": by_name("other") })
        )["type"],
        "restarted"
    );
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("looped") })
        )["type"],
        "stopped"
    );
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "restart", "selector": by_name("looped") })
        )["type"],
        "restarted"
    );
}

#[test]
fn cron_restart_of_protected_app_still_happens() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(
            &workdir(&tmp),
            "scheduled",
            json!({ "protected": true, "restart_cron": "* * * * * *" }),
        ),
    );
    let first = wait_new_pid(&daemon, "scheduled", None);
    let second = wait_new_pid(&daemon, "scheduled", Some(first));
    assert_ne!(first, second, "the schedule restarted the protected app");
}

#[test]
fn crash_restart_of_protected_app_still_happens() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let mut spec = app(&workdir(&tmp), "crashy", json!({ "protected": true }));
    spec["command"] = json!("sh");
    spec["args"] = json!(["-c", "sleep 305.6"]);
    start_as_human(&daemon, spec);
    let old = pid_of(&daemon, "crashy").expect("running");
    // kill the child behind the daemon's back, as a crash
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(old as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    wait_new_pid(&daemon, "crashy", Some(old));
    assert_eq!(status_of(&daemon, "crashy"), "running");
}

#[test]
fn boot_resurrect_ignores_markers() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "revived", json!({ "protected": true })),
    );
    let old = pid_of(&daemon, "revived");
    daemon.signal(common::daemon::Signal::SIGTERM);
    assert!(daemon.wait_exit().success());

    let daemon = DaemonProc::spawn(&home);
    let pid = wait_new_pid(&daemon, "revived", old);
    assert!(pid > 0);
    assert_eq!(status_of(&daemon, "revived"), "running");
}

// ---- SetPolicy and the markers as one source of truth (17c) ------------------------------

fn set_policy(
    daemon: &DaemonProc,
    claim: Option<&str>,
    name: &str,
    protected: Option<bool>,
    ai_control: Option<bool>,
) -> serde_json::Value {
    ask(
        daemon,
        claim,
        json!({ "type": "set_policy", "selector": by_name(name), "protected": protected, "ai_control": ai_control }),
    )
}

/// `(protected, ai_control)` of an app, as `show` reports them.
fn markers_of(daemon: &DaemonProc, name: &str) -> (bool, bool) {
    let shown = human(daemon, json!({ "type": "show", "selector": by_name(name) }));
    assert_eq!(shown["type"], "show", "{shown}");
    (
        shown["app"]["spec"]["protected"].as_bool().unwrap(),
        shown["app"]["spec"]["ai_control"].as_bool().unwrap(),
    )
}

#[test]
fn set_policy_protects_a_running_app_without_restart() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "live", json!({ "ai_control": true })),
    );
    let pid = pid_of(&daemon, "live");
    assert!(pid.is_some());
    // an ai actor may stop it ... until a person protects it
    assert_eq!(markers_of(&daemon, "live"), (false, true));

    let reply = set_policy(&daemon, None, "live", Some(true), None);
    assert_eq!(reply["type"], "policy_set", "{reply}");
    assert_eq!(reply["count"], 1, "{reply}");
    assert_eq!(
        markers_of(&daemon, "live"),
        (true, true),
        "ai_control was left alone"
    );
    assert_eq!(pid_of(&daemon, "live"), pid, "the same process: no restart");
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("live") }),
        ),
        "E_PROTECTED",
    );

    // and back: unprotect, close ai_control
    let reply = set_policy(
        &daemon,
        Some("human:alice"),
        "live",
        Some(false),
        Some(false),
    );
    assert_eq!(reply["count"], 1, "{reply}");
    assert_eq!(markers_of(&daemon, "live"), (false, false));
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("live") }),
        ),
        "E_POLICY_DENIED",
    );
    assert_eq!(pid_of(&daemon, "live"), pid);
    // nothing to change is a mistake of the caller, not a silent success
    assert_code(
        &set_policy(&daemon, None, "live", None, None),
        "E_INVALID_SPEC",
    );
    // a name that is not there
    assert_code(
        &set_policy(&daemon, None, "ghost", Some(true), None),
        "E_APP_NOT_FOUND",
    );
}

#[test]
fn set_policy_is_human_only() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "mine", json!({ "protected": true })),
    );
    for socket in [daemon.socket(), daemon.agent_socket()] {
        for claim in [Some("ai:test"), Some("robot:x"), Some("adapter:ui"), None] {
            if claim.is_none() && socket == daemon.socket() {
                continue; // a person
            }
            let reply = daemon.rpc_on(
                &socket,
                "sp",
                claim,
                json!({ "type": "set_policy", "selector": by_name("mine"), "protected": false, "ai_control": true }),
            )["response"]
                .clone();
            assert_code(&reply, "E_POLICY_DENIED");
        }
    }
    assert_eq!(
        markers_of(&daemon, "mine"),
        (true, false),
        "an AI cannot unprotect or open an app"
    );
}

#[test]
fn markers_survive_daemon_restart() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "keeps", json!({})));
    start_as_human(&daemon, app(&work, "stopped-one", json!({})));
    set_policy(&daemon, None, "keeps", Some(true), Some(true));
    set_policy(&daemon, None, "stopped-one", None, Some(true));
    human(
        &daemon,
        json!({ "type": "stop", "selector": by_name("stopped-one") }),
    );

    // a clean exit, then a daemon on the same home
    daemon.signal(common::daemon::Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
    let daemon = DaemonProc::spawn(&home);
    assert_eq!(markers_of(&daemon, "keeps"), (true, true));
    assert_eq!(markers_of(&daemon, "stopped-one"), (false, true));
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("keeps") }),
        ),
        "E_PROTECTED",
    );
    // the saved file agrees
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("saved.json")).unwrap()).unwrap();
    let keeps = saved["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "keeps")
        .unwrap();
    assert_eq!(
        (keeps["protected"].as_bool(), keeps["ai_control"].as_bool()),
        (Some(true), Some(true))
    );

    // and after the database is the only copy: the daemon is killed hard, not stopped
    set_policy(&daemon, None, "stopped-one", Some(true), Some(false));
    daemon.sigkill();
    let mut daemon = daemon;
    daemon.wait_exit();
    let daemon = DaemonProc::spawn(&home);
    assert_eq!(markers_of(&daemon, "stopped-one"), (true, false));
}

fn cluster_spec(work: &Path, name: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut extra = extra;
    extra["instances"] = json!(2);
    app(work, name, extra)
}

#[test]
fn set_policy_on_cluster_protects_every_instance() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(&daemon, cluster_spec(&workdir(&tmp), "web", json!({})));
    for name in ["web", "web-0", "web-1"] {
        assert_eq!(markers_of(&daemon, name), (false, false), "{name}");
    }
    let reply = set_policy(&daemon, None, "web", Some(true), Some(true));
    assert_eq!(reply["count"], 1, "one cluster, one result: {reply}");
    for name in ["web", "web-0", "web-1"] {
        assert_eq!(markers_of(&daemon, name), (true, true), "{name}");
    }
}

#[test]
fn protected_cluster_blocks_ai_stop_of_an_instance() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        cluster_spec(&workdir(&tmp), "web", json!({ "ai_control": true })),
    );
    let before = state(&daemon);
    // open to the ai actor: an instance can be stopped on its own
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("web-0") })
        )["type"],
        "stopped"
    );

    // the cluster is protected, then one instance is unprotected on its own: the cluster's marker
    // still holds for it
    set_policy(&daemon, None, "web", Some(true), None);
    set_policy(&daemon, None, "web-1", Some(false), None);
    assert_eq!(markers_of(&daemon, "web-1"), (false, true));
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": by_name("web-1") }),
    );
    assert_code(&reply, "E_PROTECTED");
    assert!(
        reply["message"].as_str().unwrap().contains("web-1"),
        "{reply}"
    );
    assert_eq!(status_of(&daemon, "web-1"), "running");
    let _ = before;
}

#[test]
fn cluster_stop_by_ai_is_all_or_nothing() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        cluster_spec(&workdir(&tmp), "web", json!({ "ai_control": true })),
    );
    // one instance is closed on its own
    set_policy(&daemon, None, "web-1", None, Some(false));
    let before = state(&daemon);
    let reply = ai(
        &daemon,
        json!({ "type": "stop", "selector": by_name("web") }),
    );
    assert_code(&reply, "E_POLICY_DENIED");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("web-1") && !message.contains("web-0"),
        "{message}"
    );
    assert_eq!(state(&daemon), before, "no instance was stopped");
    // the open one can be handled alone, and a person can stop the whole cluster
    assert_eq!(
        ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("web-0") })
        )["type"],
        "stopped"
    );
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("web") })
        )["type"],
        "stopped"
    );
}

#[test]
fn restart_via_config_file_keeps_protected() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let daemon = DaemonProc::spawn(&home);
    let work = workdir(&tmp);
    start_as_human(
        &daemon,
        app(
            &work,
            "cfg",
            json!({ "protected": true, "ai_control": true }),
        ),
    );
    // `oxidepm start <file>` sends the spec the file describes, which says neither marker
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("cfg") })
        )["type"],
        "stopped"
    );
    let reply = human(
        &daemon,
        json!({ "type": "start", "spec": app(&work, "cfg", json!({})) }),
    );
    assert_eq!(reply["type"], "started", "{reply}");
    assert_eq!(
        markers_of(&daemon, "cfg"),
        (true, true),
        "the file cannot clear the markers"
    );
    // the stored copy of the spec says the same (not what the file said)
    let stored = |home: &Path| -> (bool, bool) {
        let rows = db_rows(home);
        let spec: serde_json::Value =
            serde_json::from_str(&rows.iter().find(|r| r.0 == "cfg").unwrap().2).unwrap();
        (
            spec["protected"].as_bool().unwrap(),
            spec["ai_control"].as_bool().unwrap(),
        )
    };
    assert_eq!(
        stored(&home),
        (true, true),
        "spec_json keeps the markers too"
    );
    // nor can a batch, nor a spec that says the opposite
    assert_eq!(
        human(
            &daemon,
            json!({ "type": "stop", "selector": by_name("cfg") })
        )["type"],
        "stopped"
    );
    let reply = human(
        &daemon,
        json!({ "type": "start_batch", "specs": [app(&work, "cfg", json!({ "protected": false, "ai_control": false }))] }),
    );
    assert_eq!(reply["type"], "started_batch", "{reply}");
    assert_eq!(markers_of(&daemon, "cfg"), (true, true));
    // a restart keeps them too, and the AI is still held off
    human(
        &daemon,
        json!({ "type": "restart", "selector": by_name("cfg") }),
    );
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("cfg") }),
        ),
        "E_PROTECTED",
    );
}

#[test]
fn saved_json_keeps_markers_set_by_set_policy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let daemon = DaemonProc::spawn(&home);
    let work = workdir(&tmp);
    // created plain, protected later by a person
    start_as_human(&daemon, app(&work, "later", json!({})));
    set_policy(&daemon, None, "later", Some(true), Some(true));
    assert_eq!(
        human(&daemon, json!({ "type": "save", "force": false }))["type"],
        "saved"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("saved.json")).unwrap()).unwrap();
    let entry = saved["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "later")
        .unwrap();
    assert_eq!(
        (entry["protected"].as_bool(), entry["ai_control"].as_bool()),
        (Some(true), Some(true)),
        "{entry}"
    );

    // delete all, then resurrect: the app comes back and is still protected
    human(
        &daemon,
        json!({ "type": "delete", "selector": { "by": "all" } }),
    );
    assert!(state(&daemon).is_empty());
    let reply = human(&daemon, json!({ "type": "resurrect" }));
    assert_eq!(reply["type"], "resurrected", "{reply}");
    assert_eq!(reply["imported"], 1, "{reply}");
    assert_eq!(markers_of(&daemon, "later"), (true, true));
    assert_code(
        &ai(
            &daemon,
            json!({ "type": "stop", "selector": by_name("later") }),
        ),
        "E_PROTECTED",
    );
}

#[test]
fn resurrect_after_delete_all_keeps_the_markers() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "solo", json!({ "protected": true })));
    start_as_human(
        &daemon,
        cluster_spec(&work, "pair", json!({ "ai_control": true })),
    );
    set_policy(&daemon, None, "pair", Some(true), None);
    human(&daemon, json!({ "type": "save", "force": false }));
    human(
        &daemon,
        json!({ "type": "delete", "selector": { "by": "all" } }),
    );
    let reply = human(&daemon, json!({ "type": "resurrect" }));
    assert_eq!(reply["imported"], 2, "{reply}");
    assert_eq!(markers_of(&daemon, "solo"), (true, false));
    for name in ["pair", "pair-0", "pair-1"] {
        assert_eq!(markers_of(&daemon, name), (true, true), "{name}");
    }
}

#[test]
fn status_shows_markers() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let work = workdir(&tmp);
    start_as_human(&daemon, app(&work, "plain", json!({})));
    start_as_human(&daemon, app(&work, "marked", json!({ "protected": true })));
    set_policy(&daemon, None, "plain", None, Some(true));
    let status = ai(&daemon, json!({ "type": "status" }));
    let find = |name: &str| {
        status["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["spec"]["name"] == name)
            .unwrap()["spec"]
            .clone()
    };
    assert_eq!(
        (
            find("plain")["protected"].as_bool(),
            find("plain")["ai_control"].as_bool()
        ),
        (Some(false), Some(true))
    );
    assert_eq!(
        (
            find("marked")["protected"].as_bool(),
            find("marked")["ai_control"].as_bool()
        ),
        (Some(true), Some(false))
    );
}

#[test]
fn set_policy_by_ai_is_denied_and_audited_later() {
    let tmp = TempDir::new().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    start_as_human(
        &daemon,
        app(&workdir(&tmp), "target", json!({ "protected": true })),
    );
    let reply = ai(
        &daemon,
        json!({ "type": "set_policy", "selector": by_name("target"), "protected": false, "ai_control": null }),
    );
    assert_code(&reply, "E_POLICY_DENIED");
    let line = daemon.wait_stderr_line(&["denied", "ai:test", "set_policy"]);
    assert!(line.contains("dangerous"), "{line}");
    assert_eq!(markers_of(&daemon, "target"), (true, false));

    // the denial is in the security file (it was written when the request was refused)
    let path = daemon.home().join("audit/audit-security.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let denial = loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let found = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|l| {
                l["phase"] == "decision" && l["op"] == "set_policy" && l["decision"] == "deny"
            });
        if let Some(found) = found {
            break found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no denial line in {text}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(denial["actor"]["class"], "ai");
    assert_eq!(denial["deny"]["code"], "E_POLICY_DENIED");
    assert_eq!(denial["selector"], "target");
    assert_eq!(denial["tier"], "dangerous");
}
