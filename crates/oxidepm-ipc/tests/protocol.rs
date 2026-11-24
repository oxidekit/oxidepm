//! The wire format: envelopes, error codes, selectors, version checks and client timeouts.

use oxidepm_core::{ErrorCode, Selector};
use oxidepm_ipc::server::{Incoming, Reply};
use oxidepm_ipc::{
    DryRunPlan, Envelope, IpcClient, IpcServer, PlannedItem, PolicyDecision, Request, Response,
    ResponseEnvelope,
};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[test]
fn envelope_roundtrips_serialise() {
    let mut envelope = Envelope::new(
        "req-1",
        Request::Stop {
            selector: Selector::ByTag("web".into()),
        },
    );
    envelope.actor = Some("test".into());
    envelope.reveal = true;
    let json = serde_json::to_string(&envelope).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["protocol"], 2);
    assert_eq!(value["request_id"], "req-1");
    assert_eq!(value["actor"], "test");
    assert_eq!(value["reveal"], true);
    assert_eq!(value["request"]["type"], "stop");
    assert_eq!(
        value["request"]["selector"],
        serde_json::json!({"by": "tag", "value": "web"})
    );

    let back: Envelope = serde_json::from_str(&json).unwrap();
    assert_eq!(back.request_id, "req-1");
    assert!(back.reveal);
    assert!(matches!(back.request, Request::Stop { selector: Selector::ByTag(t) } if t == "web"));

    // actor and reveal are optional on the wire
    let minimal: Envelope =
        serde_json::from_str(r#"{"protocol":1,"request_id":"x","request":{"type":"ping"}}"#)
            .unwrap();
    assert!(!minimal.reveal && minimal.actor.is_none());

    let response = ResponseEnvelope::new("req-1", Response::Pong);
    let json = serde_json::to_string(&response).unwrap();
    let back: ResponseEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!((back.protocol, back.request_id.as_str()), (2, "req-1"));
    assert!(matches!(back.response, Response::Pong));
}

/// `Request::name` is the `type` tag, for every request.
#[test]
fn request_name_is_the_wire_tag() {
    use oxidepm_core::{AppSpec, Runtime};
    let sel = || Selector::All;
    let spec = || AppSpec::new("a".into(), Runtime::CMD, "true".into(), "/".into());
    let all = vec![
        Request::Hello { client: "t".into() },
        Request::Ping,
        Request::Start { spec: spec() },
        Request::StartBatch {
            specs: vec![spec()],
        },
        Request::Stop { selector: sel() },
        Request::Restart { selector: sel() },
        Request::Delete { selector: sel() },
        Request::Status,
        Request::Show { selector: sel() },
        Request::Logs {
            selector: sel(),
            lines: 1,
            follow: false,
            stdout: true,
            stderr: true,
        },
        Request::Save { force: false },
        Request::Resurrect,
        Request::Kill,
        Request::Reload { selector: sel() },
        Request::Flush { selector: sel() },
        Request::Describe { selector: sel() },
    ];
    for request in all {
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["type"], request.name());
    }
}

#[test]
fn response_error_carries_code() {
    let response = Response::error(ErrorCode::AppNotFound, "App not found: web");
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["code"], "E_APP_NOT_FOUND");
    assert_eq!(value["message"], "App not found: web");

    let from_error = Response::from_error(&oxidepm_core::Error::AppAlreadyExists("web".into()));
    assert_eq!(from_error.error_code(), Some(&ErrorCode::AppExists));
}

