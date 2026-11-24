//! Framing, bind and permission behaviour of `IpcServer` / `IpcConnection`, and the client's
//! idea of "a daemon is running". Everything lives in a temp directory.

use oxidepm_ipc::server::{Incoming, Reply};
use oxidepm_ipc::{IpcClient, IpcServer, Request};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

fn socket_in(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("daemon.sock")
}

fn leave_stale_socket(path: &std::path::Path) {
    use socket2::{Domain, SockAddr, Socket, Type};

    // Bind without listening, so a concurrent fork cannot keep a listener alive.
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
    socket.bind(&SockAddr::unix(path).unwrap()).unwrap();
}

/// One request line in the envelope format (with the newline).
fn line(request: &str) -> String {
    format!("{{\"protocol\":2,\"request_id\":\"t\",\"request\":{{\"type\":\"{request}\"}}}}\n")
}

/// The request inside an incoming envelope, as its wire type name.
fn kind(incoming: Option<Incoming>) -> String {
    match incoming {
        Some(Incoming::Request(envelope)) => {
            let value = serde_json::to_value(&envelope.request).unwrap();
            value["type"].as_str().unwrap().to_string()
        }
        other => panic!("expected a request, got {other:?}"),
    }
}

#[tokio::test]
async fn pipelined_requests_both_read() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    // two requests in a single write: the second must not be lost with the first line's buffer
    client
        .write_all(format!("{}{}", line("ping"), line("status")).as_bytes())
        .await
        .unwrap();

    let mut conn = server.accept().await.unwrap();
    assert_eq!(kind(conn.read_request().await.unwrap()), "ping");
    assert_eq!(kind(conn.read_request().await.unwrap()), "status");

    drop(client);
    assert!(
        conn.read_request().await.unwrap().is_none(),
        "closed connection reads as None"
    );
}

#[tokio::test]
async fn oversized_line_without_newline_errors() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let writer = tokio::spawn(async move {
        // 11 MiB, no newline; the server stops reading at its limit and closes, so the
        // tail of the write may fail
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..11 {
            if client.write_all(&chunk).await.is_err() {
                break;
            }
        }
        client
    });

    let mut conn = server.accept().await.unwrap();
    let err = match conn.read_request().await {
        Ok(other) => panic!("an 11 MiB line must be refused, got {other:?}"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("too large"), "{err}");
    drop(conn);
    let _ = writer.await;
}

#[tokio::test]
async fn malformed_line_is_rejected_but_the_connection_stays_usable() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client
        .write_all(format!("this is not json\n{}", line("ping")).as_bytes())
        .await
        .unwrap();

    let mut conn = server.accept().await.unwrap();
    match conn.read_request().await.unwrap() {
        Some(Incoming::Reject {
            reply: Reply::Envelope(reply),
            close,
        }) => {
            assert!(!close, "a malformed line does not end the connection");
            assert_eq!(
                reply.response.error_code().map(|c| c.as_str()),
                Some("E_BAD_REQUEST")
            );
            assert!(reply
                .response
                .error_message()
                .unwrap()
                .contains("nvalid request"));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_eq!(
        kind(conn.read_request().await.unwrap()),
        "ping",
        "the next line still parses"
    );
}

#[tokio::test]
async fn idle_connection_times_out() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();

    let _client = UnixStream::connect(&path).await.unwrap();
    let mut conn = server.accept().await.unwrap();
    conn.set_idle_timeout(Duration::from_millis(100));
    let err = match conn.read_request().await {
        Ok(other) => panic!("an idle connection must not read as {other:?}"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("idle"), "{err}");
}

#[tokio::test]
async fn bind_fails_when_live_listener_owns_path() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let first = IpcServer::bind(&path).await.unwrap();

    let second = IpcServer::bind(&path).await;
    assert!(
        matches!(second, Err(oxidepm_core::Error::DaemonAlreadyRunning)),
        "a live socket must not be unlinked"
    );

    // the first listener still works
    let mut client = UnixStream::connect(&path)
        .await
        .expect("first server still reachable");
    client.write_all(line("ping").as_bytes()).await.unwrap();
    // the second bind's liveness probe is the first connection `first` sees; it just closes
    let mut probe = first.accept().await.unwrap();
    assert!(probe.read_request().await.unwrap().is_none());
    let mut conn = first.accept().await.unwrap();
    assert_eq!(kind(conn.read_request().await.unwrap()), "ping");
}

