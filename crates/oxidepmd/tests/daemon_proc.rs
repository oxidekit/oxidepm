//! Integration tests for the daemon binary, including signals, single-instance enforcement and
//! shutdown.

mod common;

use common::daemon::{daemon_bin, DaemonProc, Signal};
use common::{saved_entries, saved_json};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn daemon_harness_answers_ping_and_exits_on_sigterm() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);

    let reply = daemon.rpc(json!({ "type": "ping" }));
    assert_eq!(reply["type"], "pong");
    assert!(
        home.join("state.db").exists(),
        "the daemon worked in the private home"
    );

    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "daemon exited with {status}");
}

/// Run `oxidepmd --check` in a private home. Bounded: the child is killed after 10 s so a daemon
/// that ignores the flag fails the test instead of hanging it.
fn run_check(home: &std::path::Path) -> (Option<std::process::ExitStatus>, String) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    std::fs::create_dir_all(home).expect("create home");
    let stderr_path = home.join("check.stderr");
    let mut child = Command::new(daemon_bin())
        .arg("--check")
        .env("OXIDEPM_HOME", home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).expect("stderr file"))
        .spawn()
        .expect("spawn oxidepmd --check");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (
        status,
        std::fs::read_to_string(&stderr_path).unwrap_or_default(),
    )
}

#[test]
fn oxidepmd_check_exits_nonzero_on_incompatible_state() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let legacy = home.join("oxidepm.db");
    std::fs::write(&legacy, b"old schema").unwrap();

    let (status, stderr) = run_check(&home);
    let status = status.expect("--check must exit by itself");
    assert!(!status.success(), "exit status {status}");
    let legacy = legacy.display().to_string();
    assert!(stderr.contains(&legacy), "names the file: {stderr}");
    assert!(
        stderr.contains(&format!("mv {legacy} {legacy}.old")),
        "gives the remedy: {stderr}"
    );
    assert!(!home.join("daemon.sock").exists(), "binds nothing");
    assert!(!home.join("oxidepmd.lock").exists(), "takes no lock");
    assert!(
        !home.join("state.db").exists(),
        "a refusal creates no state.db"
    );
}

#[test]
fn oxidepmd_check_on_a_usable_home_exits_zero_without_starting_a_daemon() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let (status, stderr) = run_check(&home);
    let status = status.expect("--check must exit by itself");
    assert!(status.success(), "exit status {status}; stderr: {stderr}");
    assert!(home.join("state.db").exists(), "the database was opened");
    assert!(!home.join("daemon.sock").exists(), "binds nothing");
}

#[test]
fn oxidepmd_check_refuses_a_group_writable_home() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o770)).unwrap();

    let (status, stderr) = run_check(&home);
    assert_eq!(status.and_then(|status| status.code()), Some(1), "{stderr}");
    assert!(stderr.contains(&home.display().to_string()), "{stderr}");
    assert!(stderr.contains("chmod 700"), "{stderr}");
    assert!(!home.join("state.db").exists());
}

#[test]
fn oxidepmd_refuses_a_group_writable_home_before_opening_the_log() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o770)).unwrap();
    std::fs::write(home.join("oxidepm.db"), b"old schema").unwrap();

    let mut daemon = DaemonProc::spawn_unready(&home);
    assert_eq!(daemon.wait_exit().code(), Some(1));
    let stderr = daemon.stderr();
    assert!(stderr.contains(&home.display().to_string()), "{stderr}");
    assert!(stderr.contains("chmod 700"), "{stderr}");
    assert!(!home.join("daemon.log").exists());
}

#[tokio::test]
async fn autostart_refuses_a_group_writable_home_before_opening_the_log() {
    use oxidepm_ipc::{IpcClient, Request};
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o770)).unwrap();
    std::fs::write(home.join("oxidepm.db"), b"old schema").unwrap();
    let client = IpcClient::new(home.join("daemon.sock")).with_daemon_bin(daemon_bin());

    let error = client
        .send_autostart(&Request::Status)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(&home.display().to_string()), "{error}");
    assert!(error.contains("chmod 700"), "{error}");
    assert!(!home.join("daemon.log").exists());
}

#[test]
fn oxidepmd_check_exits_nonzero_on_unreadable_saved_file() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("saved.json"), b"{ not json").unwrap();

    let (status, stderr) = run_check(&home);
    let status = status.expect("--check must exit by itself");
    assert!(!status.success(), "exit status {status}");
    assert!(stderr.contains("saved.json"), "{stderr}");
}

/// The legacy bare-array saved file is refused by `--check` with the remedy; a version 1 file
/// with one bad entry is fine (the entry is reported when the apps are resurrected).
#[test]
fn oxidepmd_check_reads_the_versioned_saved_file() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("saved.json"), b"[]").unwrap();
    let (status, stderr) = run_check(&home);
    assert!(!status.expect("--check must exit by itself").success());
    assert!(stderr.contains("pre-1.0 format"), "{stderr}");

    std::fs::write(
        home.join("saved.json"),
        br#"{"version":1,"apps":[{"name":"x","bogus":1}]}"#,
    )
    .unwrap();
    let (status, stderr) = run_check(&home);
    assert!(
        status.expect("--check must exit by itself").success(),
        "{stderr}"
    );
}

/// A daemon that refuses to start must not look like a timeout: the client reports that the
/// daemon exited and shows what it wrote to `daemon.log`, which names the file and the remedy.
#[tokio::test]
async fn autostart_with_legacy_db_prints_clear_error() {
    use oxidepm_ipc::{IpcClient, Request};
    use std::time::{Duration, Instant};

    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let legacy = home.join("oxidepm.db");
    std::fs::write(&legacy, b"old schema").unwrap();

    let client = IpcClient::new(home.join("daemon.sock")).with_daemon_bin(daemon_bin());
    let started = Instant::now();
    let err = client
        .send_autostart(&Request::Status)
        .await
        .expect_err("a daemon with an incompatible home cannot start");

    let text = err.to_string();
    let legacy = legacy.display().to_string();
    assert!(text.contains(&legacy), "names the file: {text}");
    assert!(
        text.contains(&format!("mv {legacy} {legacy}.old")),
        "gives the remedy: {text}"
    );
    assert!(
        !text.contains("within timeout"),
        "not the generic timeout: {text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "fails fast, took {:?}",
        started.elapsed()
    );
    let log = std::fs::read_to_string(home.join("daemon.log")).expect("daemon.log exists");
    assert!(
        log.contains(&legacy),
        "the daemon log has the message: {log}"
    );
    assert_eq!(
        log.matches("mv ").count(),
        1,
        "the message is not written twice: {log}"
    );
    assert!(!home.join("daemon.sock").exists());
}