#[test]
fn unknown_error_code_deserialises_to_other() {
    let response: Response =
        serde_json::from_str(r#"{"type":"error","code":"E_NEW_THING","message":"m"}"#).unwrap();
    assert_eq!(
        response.error_code(),
        Some(&ErrorCode::Other("E_NEW_THING".into()))
    );

    // an older daemon's error has no code at all
    let old: Response = serde_json::from_str(r#"{"type":"error","message":"m"}"#).unwrap();
    assert_eq!(old.error_message(), Some("m"));
    assert_eq!(old.error_code(), Some(&ErrorCode::Internal));
}

#[test]
fn selector_serde_roundtrips_every_variant() {
    let cases = [
        (Selector::All, serde_json::json!({"by": "all"})),
        (
            Selector::ById(7),
            serde_json::json!({"by": "id", "value": 7}),
        ),
        (
            Selector::ByName("web".into()),
            serde_json::json!({"by": "name", "value": "web"}),
        ),
        (
            Selector::ByTag("frontend".into()),
            serde_json::json!({"by": "tag", "value": "frontend"}),
        ),
    ];
    for (selector, wire) in cases {
        assert_eq!(serde_json::to_value(&selector).unwrap(), wire);
        let back: Selector = serde_json::from_value(wire).unwrap();
        assert_eq!(back, selector, "a {selector:?} must come back as itself");
    }
    // the bug: an untagged selector read a tag back as a name
    let tag: Selector = serde_json::from_str(r#"{"by":"tag","value":"x"}"#).unwrap();
    assert_eq!(tag, Selector::ByTag("x".into()));
}

#[test]
fn daemon_phase_wire_shape_roundtrips() {
    use oxidepm_ipc::protocol::DaemonPhase;
    let cases = [
        (
            DaemonPhase::Booting,
            serde_json::json!({ "state": "booting" }),
        ),
        (
            DaemonPhase::Resurrecting { done: 1, total: 3 },
            serde_json::json!({ "state": "resurrecting", "done": 1, "total": 3 }),
        ),
        (DaemonPhase::Ready, serde_json::json!({ "state": "ready" })),
        (
            DaemonPhase::ShuttingDown,
            serde_json::json!({ "state": "shutting_down" }),
        ),
    ];
    for (phase, wire) in cases {
        assert_eq!(serde_json::to_value(&phase).unwrap(), wire);
        assert_eq!(serde_json::from_value::<DaemonPhase>(wire).unwrap(), phase);
    }
}

fn socket_in(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("daemon.sock")
}

async fn read_line(stream: &mut UnixStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("an answer arrives")
        .unwrap();
    line
}

#[tokio::test]
async fn bare_request_gets_bare_protocol_error() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client.write_all(b"{\"type\":\"ping\"}\n").await.unwrap();
    let mut conn = server.accept().await.unwrap();
    match conn.read_request().await.unwrap() {
        Some(Incoming::Reject { reply, close }) => {
            assert!(
                close,
                "the connection of a client that does not speak envelopes is closed"
            );
            conn.send_reply(&reply).await.unwrap();
        }
        other => panic!("expected a rejection, got {other:?}"),
    }

    // the answer is in the old shape: no envelope, so an old client reads it as `Error{message}`
    let answer = read_line(&mut client).await;
    let value: serde_json::Value = serde_json::from_str(answer.trim()).unwrap();
    assert!(
        value.get("protocol").is_none() && value.get("response").is_none(),
        "{value}"
    );
    assert_eq!(value["type"], "error");
    assert_eq!(value["code"], "E_PROTOCOL_VERSION");
    assert!(
        value["message"].as_str().unwrap().contains("2..2"),
        "{value}"
    );
}

#[tokio::test]
async fn envelope_with_unsupported_protocol_is_rejected_by_version() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    // a request type this build has never heard of must not matter: the version is checked first
    client
        .write_all(b"{\"protocol\":99,\"request_id\":\"r9\",\"request\":{\"type\":\"teleport\"}}\n")
        .await
        .unwrap();
    let mut conn = server.accept().await.unwrap();
    match conn.read_request().await.unwrap() {
        Some(Incoming::Reject {
            reply: Reply::Envelope(reply),
            close,
        }) => {
            assert!(close);
            assert_eq!(reply.request_id, "r9");
            assert_eq!(
                reply.response.error_code(),
                Some(&ErrorCode::ProtocolVersion)
            );
            let message = reply.response.error_message().unwrap();
            assert!(
                message.contains("99") && message.contains("2..2"),
                "{message}"
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_request_type_in_a_good_envelope_is_a_bad_request_and_keeps_the_connection() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client
        .write_all(b"{\"protocol\":2,\"request_id\":\"r2\",\"request\":{\"type\":\"teleport\"}}\n")
        .await
        .unwrap();
    let mut conn = server.accept().await.unwrap();
    match conn.read_request().await.unwrap() {
        Some(Incoming::Reject {
            reply: Reply::Envelope(reply),
            close,
        }) => {
            assert!(!close);
            assert_eq!(reply.request_id, "r2");
            assert_eq!(reply.response.error_code(), Some(&ErrorCode::BadRequest));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// A listener that accepts and never answers: the client gives up with `E_TIMEOUT` inside the
/// timeout class of the request (fast for status, slow for stop).
#[tokio::test]
async fn client_times_out_with_e_timeout_when_daemon_never_answers() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let _holder = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream); // accept, read nothing, answer nothing
        }
    });

    let client = IpcClient::new(path.clone())
        .with_timeouts(Duration::from_millis(300), Duration::from_millis(900));

    let started = Instant::now();
    let err = client
        .send(&Request::Status)
        .await
        .expect_err("nobody answers");
    let fast = started.elapsed();
    assert_eq!(err.code(), ErrorCode::Timeout, "{err}");
    assert!(
        fast >= Duration::from_millis(250) && fast < Duration::from_secs(3),
        "{fast:?}"
    );

    let started = Instant::now();
    let err = client
        .send(&Request::Stop {
            selector: Selector::All,
        })
        .await
        .expect_err("nobody answers");
    let slow = started.elapsed();
    assert_eq!(err.code(), ErrorCode::Timeout, "{err}");
    assert!(
        slow >= Duration::from_millis(800) && slow < Duration::from_secs(5),
        "{slow:?}"
    );
}

/// A daemon that closes the connection without a word (an older build that cannot parse an
/// envelope) gets a readable error, not a JSON parse failure.
#[tokio::test]
async fn silent_close_is_reported_with_a_hint() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let client = IpcClient::new(path);
    let err = client.send(&Request::Ping).await.expect_err("no answer");
    let text = err.to_string();
    assert!(text.contains("oxidepm kill"), "{text}");
}

// ---- protocol 2: dry run and the version fallback -------------------------------

#[test]
fn envelope_dry_run_roundtrips() {
    let mut envelope = Envelope::new(
        "req-d",
        Request::Stop {
            selector: Selector::All,
        },
    );
    envelope.dry_run = true;
    let json = serde_json::to_string(&envelope).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["dry_run"], true, "{json}");
    let back: Envelope = serde_json::from_str(&json).unwrap();
    assert!(back.dry_run && back.request_id == "req-d");

    // a request that is not a dry run puts nothing on the wire (the line an older daemon reads
    // is the line it always read)
    let plain = serde_json::to_value(Envelope::new("req-p", Request::Ping)).unwrap();
    assert!(plain.get("dry_run").is_none(), "{plain}");

    // the response is `dry_run` with the plan inside
    let plan = DryRunPlan::allowed(
        "stop",
        vec![PlannedItem::new(Some(1), "a", "stop").order(0)],
        vec![],
    );
    let json = serde_json::to_string(&ResponseEnvelope::new(
        "req-d",
        Response::DryRun { plan: plan.clone() },
    ))
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["response"]["type"], "dry_run", "{json}");
    match serde_json::from_str::<ResponseEnvelope>(&json)
        .unwrap()
        .response
    {
        Response::DryRun { plan: back } => assert_eq!(back, plan),
        other => panic!("{other:?}"),
    }
}