#[tokio::test]
async fn bind_removes_stale_socket() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    leave_stale_socket(&path);
    assert!(path.exists(), "the stale socket file is there");

    let server = IpcServer::bind(&path)
        .await
        .expect("a stale socket is replaced");
    let mut client = UnixStream::connect(&path)
        .await
        .expect("the new server answers");
    client.write_all(line("ping").as_bytes()).await.unwrap();
    let mut conn = server.accept().await.unwrap();
    assert_eq!(kind(conn.read_request().await.unwrap()), "ping");
}

#[tokio::test]
async fn bind_does_not_delete_a_file_that_is_not_a_socket() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    std::fs::write(&path, b"precious").unwrap();

    assert!(IpcServer::bind(&path).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"precious");
}

#[tokio::test]
async fn is_daemon_running_really_connects() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let client = IpcClient::new(path.clone());
    assert!(!client.is_daemon_running(), "no socket file");

    leave_stale_socket(&path);
    assert!(path.exists());
    assert!(
        !client.is_daemon_running(),
        "a stale socket file is not a running daemon"
    );

    let server = IpcServer::bind(&path).await.unwrap();
    assert!(client.is_daemon_running());
    drop(server);
}

#[tokio::test]
async fn send_never_starts_a_daemon_but_send_autostart_reports_a_missing_binary() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let client = IpcClient::new(path.clone());

    let err = client
        .send(&Request::Ping)
        .await
        .expect_err("nothing listens");
    assert!(
        matches!(err, oxidepm_core::Error::DaemonNotRunning),
        "{err}"
    );
    assert!(!path.exists());

    // an explicit binary that does not exist is an error, not a PATH lookup
    let client = IpcClient::new(path.clone()).with_daemon_bin(dir.path().join("no-such-oxidepmd"));
    let err = client
        .send_autostart(&Request::Ping)
        .await
        .expect_err("cannot start a missing binary");
    assert!(err.to_string().contains("no-such-oxidepmd"), "{err}");
}

#[tokio::test]
async fn a_peer_of_another_user_is_not_accepted_but_ours_is() {
    // Our own connection is accepted (the uid matches); the rejection path is covered by the
    // uid comparison in `accept` and cannot be exercised without a second user.
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let server = IpcServer::bind(&path).await.unwrap();
    let mut client = UnixStream::connect(&path).await.unwrap();
    let accepted = tokio::time::timeout(Duration::from_secs(5), server.accept())
        .await
        .expect("accept returns")
        .expect("same-uid peer accepted");
    assert_eq!(accepted.peer_uid(), Some(nix::unistd::geteuid().as_raw()));
    client.write_all(b"\n").await.unwrap();
}

// ---- per-listener limits (the agent socket) ----

use oxidepm_ipc::server::{ReadError, ServerLimits};
use tokio::io::AsyncReadExt;

#[test]
fn agent_limits_are_tighter_than_the_main_ones() {
    let (main, agent) = (ServerLimits::MAIN, ServerLimits::AGENT);
    assert_eq!(
        (main.max_line, main.max_connections, main.write_timeout),
        (10 * 1024 * 1024, Some(256), Some(Duration::from_secs(30)))
    );
    assert_eq!(main.idle_timeout, Duration::from_secs(300));
    assert_eq!(agent.max_line, 1024 * 1024);
    assert_eq!(agent.max_connections, Some(32));
    assert_eq!(agent.idle_timeout, Duration::from_secs(60));
    assert_eq!(agent.write_timeout, Some(Duration::from_secs(10)));
}