/// One envelope-format `ping` request line (with the newline).
fn ping_line(request_id: &str) -> String {
    format!(
        "{{\"protocol\":2,\"request_id\":\"{request_id}\",\"request\":{{\"type\":\"ping\"}}}}\n"
    )
}

/// Two requests in one write must both be answered: the framing keeps the bytes behind the
/// first newline for the next read.
#[test]
fn pipelined_requests_are_both_answered() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let ping = ping_line("one");
    let (replies, _) = daemon.raw_exchange(format!("{ping}{}", ping_line("two")).into_bytes(), 2);
    assert_eq!(replies.len(), 2, "two answers: {replies:?}");
    assert!(
        replies.iter().all(|r| r["response"]["type"] == "pong"),
        "{replies:?}"
    );
    assert_eq!(replies[0]["request_id"], "one");
    assert_eq!(replies[1]["request_id"], "two");
}

/// A request line over the limit is refused with an error and the connection is closed; the
/// daemon keeps serving others.
#[test]
fn oversized_request_line_is_rejected() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let (replies, closed) = daemon.raw_exchange(vec![b'x'; 11 * 1024 * 1024], 1);
    assert_eq!(replies.len(), 1, "an error answer: {replies:?}");
    let response = &replies[0]["response"];
    assert_eq!(response["type"], "error", "{replies:?}");
    assert_eq!(response["code"], "E_REQUEST_TOO_LARGE", "{replies:?}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("too large"),
        "{replies:?}"
    );
    assert!(closed, "the connection is closed after the error");

    assert_eq!(
        daemon.rpc(json!({ "type": "ping" }))["type"],
        "pong",
        "daemon still serves"
    );
}

/// A line that is not JSON gets an error answer and the same connection still works.
#[test]
fn malformed_request_gets_an_error_and_the_connection_stays_usable() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let (replies, _) =
        daemon.raw_exchange(format!("garbage\n{}", ping_line("after")).into_bytes(), 2);
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert_eq!(replies[0]["response"]["type"], "error", "{replies:?}");
    assert_eq!(
        replies[0]["response"]["code"], "E_BAD_REQUEST",
        "{replies:?}"
    );
    assert_eq!(replies[1]["response"]["type"], "pong", "{replies:?}");
}

/// A daemon started by the client's autostart is detached: its own session (and process
/// group), so a SIGINT or SIGHUP aimed at the CLI's group never reaches it.
#[tokio::test]
async fn autostarted_daemon_is_its_own_session_leader() {
    use oxidepm_ipc::{IpcClient, Request, Response};

    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let client = IpcClient::new(home.join("daemon.sock")).with_daemon_bin(daemon_bin());

    let response = client
        .send_autostart(&Request::Ping)
        .await
        .expect("the daemon starts and answers");
    assert!(matches!(response, Response::Pong), "{response:?}");

    // the daemon's pid, from the socket's peer credentials (only the daemon we just started
    // can be on this private socket)
    let stream = tokio::net::UnixStream::connect(home.join("daemon.sock"))
        .await
        .unwrap();
    let pid = stream.peer_cred().unwrap().pid().expect("peer pid") as u32;
    drop(stream);

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("daemon stat");
    // fields after the ")" that closes the command name: state ppid pgrp session ...
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let (pgrp, session): (u32, u32) = (fields[2].parse().unwrap(), fields[3].parse().unwrap());
    let ours = std::fs::read_to_string("/proc/self/stat").unwrap();
    let our_session: u32 = ours[ours.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .nth(3)
        .unwrap()
        .parse()
        .unwrap();

    // stop the daemon before asserting, so a failure does not leave it running
    let _ = client.send(&Request::Kill).await;
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert_eq!(session, pid, "the daemon leads its own session");
    assert_eq!(pgrp, pid, "and its own process group");
    assert_ne!(session, our_session);
}

/// `status`, `show` and `describe` never carry an env value (or a hook script) to the client;
/// the keys stay visible.
#[test]
fn status_and_show_redact_all_env_values() {
    use oxidepm_core::{AppSpec, Runtime};

    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut spec = AppSpec::new(
        "redact-app".to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        workdir,
    );
    spec.args = vec!["301.5".to_string()];
    for (key, value) in [
        ("SECRET_X", "value-of-secret-x"),
        ("MNEMONIC", "value-of-mnemonic"),
        ("DB_DSN", "value-of-db-dsn"),
        ("HOME", "value-of-home"),
    ] {
        spec.env.insert(key.to_string(), value.to_string());
    }
    spec.hooks.on_start = Some("echo value-of-hook".to_string());

    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    let status = daemon.rpc(json!({ "type": "status" }));
    let show =
        daemon.rpc(json!({ "type": "show", "selector": { "by": "name", "value": "redact-app" } }));
    let describe = daemon
        .rpc(json!({ "type": "describe", "selector": { "by": "name", "value": "redact-app" } }));
    // stop the app (and the daemon) before asserting, so a failure leaves nothing running
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert_eq!(started["type"], "started", "{started}");
    for (what, reply) in [
        ("status", &status),
        ("show", &show),
        ("describe", &describe),
    ] {
        let raw = reply.to_string();
        assert_ne!(reply["type"], "error", "{what}: {raw}");
        for value in [
            "value-of-secret-x",
            "value-of-mnemonic",
            "value-of-db-dsn",
            "value-of-home",
            "value-of-hook",
        ] {
            assert!(!raw.contains(value), "{what} leaked {value}: {raw}");
        }
        for key in ["SECRET_X", "MNEMONIC", "DB_DSN", "HOME"] {
            assert!(raw.contains(key), "{what} lost the key {key}: {raw}");
        }
    }
}

/// Wrong or missing protocol: an envelope outside the supported range gets `E_PROTOCOL_VERSION`
/// naming that range (in an envelope); a bare request from an old client gets a bare error
/// and the connection is closed.
#[test]
fn request_with_unsupported_protocol_gets_protocol_error() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let reply = daemon.rpc_envelope(json!({
        "protocol": 99, "request_id": "r99", "request": { "type": "ping" }
    }));
    assert_eq!(reply["request_id"], "r99", "{reply}");
    assert_eq!(reply["response"]["type"], "error", "{reply}");
    assert_eq!(reply["response"]["code"], "E_PROTOCOL_VERSION", "{reply}");
    let message = reply["response"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("99") && message.contains("2..2"),
        "names the range: {message}"
    );

    let (replies, closed) = daemon.raw_exchange(b"{\"type\":\"ping\"}\n".to_vec(), 1);
    assert_eq!(replies.len(), 1, "{replies:?}");
    let bare = &replies[0];
    assert!(
        bare.get("protocol").is_none() && bare.get("response").is_none(),
        "bare shape: {bare}"
    );
    assert_eq!(bare["type"], "error", "{bare}");
    assert_eq!(bare["code"], "E_PROTOCOL_VERSION", "{bare}");
    assert!(
        bare["message"]
            .as_str()
            .unwrap_or_default()
            .contains("oxidepm kill"),
        "{bare}"
    );
    assert!(closed, "the connection of a bare client is closed");

    assert_eq!(
        daemon.rpc(json!({ "type": "ping" }))["type"],
        "pong",
        "daemon still serves"
    );
}

