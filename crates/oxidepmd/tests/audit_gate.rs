//! The audit log in process: the writer is held with a test hook, the clock is moved by hand.
//! Real supervisor, private home.

mod common;

use common::*;
use oxidepm_core::actor::{ActorClass, ActorId, Via};
use oxidepm_core::AppStatus;
use oxidepm_ipc::{Envelope, Request, Response};
use oxidepmd::audit::{Audit, AuditConfig};
use oxidepmd::handlers::RequestHandler;
use oxidepmd::policy::{PeerCred, ReqCtx};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ctx(claim: Option<&str>, request: &Request) -> ReqCtx {
    let mut envelope = Envelope::new("req-audit", request.clone());
    envelope.actor = claim.map(str::to_string);
    ReqCtx::new(PeerCred::default(), Via::Main, &envelope)
}

fn adapter_ctx(name: &str, request: &Request) -> ReqCtx {
    let mut ctx = ctx(None, request);
    ctx.actor = ActorId::new(ActorClass::Adapter, name, Via::AdapterChannel);
    ctx
}

fn selector(name: &str) -> oxidepm_core::Selector {
    oxidepm_core::Selector::ByName(name.into())
}

fn sleeper(h: &Harness, name: &str, extra: Value) -> oxidepm_core::AppSpec {
    h.cmd_with(name, &format!("exec sleep {}", h.marker()), extra)
}

/// Quick settings: the real limits would make a test slow.
fn quick(paths: &oxidepm_core::Paths) -> AuditConfig {
    let mut config = AuditConfig::new(paths);
    for writer in [&mut config.main, &mut config.security] {
        writer.retry = Duration::from_millis(60);
        writer.sync_interval = Duration::from_millis(20);
    }
    config
}

fn lines(dir: &Path, file: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

async fn wait_lines<T>(dir: &Path, file: &str, pick: impl Fn(&[Value]) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let all = lines(dir, file);
        if let Some(found) = pick(&all) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "nothing found in {file}: {all:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn daemon_log_bounds_and_escapes_client_strings() {
    use common::daemon::DaemonProc;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let mut daemon = DaemonProc::spawn_with_env(&home, &[("RUST_LOG", "debug")]);
    let suffix = "a".repeat(1024);
    let id = format!("request\n\r\x1b[0m{suffix}");
    let client = format!("client\n\r\x1b[0m{suffix}");
    let reply = daemon.rpc_on(
        &daemon.socket(),
        &id,
        None,
        json!({ "type": "hello", "client": client }),
    );
    assert_eq!(reply["request_id"], id);
    assert_eq!(reply["response"]["type"], "hello");

    let name = format!("app\n\r\x1b[0m{suffix}");
    let mut spec = oxidepm_core::AppSpec::new(
        "app".into(),
        oxidepm_core::Runtime::CMD,
        "/bin/true".into(),
        dir.path().to_path_buf(),
    );
    spec.name = name.clone();
    assert_eq!(
        daemon.rpc(json!({ "type": "start", "spec": spec }))["type"],
        "error"
    );
    let reply = daemon.rpc(json!({ "type": "describe", "selector": selector(&name) }));
    assert_eq!(reply["type"], "error");
    assert_eq!(daemon.rpc(json!({ "type": "kill" }))["type"], "ok");
    assert!(daemon.wait_exit().success());

    let log: String = std::fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("daemon.log")
        })
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    for prefix in [
        "request request",
        "Hello from client",
        "Handling start request for: app",
        "Handling describe request for: app",
    ] {
        assert!(
            log.contains(&format!(r"{prefix}\n\r\u{{1b}}[0m")),
            "{prefix}"
        );
    }
    assert!(!log.contains('\r'));
    assert!(!log.contains('\x1b'));
    assert!(!log.contains(&suffix));
}

#[test]
fn non_human_ping_and_hello_share_the_request_budget() {
    use common::daemon::DaemonProc;

    let dir = tempfile::tempdir().unwrap();
    let daemon = DaemonProc::spawn(&dir.path().join("home"));
    daemon.wait_for_boot();
    let mut limited = [false; 2];
    for n in 0..300 {
        let (request, expected) = if n % 2 == 0 {
            (json!({ "type": "ping" }), "pong")
        } else {
            (json!({ "type": "hello", "client": "test" }), "hello")
        };
        let reply = daemon.rpc_on(&daemon.socket(), "poll", Some("ai:poller"), request);
        let response = &reply["response"];
        if response["type"] == "error" {
            assert_eq!(response["code"], "E_RATE_LIMITED", "{response}");
            limited[n % 2] = true;
        } else {
            assert_eq!(response["type"], expected, "{response}");
        }
        if limited == [true, true] {
            break;
        }
    }
    assert_eq!(limited, [true, true]);
    assert_eq!(daemon.rpc(json!({ "type": "ping" }))["type"], "pong");
    assert_eq!(
        daemon.rpc(json!({ "type": "hello", "client": "cli" }))["type"],
        "hello"
    );
}