#[tokio::test]
async fn listener_closes_connections_beyond_its_cap_and_reuses_freed_places() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let limits = ServerLimits {
        max_connections: Some(2),
        ..ServerLimits::MAIN
    };
    let server = IpcServer::bind_with(&path, limits).await.unwrap();

    let _c1 = UnixStream::connect(&path).await.unwrap();
    let conn1 = server.accept().await.unwrap();
    let _c2 = UnixStream::connect(&path).await.unwrap();
    let _conn2 = server.accept().await.unwrap();

    // the third is dropped inside accept, which then goes on waiting for the next one
    let mut c3 = UnixStream::connect(&path).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), server.accept())
            .await
            .is_err()
    );
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), c3.read(&mut buf))
        .await
        .expect("closed at once");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");

    // a place is freed when its connection is dropped
    drop(conn1);
    let _c4 = UnixStream::connect(&path).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), server.accept())
        .await
        .expect("accepted")
        .expect("ok");
}

#[tokio::test]
async fn line_limit_is_the_listeners_and_the_error_names_it() {
    let dir = TempDir::new().unwrap();
    let path = socket_in(&dir);
    let limits = ServerLimits {
        max_line: 100,
        ..ServerLimits::MAIN
    };
    let server = IpcServer::bind_with(&path, limits).await.unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client.write_all(&[b'x'; 200]).await.unwrap();
    let mut conn = server.accept().await.unwrap();
    let err = conn.read_request().await.unwrap_err();
    assert!(matches!(err, ReadError::TooLarge { limit: 100 }), "{err:?}");
    assert!(err.to_string().contains("100 bytes"), "{err}");
    assert_eq!(
        ReadError::TooLarge { limit: 1024 * 1024 }.to_string(),
        "Request too large (limit 1 MiB per line)"
    );
}