#[test]
fn hello_reports_version_pid_phase_features() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    // The socket is bound before the boot resurrect runs (in the background), so a first hello
    // may legitimately report `resurrecting`; wait for `ready`, then assert everything.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let hello = loop {
        let hello = daemon.rpc(json!({ "type": "hello", "client": "test/0" }));
        if hello["phase"]["state"] == "ready" || std::time::Instant::now() >= deadline {
            break hello;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["protocol_min"], 2);
    assert_eq!(hello["protocol_max"], 2);
    assert_eq!(hello["pid"], daemon.pid());
    assert_eq!(hello["phase"]["state"], "ready", "{hello}");
    assert_eq!(hello["daemon_version"], env!("CARGO_PKG_VERSION"));
    assert!(
        hello["started_at"]
            .as_str()
            .is_some_and(|t| t.contains('T')),
        "{hello}"
    );
    let features: Vec<&str> = hello["features"]
        .as_array()
        .expect("features list")
        .iter()
        .filter_map(|f| f.as_str())
        .collect();
    for feature in [
        "envelope",
        "item-results",
        "tagged-selector",
        "start-batch",
        "actor",
        "audit",
        "dry-run",
    ] {
        assert!(
            features.contains(&feature),
            "{feature} missing from {features:?}"
        );
    }
}

#[test]
fn errors_carry_structured_code() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));

    let missing = daemon.rpc(json!({
        "type": "show", "selector": { "by": "name", "value": "nobody" }
    }));
    assert_eq!(missing["type"], "error", "{missing}");
    assert_eq!(missing["code"], "E_APP_NOT_FOUND", "{missing}");
    assert!(
        missing["message"]
            .as_str()
            .unwrap_or_default()
            .contains("nobody"),
        "{missing}"
    );

    let stop = daemon.rpc(json!({ "type": "stop", "selector": { "by": "tag", "value": "none" } }));
    assert_eq!(
        stop["code"], "E_APP_NOT_FOUND",
        "a selector that matches nothing is an error: {stop}"
    );

    let all = daemon.rpc(json!({ "type": "show", "selector": { "by": "all" } }));
    assert_eq!(all["code"], "E_INVALID_SELECTOR", "{all}");

    let follow = daemon.rpc(json!({
        "type": "logs", "selector": { "by": "all" }, "lines": 5,
        "follow": true, "stdout": true, "stderr": true
    }));
    assert_eq!(follow["code"], "E_UNSUPPORTED", "{follow}");

    let empty = daemon.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
    assert_eq!(
        empty["type"], "stopped",
        "'all' on an empty registry is not an error: {empty}"
    );
    assert_eq!(empty["count"], 0);
}

/// A sleeping app with a unique duration, mode `cmd`, working in `workdir`.
fn sleeper(name: &str, seconds: &str, workdir: &std::path::Path) -> oxidepm_core::AppSpec {
    use oxidepm_core::{AppSpec, Runtime};
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec![seconds.to_string()];
    spec
}

#[test]
fn show_reveal_returns_env_values() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut spec = sleeper("reveal-app", "302.5", &workdir);
    spec.env
        .insert("SECRET_X".to_string(), "value-of-secret-x".to_string());
    spec.hooks.on_start = Some("echo value-of-hook".to_string());
    let selector = json!({ "by": "name", "value": "reveal-app" });

    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    let hidden = daemon.rpc(json!({ "type": "show", "selector": selector }));
    let revealed = daemon.rpc_with(json!({ "type": "show", "selector": selector }), true);
    let described = daemon.rpc_with(json!({ "type": "describe", "selector": selector }), true);
    let status = daemon.rpc_with(json!({ "type": "status" }), true);
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert_eq!(started["type"], "started", "{started}");
    assert!(
        !hidden.to_string().contains("value-of-secret-x"),
        "{hidden}"
    );
    assert_eq!(
        revealed["app"]["spec"]["env"]["SECRET_X"], "value-of-secret-x",
        "{revealed}"
    );
    assert_eq!(
        revealed["app"]["spec"]["hooks"]["on_start"], "echo value-of-hook",
        "{revealed}"
    );
    assert_eq!(
        described["env"]["SECRET_X"], "value-of-secret-x",
        "{described}"
    );
    assert!(
        !status.to_string().contains("value-of-secret-x"),
        "status never reveals, whatever the envelope says: {status}"
    );
}

/// `@tag` selects by tag over the wire: the app tagged `frontend` is stopped, the app that is
/// merely *named* `frontend` and the untagged one keep running.
#[test]
fn selector_by_tag_stops_only_tagged_apps() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut tagged = sleeper("a", "303.1", &workdir);
    tagged.tags = vec!["frontend".to_string()];
    let named = sleeper("frontend", "303.2", &workdir);
    let other = sleeper("b", "303.3", &workdir);
    let started: Vec<_> = [tagged, named, other]
        .into_iter()
        .map(|spec| daemon.rpc(json!({ "type": "start", "spec": spec })))
        .collect();

    let stop =
        daemon.rpc(json!({ "type": "stop", "selector": { "by": "tag", "value": "frontend" } }));
    let status = daemon.rpc(json!({ "type": "status" }));
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert!(
        started.iter().all(|s| s["type"] == "started"),
        "{started:?}"
    );
    assert_eq!(stop["type"], "stopped", "{stop}");
    assert_eq!(stop["count"], 1, "{stop}");
    let items = stop["items"].as_array().expect("item results");
    assert_eq!(items.len(), 1, "{stop}");
    assert_eq!(items[0]["name"], "a", "{stop}");
    assert_eq!(items[0]["result"]["outcome"], "ok", "{stop}");
    assert_eq!(items[0]["result"]["changed"], true, "{stop}");

    let state_of = |name: &str| {
        status["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|app| app["spec"]["name"] == name)
            .map(|app| app["state"]["status"].clone())
            .unwrap_or_else(|| panic!("{name} missing from {status}"))
    };
    assert_eq!(state_of("a"), "stopped");
    assert_eq!(state_of("frontend"), "running");
    assert_eq!(state_of("b"), "running");
}