#[test]
fn dry_run_defaults_false() {
    let envelope: Envelope =
        serde_json::from_str(r#"{"protocol":2,"request_id":"x","request":{"type":"ping"}}"#)
            .unwrap();
    assert!(!envelope.dry_run);
    assert!(!Envelope::new("y", Request::Ping).dry_run);
    assert!(!oxidepm_ipc::CallOptions::default().dry_run);
}

#[test]
fn plan_serialises_with_order() {
    let plan = DryRunPlan {
        op: "stop".into(),
        decision: PolicyDecision::Allow,
        items: vec![
            PlannedItem::new(Some(2), "web", "stop")
                .order(0)
                .detail("running"),
            PlannedItem::new(Some(1), "db", "stop").order(1),
            PlannedItem::new(None, "new", "would_create"),
        ],
        warnings: vec!["w".into()],
    };
    let value = serde_json::to_value(&plan).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "op": "stop",
            "decision": { "decision": "allow" },
            "items": [
                { "id": 2, "name": "web", "action": "stop", "detail": "running", "order": 0 },
                { "id": 1, "name": "db", "action": "stop", "detail": null, "order": 1 },
                { "id": null, "name": "new", "action": "would_create", "detail": null, "order": null },
            ],
            "warnings": ["w"],
        })
    );
    assert_eq!(serde_json::from_value::<DryRunPlan>(value).unwrap(), plan);

    let denied = DryRunPlan::denied("kill", ErrorCode::PolicyDenied, "not for you");
    let value = serde_json::to_value(&denied).unwrap();
    assert_eq!(
        value["decision"],
        serde_json::json!({ "decision": "deny", "code": "E_POLICY_DENIED", "message": "not for you" })
    );
    assert_eq!(serde_json::from_value::<DryRunPlan>(value).unwrap(), denied);
    // a plan without items or warnings still reads
    let bare: DryRunPlan =
        serde_json::from_str(r#"{"op":"x","decision":{"decision":"allow"}}"#).unwrap();
    assert!(bare.items.is_empty() && bare.warnings.is_empty());
}