#[tokio::test]
async fn decision_is_durable_before_the_effect() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "guarded", json!({ "ai_control": true })))
        .await
        .unwrap();
    let before = h.pid_of("guarded").await;
    let handler = Arc::new(RequestHandler::with_audit(
        Arc::clone(&h.sup),
        Arc::new(Audit::with_config(quick(&h.paths))),
    ));
    let hold = handler.audit().writers().0.pause();

    let request = Request::Stop {
        selector: selector("guarded"),
    };
    let task = {
        let (handler, ctx, request) = (
            Arc::clone(&handler),
            ctx(Some("ai:test"), &request),
            request.clone(),
        );
        tokio::spawn(async move { handler.guarded(&ctx, request).await })
    };
    // the writer is held: the decision is not on disk, so the app has not been touched
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!task.is_finished(), "the request waits for its record");
    assert_eq!(h.state("guarded").await.status, AppStatus::Running);
    assert_eq!(h.pid_of("guarded").await, before);
    assert!(lines(h.paths.audit_dir().as_path(), "audit.jsonl").is_empty());

    drop(hold);
    let response = task
        .await
        .unwrap()
        .expect("the stop ran once the decision was written");
    assert!(
        matches!(response, Response::Stopped { count: 1, .. }),
        "{response:?}"
    );
    h.wait_status("guarded", AppStatus::Stopped).await;
    // the decision is the line before the effect: it is on disk by now
    let dir = h.paths.audit_dir();
    let line = wait_lines(&dir, "audit.jsonl", |l| {
        l.iter().find(|x| x["decision"] == "allow").cloned()
    })
    .await;
    assert_eq!(line["op"], "stop");
}

#[tokio::test]
async fn a_decision_that_times_out_refuses_the_request() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "kept", json!({ "ai_control": true })))
        .await
        .unwrap();
    let mut config = quick(&h.paths);
    config.ack_timeout = Duration::from_millis(200);
    let handler =
        RequestHandler::with_audit(Arc::clone(&h.sup), Arc::new(Audit::with_config(config)));
    let _hold = handler.audit().writers().0.pause();
    let request = Request::Stop {
        selector: selector("kept"),
    };
    let started = Instant::now();
    let err = handler
        .guarded(&ctx(Some("ai:test"), &request), request)
        .await
        .unwrap_err();
    assert_eq!(err.code().as_str(), "E_AUDIT_UNAVAILABLE", "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(h.state("kept").await.status, AppStatus::Running);
}

#[tokio::test]
async fn human_request_never_waits_for_a_full_audit_queue() {
    let h = Harness::new().await;
    for n in 0..10 {
        h.start(sleeper(&h, &format!("p{n}"), json!({})))
            .await
            .unwrap();
    }
    let mut config = quick(&h.paths);
    config.main.queue = 2;
    config.security.queue = 2;
    let handler =
        RequestHandler::with_audit(Arc::clone(&h.sup), Arc::new(Audit::with_config(config)));
    let _hold = handler.audit().writers().0.pause();

    let started = Instant::now();
    for n in 0..10 {
        let request = Request::Stop {
            selector: selector(&format!("p{n}")),
        };
        let response = handler
            .guarded(&ctx(None, &request), request)
            .await
            .expect("a person's stop runs");
        assert!(
            matches!(response, Response::Stopped { count: 1, .. }),
            "{response:?}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let state = handler.audit().state();
    assert_eq!(state.state, "degraded", "{state:?}");
    assert!(state.lost > 0, "the lost lines are counted: {state:?}");
}

#[tokio::test]
async fn audit_recovers_when_directory_is_writable_again() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "recovering", json!({ "ai_control": true })))
        .await
        .unwrap();
    let dir = h.paths.audit_dir();
    std::fs::write(&dir, b"in the way").unwrap();
    let handler = RequestHandler::with_audit(
        Arc::clone(&h.sup),
        Arc::new(Audit::with_config(quick(&h.paths))),
    );

    let request = Request::Stop {
        selector: selector("recovering"),
    };
    let err = handler
        .guarded(&ctx(Some("ai:test"), &request), request.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code().as_str(), "E_AUDIT_UNAVAILABLE", "{err}");
    assert_eq!(handler.audit().state().state, "degraded");

    std::fs::remove_file(&dir).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let response = handler
        .guarded(&ctx(Some("ai:test"), &request), request)
        .await
        .expect("the log is back");
    assert!(
        matches!(response, Response::Stopped { count: 1, .. }),
        "{response:?}"
    );
    assert_eq!(handler.audit().state().state, "ok");
    // the lines that were lost are reported by the first one that gets through
    wait_lines(&dir, "audit.jsonl", |l| {
        l.iter()
            .find(|x| x["phase"] == "note" && x["dropped"].is_u64())
            .cloned()
    })
    .await;
}