/// Answer time of one request, in milliseconds.
fn timed_rpc(daemon: &DaemonProc, request: serde_json::Value) -> (serde_json::Value, u128) {
    let begun = std::time::Instant::now();
    let reply = daemon.rpc(request);
    (reply, begun.elapsed().as_millis())
}

/// A `sh -c` app (mode `cmd`) in `workdir`.
fn shell_app(name: &str, script: &str, workdir: &std::path::Path) -> oxidepm_core::AppSpec {
    use oxidepm_core::{AppSpec, Runtime};
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sh".to_string(),
        workdir.to_path_buf(),
    );
    spec.args = vec!["-c".to_string(), script.to_string()];
    spec
}

/// A start that is still waiting out its `startup_delay_ms` holds nothing that a `ping` needs.
#[test]
fn ping_answers_while_slow_start_is_in_flight() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut spec = sleeper("slow-start", "304.1", &workdir);
    spec.startup_delay_ms = Some(3000);
    let start_done = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let start = scope.spawn(|| {
            let reply = daemon.rpc(json!({ "type": "start", "spec": spec }));
            start_done.store(true, Ordering::Release);
            reply
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let status = daemon.rpc(json!({ "type": "status" }));
            let waiting = status["apps"].as_array().is_some_and(|apps| {
                apps.iter().any(|app| {
                    app["spec"]["name"] == "slow-start" && app["state"]["status"] == "waiting"
                })
            });
            if waiting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "slow-start never entered waiting state: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (reply, ms) = timed_rpc(&daemon, json!({ "type": "ping" }));
        assert_eq!(reply["type"], "pong", "{reply}");
        assert!(
            !start_done.load(Ordering::Acquire),
            "start already finished before ping answered ({ms} ms)"
        );
        assert!(ms < 1500, "ping took {ms} ms while a start was in flight");
        let started = start.join().expect("start thread");
        assert_eq!(started["type"], "started", "{started}");
    });
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// The old global handler lock made every request, `status` included, wait behind a `stop`
/// that was waiting for its kill timeout.
#[test]
fn status_answers_while_stop_waits_for_kill_timeout() {
    let tmp = TempDir::new().expect("temp dir");
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let ready = workdir.join("trap.ready");
    let script = format!(
        "trap '' TERM; touch '{}'; while :; do sleep 0.1; done # 305.1",
        ready.display()
    );
    let mut spec = shell_app("stubborn", &script, &workdir);
    spec.kill_timeout_ms = 3000;
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the app never installed its trap"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    std::thread::scope(|scope| {
        let stop = scope.spawn(|| {
            timed_rpc(
                &daemon,
                json!({ "type": "stop", "selector": { "by": "name", "value": "stubborn" } }),
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (status, ms) = timed_rpc(&daemon, json!({ "type": "status" }));
        assert_eq!(status["type"], "status", "{status}");
        assert!(
            ms < 500,
            "status took {ms} ms while a stop waited for the kill timeout"
        );
        let (stopped, stop_ms) = stop.join().expect("stop thread");
        assert_eq!(stopped["type"], "stopped", "{stopped}");
        assert!(
            stop_ms >= 2500,
            "the stop really did wait for the kill timeout ({stop_ms} ms)"
        );
    });
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// Write a `saved.json` with one sleeping app that waits `delay_ms` before it starts, so the
/// daemon's boot resurrect stays busy for that long.
fn save_slow_app(home: &std::path::Path, workdir: &std::path::Path, delay_ms: u64, marker: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::create_dir_all(workdir).unwrap();
    let mut spec = sleeper("slow-boot", marker, workdir);
    spec.startup_delay_ms = Some(delay_ms);
    std::fs::write(home.join("saved.json"), saved_json(&[spec])).unwrap();
}

/// Poll until one of the two daemons has exited; returns which and with what.
fn wait_for_one_exit(a: &mut DaemonProc, b: &mut DaemonProc) -> (usize, std::process::ExitStatus) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = a.try_exit() {
            return (0, status);
        }
        if let Some(status) = b.try_exit() {
            return (1, status);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "neither daemon exited"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Two daemons started together on one home: the flock lets exactly one run; the other says so
/// and exits with code 3. Twenty rounds, because the failure is a race.
#[test]
fn second_daemon_exits_with_already_running() {
    for round in 0..20 {
        let tmp = TempDir::new().expect("temp dir");
        let home = tmp.path().join("home");
        let mut a = DaemonProc::spawn_unready_with_stderr(&home, "a.stderr");
        let mut b = DaemonProc::spawn_unready_with_stderr(&home, "b.stderr");

        let (loser, status) = wait_for_one_exit(&mut a, &mut b);
        assert_eq!(
            status.code(),
            Some(3),
            "round {round}: the second daemon exits with 3: {status}"
        );
        let (winner, loser_proc) = if loser == 0 {
            (&mut b, &a)
        } else {
            (&mut a, &b)
        };
        assert!(
            loser_proc.stderr().contains("already running"),
            "round {round}: it says why: {:?}",
            loser_proc.stderr()
        );
        winner.wait_ready();
        let pong = winner.rpc(json!({ "type": "ping" }));
        assert_eq!(
            pong["type"], "pong",
            "round {round}: the first daemon stays up: {pong}"
        );
        assert!(winner.try_exit().is_none());
        let _ = winner.rpc(json!({ "type": "kill" }));
    }
}

/// The lock is taken before anything slow: while the first daemon is still working through its
/// boot resurrect, a second one is refused at once and the first is not disturbed.
#[test]
fn second_daemon_during_slow_boot_exits_with_already_running() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    save_slow_app(&home, &tmp.path().join("work"), 6000, "306.2");
    let mut first = DaemonProc::spawn_unready_with_stderr(&home, "first.stderr");
    // the first daemon is up and busy with its slow saved app
    first.wait_ready();
    let hello = first.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(
        hello["phase"]["state"], "resurrecting",
        "the first daemon is still booting: {hello}"
    );

    let mut second = DaemonProc::spawn_unready_with_stderr(&home, "second.stderr");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let status = loop {
        if let Some(status) = second.try_exit() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the second daemon was still running 4 s later; stderr: {}",
            second.stderr()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(3),
        "{status}; stderr: {}",
        second.stderr()
    );
    assert!(first.try_exit().is_none(), "the first daemon stays up");
    let hello = first.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(
        hello["phase"]["state"], "resurrecting",
        "and is undisturbed: {hello}"
    );
    let _ = first.rpc(json!({ "type": "kill" }));
}

/// The lock is what stops a second daemon, not the socket check: with the lock held by someone
/// else before any socket exists, the daemon exits 3 without binding one.
#[test]
fn daemon_exits_with_already_running_when_the_lock_is_held_and_no_socket_exists() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let paths = oxidepm_core::Paths::new(home.clone());
    std::fs::create_dir_all(&home).unwrap();
    let held =
        oxidepmd::lock::DaemonLock::acquire(&paths.lock()).expect("the test takes the lock first");
    assert!(!paths.socket().exists());

    let mut daemon = DaemonProc::spawn_unready_with_stderr(&home, "daemon.stderr");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = daemon.try_exit() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit; stderr: {}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(3),
        "{status}; stderr: {}",
        daemon.stderr()
    );
    assert!(
        daemon.stderr().contains("already running"),
        "{:?}",
        daemon.stderr()
    );
    assert!(!paths.socket().exists(), "it never bound a socket");
    drop(held);
}