fn classify(protocol: u32, request: &str) -> Incoming {
    oxidepm_ipc::server::classify_line(&format!(
        "{{\"protocol\":{protocol},\"request_id\":\"c\",\"request\":{request}}}"
    ))
}

#[test]
fn old_protocol_gets_e_protocol_version() {
    // every request but the three that cross versions is refused for protocol 1, whatever it is
    for request in [
        r#"{"type":"status"}"#,
        r#"{"type":"stop","selector":{"by":"all"}}"#,
        r#"{"type":"save"}"#,
        r#"{"type":"resurrect"}"#,
        r#"{"type":"teleport"}"#,
    ] {
        match classify(1, request) {
            Incoming::Reject {
                reply: Reply::Envelope(reply),
                close,
            } => {
                assert!(close, "{request}");
                assert_eq!(
                    reply.response.error_code(),
                    Some(&ErrorCode::ProtocolVersion),
                    "{request}"
                );
                let message = reply.response.error_message().unwrap();
                assert!(
                    message.contains("protocol 1") && message.contains("2..2"),
                    "{message}"
                );
            }
            other => panic!("{request}: expected a version rejection, got {other:?}"),
        }
    }
    // protocol 0 and a newer protocol are refused even for the three
    for protocol in [0, 3, 99] {
        assert!(
            matches!(
                classify(protocol, r#"{"type":"kill"}"#),
                Incoming::Reject { .. }
            ),
            "{protocol}"
        );
    }
    // `hello`, `ping` and `kill` cross versions, in both directions of an update
    for request in [
        r#"{"type":"hello","client":"old"}"#,
        r#"{"type":"ping"}"#,
        r#"{"type":"kill"}"#,
    ] {
        assert!(
            matches!(classify(1, request), Incoming::Request(_)),
            "{request}"
        );
        assert!(
            matches!(classify(2, request), Incoming::Request(_)),
            "{request}"
        );
    }
    // the current protocol is accepted for everything
    assert!(matches!(
        classify(2, r#"{"type":"status"}"#),
        Incoming::Request(_)
    ));
}

/// A daemon of the first protocol, as a thread on a socket: it answers a protocol-1 envelope
/// (`kill`: ok) and refuses any other number the way `classify_line` of that release did, with
/// `E_PROTOCOL_VERSION` in an envelope, and closes the connection. Returns the numbers and
/// request types it saw.
/// The protocol numbers and request types an [`old_daemon`] has seen.
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(u64, String)>>>;

fn old_daemon(path: std::path::PathBuf) -> (std::thread::JoinHandle<()>, Seen) {
    use std::io::{BufRead, Write};
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let number = value["protocol"].as_u64().unwrap();
            let kind = value["request"]["type"].as_str().unwrap().to_string();
            log.lock().unwrap().push((number, kind.clone()));
            let id = value["request_id"].as_str().unwrap();
            let response = if number == 1 {
                serde_json::json!({ "type": "ok", "message": format!("old daemon: {kind}") })
            } else {
                serde_json::json!({ "type": "error", "code": "E_PROTOCOL_VERSION",
                    "message": format!("client speaks protocol {number}, daemon speaks 1..1") })
            };
            let reply =
                serde_json::json!({ "protocol": 1, "request_id": id, "response": response });
            let mut stream = stream;
            let _ = writeln!(stream, "{reply}");
            if number != 1 {
                // the refused connection is closed
                drop(stream);
            }
            if kind == "kill" && number == 1 {
                return;
            }
        }
    });
    (handle, seen)
}