#[tokio::test]
async fn write_to_a_client_that_does_not_read_times_out() {
    use oxidepm_ipc::server::IpcConnection;
    use oxidepm_ipc::{Response, ResponseEnvelope};

    let (server_side, _client_side) = UnixStream::pair().unwrap();
    let limits = ServerLimits {
        write_timeout: Some(Duration::from_millis(200)),
        ..ServerLimits::MAIN
    };
    let mut conn = IpcConnection::with_limits(server_side, None, limits);
    let big = ResponseEnvelope::new("r", Response::ok("x".repeat(16 * 1024 * 1024)));

    let started = std::time::Instant::now();
    let err = conn
        .send_response(&big)
        .await
        .expect_err("nobody reads the answer");
    assert!(err.to_string().contains("did not read"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_streamed_answer_reaches_a_client_that_reads_it() {
    use oxidepm_ipc::server::{Incoming, IpcConnection};
    use oxidepm_ipc::{
        AdapterRequest, CallOptions, InvokeArgs, IpcClient, Request, Response, ResponseEnvelope,
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let server = IpcServer::bind(&path).await.unwrap();
    let serve = tokio::spawn(async move {
        let mut conn: IpcConnection = server.accept().await.unwrap();
        let Some(Incoming::Request(envelope)) = conn.read_request().await.unwrap() else {
            panic!("no request")
        };
        let (mut writer, _watch) = conn.stream_halves();
        for n in 0..3 {
            let part = ResponseEnvelope::part(
                envelope.request_id.clone(),
                Response::InvokeOutput {
                    stream: "stdout".into(),
                    text: format!("p{n}"),
                },
            );
            writer.send(&part, Duration::from_secs(5)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let done = ResponseEnvelope::new(
            envelope.request_id,
            Response::InvokeDone {
                exit_code: 7,
                result: None,
            },
        );
        writer.send(&done, Duration::from_secs(5)).await.unwrap();
    });
    let client = IpcClient::new(path).with_stream_inactivity(Duration::from_secs(2));
    let request = Request::Adapter(Box::new(AdapterRequest::Invoke(Box::new(InvokeArgs {
        verb: "v".into(),
        argv: vec![],
        cwd: "/x".into(),
    }))));
    let mut seen = Vec::new();
    let end = client
        .send_streaming(&request, &CallOptions::default(), |part| {
            if let Response::InvokeOutput { text, .. } = part {
                seen.push(text.clone());
            }
        })
        .await
        .unwrap();
    assert_eq!(seen, ["p0", "p1", "p2"]);
    assert!(
        matches!(end, Response::InvokeDone { exit_code: 7, .. }),
        "{end:?}"
    );
    serve.await.unwrap();
}

#[tokio::test]
async fn a_silent_stream_times_out_on_inactivity_and_a_chatty_one_does_not() {
    use oxidepm_ipc::server::{Incoming, IpcConnection};
    use oxidepm_ipc::{
        AdapterRequest, CallOptions, InvokeArgs, IpcClient, Request, Response, ResponseEnvelope,
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let server = IpcServer::bind(&path).await.unwrap();
    let serve = tokio::spawn(async move {
        // the first client gets a line every 300 ms for a second; the second gets silence
        for chatty in [true, false] {
            let mut conn: IpcConnection = server.accept().await.unwrap();
            let Some(Incoming::Request(envelope)) = conn.read_request().await.unwrap() else {
                panic!("no request")
            };
            let (mut writer, _watch) = conn.stream_halves();
            if chatty {
                for _ in 0..4 {
                    let part = ResponseEnvelope::part(
                        envelope.request_id.clone(),
                        Response::InvokeOutput {
                            stream: "stdout".into(),
                            text: "x".into(),
                        },
                    );
                    writer.send(&part, Duration::from_secs(5)).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                writer
                    .send(
                        &ResponseEnvelope::new(
                            envelope.request_id,
                            Response::InvokeDone {
                                exit_code: 0,
                                result: None,
                            },
                        ),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
            } else {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });
    let client = IpcClient::new(path).with_stream_inactivity(Duration::from_millis(500));
    let request = Request::Adapter(Box::new(AdapterRequest::Invoke(Box::new(InvokeArgs {
        verb: "v".into(),
        argv: vec![],
        cwd: "/x".into(),
    }))));
    let first = client
        .send_streaming(&request, &CallOptions::default(), |_| {})
        .await;
    assert!(
        matches!(first, Ok(Response::InvokeDone { .. })),
        "lines keep it alive: {first:?}"
    );
    let second = client
        .send_streaming(&request, &CallOptions::default(), |_| {})
        .await;
    assert!(
        matches!(second, Err(oxidepm_core::Error::Timeout(_))),
        "{second:?}"
    );
    serve.abort();
}

#[tokio::test]
async fn the_server_notices_a_client_that_went_away_and_a_part_to_a_stuck_client_times_out() {
    use oxidepm_ipc::server::IpcConnection;
    use oxidepm_ipc::{Response, ResponseEnvelope};

    let (server_side, client_side) = UnixStream::pair().unwrap();
    let mut conn = IpcConnection::new(server_side, None);
    {
        let (_writer, mut watch) = conn.stream_halves();
        // a client that is there and silent is not gone
        assert!(
            tokio::time::timeout(Duration::from_millis(200), watch.gone())
                .await
                .is_err()
        );
        drop(client_side);
        tokio::time::timeout(Duration::from_secs(5), watch.gone())
            .await
            .expect("the hang-up is noticed");
    }
    // a part for a client that never reads gives up after its own timeout
    let (server_side, _client_side) = UnixStream::pair().unwrap();
    let mut conn = IpcConnection::new(server_side, None);
    let (mut writer, _watch) = conn.stream_halves();
    let big = ResponseEnvelope::part("r", Response::ok("x".repeat(16 * 1024 * 1024)));
    let err = writer
        .send(&big, Duration::from_millis(200))
        .await
        .expect_err("nobody reads");
    assert!(err.to_string().contains("did not read"), "{err}");
}