/// The socket is bound before the saved apps are started: it answers while a resurrecting app
/// is still in its startup delay, and `hello` says the daemon is resurrecting until that is done.
#[test]
fn socket_answers_before_resurrect_completes() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    save_slow_app(&home, &tmp.path().join("work"), 4000, "306.3");

    let daemon = DaemonProc::spawn(&home);
    let reply = daemon.rpc(json!({ "type": "ping" }));
    assert_eq!(reply["type"], "pong", "{reply}");

    // the saved app is still waiting out its startup delay: resurrecting, not running yet
    let hello = daemon.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(hello["phase"]["state"], "resurrecting", "{hello}");
    assert_eq!(hello["phase"]["total"], 1, "{hello}");
    let status = daemon.rpc(json!({ "type": "status" }));
    assert!(
        status["apps"]
            .as_array()
            .is_some_and(|apps| apps.iter().all(|a| a["state"]["status"] != "running")),
        "nothing is running before the startup delay is over: {status}"
    );

    // then it becomes ready, and the saved app is running
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let hello = daemon.rpc(json!({ "type": "hello", "client": "test" }));
        if hello["phase"]["state"] == "ready" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never became ready: {hello}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = daemon.rpc(json!({ "type": "status" }));
    let apps = status["apps"].as_array().expect("apps");
    assert_eq!(apps.len(), 1, "{status}");
    assert_eq!(apps[0]["state"]["status"], "running", "{status}");
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// A `resurrect` request that arrives while the boot resurrect is still running joins it: it
/// answers after it, and nothing is started twice.
#[test]
fn resurrect_request_during_boot_resurrect_joins_it() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    save_slow_app(&home, &tmp.path().join("work"), 2500, "306.5");
    let daemon = DaemonProc::spawn(&home);
    let hello = daemon.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(hello["phase"]["state"], "resurrecting", "{hello}");

    let (reply, ms) = timed_rpc(&daemon, json!({ "type": "resurrect" }));
    assert_eq!(reply["type"], "resurrected", "{reply}");
    assert_eq!(
        reply["started"], 0,
        "the boot resurrect started it, not this request: {reply}"
    );
    assert!(ms >= 1000, "it waited for the boot resurrect ({ms} ms)");
    assert_eq!(
        Harness::procs_with_cmdline("sleep 306.5").len(),
        1,
        "one process, not two"
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// The registry is the database: a daemon that boots with a saved file it cannot read still
/// registers and resurrects every app of the database, answers `status` with them, leaves the
/// unreadable file as it found it, and an explicit `resurrect` reports the file's error.
#[test]
fn daemon_boots_from_the_database_when_the_saved_file_is_corrupt() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut first = DaemonProc::spawn(&home);
    let started =
        first.rpc(json!({ "type": "start", "spec": sleeper("db-boot", "313.1", &workdir) }));
    assert_eq!(started["type"], "started", "{started}");
    let stopped = first
        .rpc(json!({ "type": "start", "spec": sleeper("db-boot-stopped", "313.2", &workdir) }));
    assert_eq!(stopped["type"], "started", "{stopped}");
    let stop = first
        .rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "db-boot-stopped" } }));
    assert_eq!(stop["type"], "stopped", "{stop}");
    let _ = first.rpc(json!({ "type": "kill" }));
    first.wait_exit();
    assert!(Harness::procs_with_cmdline("sleep 313.").is_empty());

    std::fs::write(home.join("saved.json"), "{ not json").unwrap();
    let mut second = DaemonProc::spawn(&home);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let hello = second.rpc(json!({ "type": "hello", "client": "test" }));
        if hello["phase"]["state"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "never became ready: {hello}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = second.rpc(json!({ "type": "status" }));
    let state_of = |name: &str| -> String {
        status["apps"]
            .as_array()
            .and_then(|apps| apps.iter().find(|a| a["spec"]["name"] == name))
            .map(|a| a["state"]["status"].as_str().unwrap_or("?").to_string())
            .unwrap_or_else(|| panic!("{name} is not listed: {status}"))
    };
    assert_eq!(state_of("db-boot"), "running", "{status}");
    assert_eq!(
        state_of("db-boot-stopped"),
        "stopped",
        "stopped by the operator: {status}"
    );
    assert_eq!(Harness::procs_with_cmdline("sleep 313.1").len(), 1);
    assert!(Harness::procs_with_cmdline("sleep 313.2").is_empty());

    let reply = second.rpc(json!({ "type": "resurrect" }));
    assert_eq!(
        reply["type"], "error",
        "an explicit resurrect reports the unreadable file: {reply}"
    );

    let _ = second.rpc(json!({ "type": "kill" }));
    second.wait_exit();
    assert_eq!(
        std::fs::read_to_string(home.join("saved.json")).unwrap(),
        "{ not json",
        "the unreadable file was left as it was"
    );
    assert!(Harness::procs_with_cmdline("sleep 313.").is_empty());
}