#[tokio::test]
async fn new_cli_can_kill_old_daemon() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let (handle, seen) = old_daemon(path.clone());

    let client = IpcClient::new(path);
    let response = client
        .send(&Request::Kill)
        .await
        .expect("the old daemon answers the repeat");
    match response {
        Response::Ok { message } => assert_eq!(message, "old daemon: kill"),
        other => panic!("{other:?}"),
    }
    handle.join().unwrap();
    // first with this build's number, refused, then again with 1
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(2, "kill".to_string()), (1, "kill".to_string())]
    );
}

#[tokio::test]
async fn only_ping_hello_and_kill_fall_back_to_the_old_protocol() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let (handle, seen) = old_daemon(path.clone());
    let client = IpcClient::new(path.clone());

    // a status gets the refusal and is not repeated
    match client.send(&Request::Status).await.unwrap() {
        Response::Error { code, .. } => assert_eq!(code, ErrorCode::ProtocolVersion),
        other => panic!("{other:?}"),
    }
    // a dry run of a kill must never be repeated as a real one: it would stop the old daemon
    let options = oxidepm_ipc::CallOptions {
        dry_run: true,
        ..Default::default()
    };
    match client.send_with(&Request::Kill, &options).await.unwrap() {
        Response::Error { code, .. } => assert_eq!(code, ErrorCode::ProtocolVersion),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(2, "status".to_string()), (2, "kill".to_string())],
        "no repeat with protocol 1"
    );
    // ping falls back and works
    assert!(client.ping().await.is_ok());
    assert_eq!(seen.lock().unwrap().len(), 4);
    // let the thread end: a real kill at protocol 1
    client.send(&Request::Kill).await.unwrap();
    handle.join().unwrap();
}

/// A streamed answer marks every part but the last with `more`; the flag is absent otherwise, so
/// an ordinary answer is the same line as before.
#[test]
fn response_envelope_more_is_absent_unless_set() {
    let plain = serde_json::to_value(ResponseEnvelope::new("r", Response::Pong)).unwrap();
    assert!(plain.get("more").is_none(), "{plain}");
    let part = ResponseEnvelope::part(
        "r",
        Response::InvokeOutput {
            stream: "stdout".into(),
            text: "hi".into(),
        },
    );
    let value = serde_json::to_value(&part).unwrap();
    assert_eq!(value["more"], true);
    assert_eq!(
        value["response"],
        serde_json::json!({ "type": "invoke_output", "stream": "stdout", "text": "hi" })
    );
    let back: ResponseEnvelope = serde_json::from_value(value).unwrap();
    assert!(back.more);
    let done = serde_json::to_value(ResponseEnvelope::new(
        "r",
        Response::InvokeDone {
            exit_code: 2,
            result: None,
        },
    ))
    .unwrap();
    assert_eq!(
        done["response"],
        serde_json::json!({ "type": "invoke_done", "exit_code": 2 })
    );
    let old: ResponseEnvelope =
        serde_json::from_str(r#"{"protocol":2,"request_id":"r","response":{"type":"pong"}}"#)
            .unwrap();
    assert!(!old.more, "a line from a daemon that has no streams");
}