#[tokio::test]
async fn denials_are_recorded_for_every_actor() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "target", json!({}))).await.unwrap();
    let handler = RequestHandler::with_audit(
        Arc::clone(&h.sup),
        Arc::new(Audit::with_config(quick(&h.paths))),
    );
    let dir = h.paths.audit_dir();

    // an AI actor on the main socket and one on the agent socket, and an adapter
    let request = Request::Delete {
        selector: selector("target"),
    };
    let mut agent = ctx(Some("ai:other-agent"), &request);
    agent.actor = ActorId::from_connection(Via::Agent, Some("ai:other-agent"));
    for context in [
        ctx(Some("ai:test"), &request),
        agent,
        adapter_ctx("ui-web", &request),
    ] {
        let err = handler
            .guarded(&context, request.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code().as_str(), "E_POLICY_DENIED", "{err}");
    }
    for file in ["audit.jsonl", "audit-security.jsonl"] {
        let denials = wait_lines(&dir, file, |l| {
            let d: Vec<Value> = l
                .iter()
                .filter(|x| x["decision"] == "deny")
                .cloned()
                .collect();
            (d.len() >= 3).then_some(d)
        })
        .await;
        let who: Vec<String> = denials
            .iter()
            .map(|d| {
                format!(
                    "{}:{}:{}",
                    d["actor"]["class"].as_str().unwrap(),
                    d["actor"]["label"].as_str().unwrap(),
                    d["actor"]["via"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            who,
            [
                "ai:test:main",
                "ai:other-agent:agent",
                "adapter:ui-web:adapter"
            ],
            "{file}"
        );
    }
    assert_eq!(h.state("target").await.status, AppStatus::Running);
}

#[test]
fn repeated_refused_verb_or_session_requests_have_a_security_audit_budget() {
    use common::daemon::DaemonProc;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let mut daemon = DaemonProc::spawn(&home);
    daemon.wait_for_boot();
    for n in 0..24 {
        let op = if n % 2 == 0 { "invoke" } else { "session_open" };
        let reply = daemon.rpc_on(
            &daemon.socket(),
            "lookup",
            Some("ai:lookup"),
            json!({ "type": "adapter", "op": op, "verb": "missing", "cwd": dir.path() }),
        );
        assert_eq!(reply["response"]["type"], "error", "{reply}");
        assert_ne!(reply["response"]["code"], "E_RATE_LIMITED", "{reply}");
    }
    assert_eq!(daemon.rpc(json!({ "type": "kill" }))["type"], "ok");
    assert!(daemon.wait_exit().success());

    let dir = home.join("audit");
    let main = lines(&dir, "audit.jsonl");
    let security = lines(&dir, "audit-security.jsonl");
    let refusals = |records: &[Value]| {
        records
            .iter()
            .filter(|line| line["decision"] == "deny")
            .count()
    };
    assert_eq!(refusals(&main), 24);
    assert_eq!(refusals(&security), 10);
    for op in ["adapter invoke", "adapter session", "kill"] {
        assert!(security.iter().any(|line| line["op"] == op), "{op}");
    }
}

#[tokio::test]
async fn adapter_status_polling_is_coalesced_into_one_line_per_minute() {
    let h = Harness::new().await;
    // a clock the test moves by hand
    let origin = Instant::now();
    let offset = Arc::new(parking_lot::Mutex::new(Duration::ZERO));
    let mut config = quick(&h.paths);
    config.clock = {
        let offset = Arc::clone(&offset);
        Arc::new(move || origin + *offset.lock())
    };
    let handler =
        RequestHandler::with_audit(Arc::clone(&h.sup), Arc::new(Audit::with_config(config)));
    let dir = h.paths.audit_dir();

    for _ in 0..1000 {
        let request = Request::Status;
        // the adapter polls inside the burst the limiter allows: spread over simulated seconds
        *offset.lock() += Duration::from_millis(50);
        handler
            .guarded(&adapter_ctx("notifier", &request), request)
            .await
            .expect("status");
    }
    // 50 simulated seconds: the window is not over, nothing was written
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        lines(&dir, "audit.jsonl").is_empty(),
        "{:?}",
        lines(&dir, "audit.jsonl")
    );

    *offset.lock() += Duration::from_secs(20);
    let line = wait_lines(&dir, "audit.jsonl", |l| l.first().cloned()).await;
    assert_eq!(line["op"], "status");
    assert_eq!(line["count"], 1000);
    assert_eq!(line["actor"]["class"], "adapter");
    assert_eq!(line["actor"]["label"], "notifier");
    assert!(line["first"].is_string() && line["last"].is_string());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        lines(&dir, "audit.jsonl").len(),
        1,
        "one line for the window"
    );
}

#[tokio::test]
async fn an_adapter_is_rate_limited_by_its_name() {
    let h = Harness::new().await;
    let handler = RequestHandler::with_audit(
        Arc::clone(&h.sup),
        Arc::new(Audit::with_config(quick(&h.paths))),
    );
    let request = Request::Status;
    let mut limited = 0;
    for _ in 0..300 {
        if let Err(e) = handler
            .guarded(&adapter_ctx("busy", &request), request.clone())
            .await
        {
            assert_eq!(e.code().as_str(), "E_RATE_LIMITED", "{e}");
            limited += 1;
        }
    }
    assert!(limited > 100, "{limited}");
    // another adapter, and a person, have their own allowance
    handler
        .guarded(&adapter_ctx("quiet", &request), request.clone())
        .await
        .expect("its own bucket");
    handler
        .guarded(&ctx(None, &request), request)
        .await
        .expect("people are not limited");
}