/// The daemon records its live children in `pids.json` (one entry per child, with its group and
/// start time) and takes the record out when the child is stopped.
#[test]
fn pids_file_tracks_live_children() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let daemon = DaemonProc::spawn(&home);
    let started =
        daemon.rpc(json!({ "type": "start", "spec": sleeper("pid-tracked", "314.2", &workdir) }));
    assert_eq!(started["type"], "started", "{started}");
    let pid = pid_of(&daemon, "pid-tracked");

    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(home.join("pids.json")).expect("pids.json"))
            .unwrap()
    };
    let file = read();
    let apps = file["apps"].as_array().expect("apps");
    assert_eq!(apps.len(), 1, "{file}");
    assert_eq!(apps[0]["name"], "pid-tracked");
    assert_eq!(apps[0]["pid"], pid);
    assert_eq!(apps[0]["pgid"], pid, "the child leads its own group");
    assert!(apps[0]["start_ticks"].as_u64().unwrap() > 0, "{file}");
    assert!(
        file["boot_id"].as_str().is_some_and(|b| !b.is_empty()),
        "{file}"
    );

    let stop =
        daemon.rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "pid-tracked" } }));
    assert_eq!(stop["type"], "stopped", "{stop}");
    assert!(
        read()["apps"].as_array().unwrap().is_empty(),
        "stopped: {}",
        read()
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// A daemon that dies by SIGKILL cannot stop its children. The next daemon on the home kills
/// what the dead one left, then resurrects: exactly one process with the marker afterwards, not
/// the old one and a new one (and not merely "the old pid is gone").
#[test]
fn orphans_are_swept_after_sigkilled_daemon() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    // a shell with a background grandchild: the sweep must take the whole group
    let spec = shell_app("sweep-me", "sleep 314.3 & wait", &workdir);

    // whatever happens below, nothing with the marker outlives the test
    struct Leaks(&'static str);
    impl Drop for Leaks {
        fn drop(&mut self) {
            for pid in Harness::procs_with_cmdline(self.0) {
                let _ =
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
        }
    }
    let _leaks = Leaks("sleep 314.3");

    let first = DaemonProc::spawn(&home);
    let started = first.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    let old_pid = pid_of(&first, "sweep-me");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Harness::procs_with_cmdline("sleep 314.3").is_empty() {
        assert!(Instant::now() < deadline, "the app never started its sleep");
        std::thread::sleep(Duration::from_millis(20));
    }
    // the shell (its command line holds the script) and its sleep
    let old_procs = Harness::procs_with_cmdline("sleep 314.3");
    assert_eq!(old_procs.len(), 2, "{old_procs:?}");

    first.sigkill();
    let mut first = first;
    first.wait_exit();
    assert!(
        old_procs.iter().all(|p| alive(*p)),
        "SIGKILL of the daemon leaves its children running"
    );

    let second = DaemonProc::spawn(&home);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let hello = second.rpc(json!({ "type": "hello", "client": "test" }));
        if hello["phase"]["state"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "never became ready: {hello}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !old_procs.iter().any(|p| alive(*p)),
        "the old shell and its grandchild were swept"
    );
    let new_pid = pid_of(&second, "sweep-me");
    assert_ne!(new_pid, old_pid, "the app runs again, as a new process");
    assert_eq!(
        Harness::procs_with_cmdline("sleep 314.3").len(),
        2,
        "exactly one copy (a shell and its sleep): the sweep killed the old one, the resurrect did not add a second"
    );
    let _ = second.rpc(json!({ "type": "kill" }));
}

/// A client that started a daemon which lost the start race (exit code 3) waits for the winner
/// instead of reporting a failed startup.
#[tokio::test]
async fn client_autostart_waits_for_the_winner_of_a_start_race() {
    use oxidepm_ipc::{IpcClient, Request, Response};
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // "the daemon this client started": exits at once with the already-running code
    let loser = tmp.path().join("loser.sh");
    std::fs::write(&loser, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&loser, std::fs::Permissions::from_mode(0o755)).unwrap();

    // the winner comes up a moment later
    let winner_home = home.clone();
    let winner = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        DaemonProc::spawn(&winner_home)
    });

    let client = IpcClient::new(home.join("daemon.sock")).with_daemon_bin(&loser);
    let response = client
        .send_autostart(&Request::Ping)
        .await
        .expect("the client waits for the daemon that won the race");
    assert!(matches!(response, Response::Pong), "{response:?}");

    let winner = winner.join().expect("winner thread");
    let _ = winner.rpc(json!({ "type": "kill" }));
}

// ---- shutdown: signals, Kill, saved state ---------------------------------------------------

use common::Harness;
use std::time::{Duration, Instant};

/// An app whose shell runs `on_term` when it gets SIGTERM (`trap '' TERM` ignores it), creates
/// `ready` once the trap is installed, and then idles. `marker` ends up in its command line.
fn trap_app(
    name: &str,
    marker: &str,
    on_term: &str,
    kill_timeout_ms: u64,
    workdir: &std::path::Path,
) -> oxidepm_core::AppSpec {
    let ready = workdir.join(format!("{name}.ready"));
    let script = format!(
        "trap '{on_term}' TERM; touch '{}'; while :; do sleep 0.1; done # {marker}",
        ready.display()
    );
    let mut spec = shell_app(name, &script, workdir);
    spec.kill_timeout_ms = kill_timeout_ms;
    spec
}

/// Start `spec` and wait until its shell has installed its trap; returns the app's pid.
fn start_and_wait_ready(
    daemon: &DaemonProc,
    spec: &oxidepm_core::AppSpec,
    workdir: &std::path::Path,
) -> u32 {
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    let ready = workdir.join(format!("{}.ready", spec.name));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never installed its trap",
            spec.name
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    pid_of(daemon, &spec.name)
}

/// The pid of a running app, from `status`.
fn pid_of(daemon: &DaemonProc, name: &str) -> u32 {
    let status = daemon.rpc(json!({ "type": "status" }));
    status["apps"]
        .as_array()
        .and_then(|apps| apps.iter().find(|a| a["spec"]["name"] == name))
        .and_then(|a| a["state"]["pid"].as_u64())
        .unwrap_or_else(|| panic!("{name} has no pid in {status}")) as u32
}

fn alive(pid: u32) -> bool {
    Harness::pid_alive(pid)
}

/// SIGTERM: the daemon stops the app (which takes a moment), saves the running apps to
/// `saved.json`, and only then exits.
#[test]
fn sigterm_stops_children_saves_state_then_exits() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    let spec = trap_app("term-slow", "309.1", "sleep 0.6; exit 0", 5000, &workdir);
    let pid = start_and_wait_ready(&daemon, &spec, &workdir);

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}; stderr: {}", daemon.stderr());
    assert!(
        signalled.elapsed() >= Duration::from_millis(500),
        "the daemon waited for the app's own shutdown ({:?})",
        signalled.elapsed()
    );
    assert!(
        !alive(pid),
        "the app is gone by the time the daemon has exited"
    );

    let saved = std::fs::read_to_string(home.join("saved.json")).expect("saved.json");
    let names: Vec<_> = saved_entries(&saved)
        .iter()
        .map(|a| a["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("term-slow")], "{saved}");
    assert!(!home.join("daemon.sock").exists(), "the socket is removed");
}

/// An app that ignores SIGTERM is killed when its `kill_timeout_ms` is up.
#[test]
fn sigterm_escalates_to_sigkill_after_kill_timeout() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let spec = trap_app("stubborn-1", "309.2", "", 1000, &workdir);
    let pid = start_and_wait_ready(&daemon, &spec, &workdir);

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}");
    assert!(
        signalled.elapsed() < Duration::from_secs(4),
        "took {:?}",
        signalled.elapsed()
    );
    assert!(!alive(pid), "the app was killed");
}

/// A second SIGTERM during a slow shutdown kills every app at once and ends the daemon (130).
#[test]
fn second_sigterm_forces_immediate_exit() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let spec = trap_app("stubborn-2", "309.3", "", 20_000, &workdir);
    let pid = start_and_wait_ready(&daemon, &spec, &workdir);

    daemon.signal(Signal::SIGTERM);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        daemon.try_exit().is_none(),
        "the first SIGTERM starts a slow shutdown"
    );
    let second = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(
        second.elapsed() < Duration::from_secs(2),
        "took {:?}",
        second.elapsed()
    );
    assert_eq!(status.code(), Some(130), "{status}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the app survived the forced exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `Kill` runs the same shutdown whether or not the client stays to read the answer.
#[test]
fn kill_request_shuts_down_even_if_client_disconnects() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let spec = trap_app("kill-gone", "309.4", "sleep 0.5; exit 0", 5000, &workdir);
    let pid = start_and_wait_ready(&daemon, &spec, &workdir);

    daemon.send_and_hang_up(json!({ "type": "kill" }));
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}");
    assert!(!alive(pid), "the app was stopped");
}

/// The answer to `Kill` means the apps are stopped.
#[test]
fn kill_reply_arrives_after_children_stopped() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let spec = trap_app("kill-wait", "309.5", "sleep 0.6; exit 0", 5000, &workdir);
    let pid = start_and_wait_ready(&daemon, &spec, &workdir);

    let asked = Instant::now();
    let reply = daemon.rpc(json!({ "type": "kill" }));
    assert_eq!(reply["type"], "ok", "{reply}");
    assert!(
        asked.elapsed() >= Duration::from_millis(500),
        "answered after {:?}",
        asked.elapsed()
    );
    assert!(
        !alive(pid),
        "the app is already dead when the answer arrives"
    );
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}");
}

/// SIGHUP (a closed terminal) is ignored: the daemon and its apps carry on.
#[test]
fn sighup_does_not_orphan_children() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let started =
        daemon.rpc(json!({ "type": "start", "spec": sleeper("hup-app", "309.6", &workdir) }));
    assert_eq!(started["type"], "started", "{started}");
    let pid = pid_of(&daemon, "hup-app");

    daemon.signal(Signal::SIGHUP);
    std::thread::sleep(Duration::from_millis(500));
    assert!(daemon.try_exit().is_none(), "the daemon survives SIGHUP");
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    assert!(alive(pid), "and so does its app");
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

/// A `start` (of a new app and of a stopped one) that races SIGTERM never leaves a process
/// behind: it either completes and is stopped by the shutdown, or is refused / cancelled. Both
/// starts wait 200 ms before they spawn; the SIGTERM lands before, around and after that moment
/// in different rounds.
#[test]
fn start_racing_sigterm_leaves_no_process() {
    const SIGNAL_AT_MS: [u64; 7] = [0, 60, 120, 180, 210, 260, 400];
    for (round, signal_at) in SIGNAL_AT_MS.into_iter().enumerate() {
        let round = round as u64;
        let tmp = TempDir::new().expect("temp dir");
        let workdir = tmp.path().join("work");
        std::fs::create_dir_all(&workdir).unwrap();
        let mut daemon = DaemonProc::spawn(&tmp.path().join("home"));

        // a stopped app, to be started again during the shutdown
        let old = sleeper("racer-old", &format!("308.{round}1"), &workdir);
        assert_eq!(
            daemon.rpc(json!({ "type": "start", "spec": old }))["type"],
            "started"
        );
        let stopped = daemon
            .rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "racer-old" } }));
        assert_eq!(stopped["type"], "stopped", "{stopped}");

        let mut old = sleeper("racer-old", &format!("308.{round}1"), &workdir);
        let mut new = sleeper("racer-new", &format!("308.{round}2"), &workdir);
        old.startup_delay_ms = Some(200);
        new.startup_delay_ms = Some(200);

        let replies = std::thread::scope(|scope| {
            let a = scope.spawn(|| daemon.try_rpc(json!({ "type": "start", "spec": old })));
            let b = scope.spawn(|| daemon.try_rpc(json!({ "type": "start", "spec": new })));
            std::thread::sleep(Duration::from_millis(signal_at));
            daemon.signal(Signal::SIGTERM);
            [a.join().unwrap(), b.join().unwrap()]
        });
        let status = daemon.wait_exit();
        assert!(
            status.success(),
            "round {round}: {status}; stderr: {}",
            daemon.stderr()
        );

        for reply in replies.into_iter().flatten() {
            let ok = reply["type"] == "started" || reply["code"] == "E_SHUTTING_DOWN";
            assert!(
                ok,
                "round {round}: a result or E_SHUTTING_DOWN, got {reply}"
            );
        }
        std::thread::sleep(Duration::from_millis(300));
        let left = Harness::procs_with_cmdline(&format!("sleep 308.{round}"));
        assert!(
            left.is_empty(),
            "round {round}: processes left behind: {left:?}"
        );
    }
}

/// Stopped in the middle of the boot resurrect, the daemon neither starts the app it was about
/// to resurrect nor overwrites `saved.json` with the (still empty) list of what is running.
#[test]
fn sigterm_during_boot_resurrect_keeps_saved_file() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    save_slow_app(&home, &tmp.path().join("work"), 8000, "306.4");
    let before = std::fs::read_to_string(home.join("saved.json")).unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    let hello = daemon.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(hello["phase"]["state"], "resurrecting", "{hello}");

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}");
    assert!(
        signalled.elapsed() < Duration::from_secs(4),
        "the app's 8 s startup delay was cancelled, took {:?}",
        signalled.elapsed()
    );
    assert_eq!(
        std::fs::read_to_string(home.join("saved.json")).unwrap(),
        before,
        "saved.json untouched"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        Harness::procs_with_cmdline("sleep 306.4").is_empty(),
        "the saved app was never started"
    );
}

/// One saved app is up, the next one is still waiting out its startup delay when SIGTERM
/// arrives: the boot resurrect was interrupted, so `saved.json` keeps both apps. (The resurrect
/// used to swallow the refused start and report success, and the shutdown then saved only the
/// app that was already running.)
#[test]
fn sigterm_in_the_middle_of_the_boot_resurrect_keeps_every_saved_app() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let first = sleeper("resurrect-first", "306.5", &workdir);
    let mut second = sleeper("resurrect-second", "306.6", &workdir);
    second.startup_delay_ms = Some(8000);
    std::fs::write(home.join("saved.json"), saved_json(&[first, second])).unwrap();
    let before = std::fs::read_to_string(home.join("saved.json")).unwrap();

    let mut daemon = DaemonProc::spawn(&home);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Harness::procs_with_cmdline("sleep 306.5").is_empty() {
        assert!(
            Instant::now() < deadline,
            "the first saved app never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let hello = daemon.rpc(json!({ "type": "hello", "client": "test" }));
    assert_eq!(
        hello["phase"]["state"], "resurrecting",
        "the second app is still waiting: {hello}"
    );

    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}");
    let after = std::fs::read_to_string(home.join("saved.json")).unwrap();
    let names = |text: &str| -> Vec<String> {
        saved_entries(text)
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names(&after),
        names(&before),
        "both saved apps are still listed"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        Harness::procs_with_cmdline("sleep 306.").is_empty(),
        "nothing is left running"
    );
}

/// Four apps that ignore SIGTERM and have 30 s kill timeouts: the 10 s shutdown budget (from
/// `daemon.toml`) bounds the shutdown.
#[cfg(feature = "slow-tests")]
#[test]
fn shutdown_respects_budget_with_slow_apps() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(home.join("daemon.toml"), "shutdown_budget_secs = 10\n").unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    let pids: Vec<u32> = (0..4)
        .map(|n| {
            let spec = trap_app(
                &format!("slowpoke-{n}"),
                &format!("309.7{n}"),
                "",
                30_000,
                &workdir,
            );
            start_and_wait_ready(&daemon, &spec, &workdir)
        })
        .collect();

    let signalled = Instant::now();
    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit_for(Duration::from_secs(40));
    assert!(status.success(), "{status}");
    let took = signalled.elapsed();
    assert!(
        took >= Duration::from_secs(9),
        "the budget, not less, was used ({took:?})"
    );
    // The four apps stop in one layer, so the cap is the whole budget (about 10 s). Without the
    // cap the SIGKILL fallback (cap + 15 s) would still end it near 26 s, so the bound sits
    // well under that.
    assert!(
        took < Duration::from_secs(15),
        "bounded by the budget, took {took:?}"
    );
    assert!(pids.iter().all(|pid| !alive(*pid)), "every app is dead");
}

/// A bad `daemon.toml` is a startup error that names the file, not a silent default.
#[test]
fn invalid_daemon_config_stops_the_daemon_with_a_clear_message() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("daemon.toml"), "shutdown_budget_secs = 5\n").unwrap();
    let mut daemon = DaemonProc::spawn_unready(&home);
    let status = daemon.wait_exit();
    assert_eq!(status.code(), Some(1), "{status}");
    let stderr = daemon.stderr();
    assert!(
        stderr.contains("daemon.toml") && stderr.contains("between 10 and 900"),
        "{stderr}"
    );
}

/// The grandchildren of an app die with it when the daemon is told to stop.
#[test]
fn sigterm_leaves_no_grandchildren() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut daemon = DaemonProc::spawn(&home);
    let marker = format!("302.{:05}1", std::process::id() % 100_000);
    let ready = workdir.join("grandchild.ready");
    let script = format!("sleep {marker} & touch '{}'; wait", ready.display());
    let spec = shell_app("grandparent", &script, &workdir);
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "the app never started its grandchild"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !Harness::procs_with_cmdline(&format!("sleep {marker}")).is_empty(),
        "the grandchild runs"
    );

    daemon.signal(Signal::SIGTERM);
    let status = daemon.wait_exit();
    assert!(status.success(), "{status}; stderr: {}", daemon.stderr());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !Harness::procs_with_cmdline(&marker).is_empty() {
        assert!(
            Instant::now() < deadline,
            "a grandchild outlived the daemon"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `start_batch` over the wire: the dependent is listed first, one item per spec comes back in
/// the order sent, a refused app is its own error with its code, and `stop all` stops the
/// dependent first. A `daemon.toml` with `launch_concurrency` is read.
#[test]
fn start_batch_over_ipc_reports_each_app() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("daemon.toml"), "launch_concurrency = 2\n").unwrap();
    let daemon = DaemonProc::spawn(&home);

    let mut web = sleeper("wire-web", "314.1", &workdir);
    web.depends_on = vec!["wire-db".to_string()];
    let db = sleeper("wire-db", "314.2", &workdir);
    let mut lost = sleeper("wire-lost", "314.3", &workdir);
    lost.depends_on = vec!["wire-nowhere".to_string()];

    let reply = daemon.rpc(json!({ "type": "start_batch", "specs": [web, db, lost] }));
    assert_eq!(reply["type"], "started_batch", "{reply}");
    let items = reply["items"].as_array().expect("items");
    let names: Vec<&str> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["wire-web", "wire-db", "wire-lost"]);
    assert_eq!(items[0]["result"]["outcome"], "ok", "{reply}");
    assert_eq!(items[1]["result"]["outcome"], "ok", "{reply}");
    assert_eq!(items[2]["result"]["outcome"], "err", "{reply}");
    assert_eq!(
        items[2]["result"]["code"], "E_DEPENDENCY_MISSING",
        "{reply}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = daemon.rpc(json!({ "type": "status" }));
        let running = status["apps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["state"]["status"] == "running")
            .count();
        if running == 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not both running: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let stopped = daemon.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
    assert_eq!(stopped["type"], "stopped", "{stopped}");
    assert_eq!(stopped["count"], 2, "{stopped}");
    let _ = daemon.rpc(json!({ "type": "kill" }));
}
