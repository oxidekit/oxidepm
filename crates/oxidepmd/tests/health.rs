//! Characterisation of health checks.

mod common;

use std::time::Duration;

use common::daemon::DaemonProc;
use common::*;
use oxidepm_core::{AppSpec, AppStatus, Runtime};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_without_health_check_is_running_immediately() {
    let h = Harness::new().await;
    let m = h.marker();
    h.start(h.cmd("health-none", &format!("exec sleep {m}")))
        .await
        .expect("start");

    let state = h.state("health-none").await;
    assert_eq!(state.status, AppStatus::Running);
    assert!(state.healthy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_probe_failure_keeps_child_alive_and_reports_unhealthy() {
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    h.start(h.cmd_with("health-bad", &format!("exec sleep {m}"), failing_probe()))
        .await
        .expect("start");

    // The legacy checker waits 5 s before its first probe.
    let state = h
        .wait_until_for("health-bad", "unhealthy", Duration::from_secs(20), |s| {
            h.is_unhealthy(s)
        })
        .await;
    let pid = state.pid.expect("unhealthy app keeps its pid");
    assert!(Harness::pid_alive(pid));
    assert_eq!(Harness::procs_with_cmdline(&needle), vec![pid]);
}

/// A probe that hangs is killed with everything it started when its timeout is up. (The first
/// sleep is the probe's: every later one is too, so the test follows one pid.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_probe_timeout_kills_probe_group() {
    let h = Harness::new().await;
    let m = h.marker();
    let m_probe = h.marker();
    let needle = format!("sleep {m_probe}");
    let probe = format!("cd '{}'; sleep {m_probe} & wait", h.dir().display());
    h.start(h.cmd_with(
        "probe-hang",
        &format!("exec sleep {m}"),
        json!({ "health_check": {
            "script": probe,
            "expected_status": [200],
            "interval_secs": 2,
            "timeout_secs": 1,
            "retries": 1
        } }),
    ))
    .await
    .expect("start");

    // the legacy checker waits 5 s before its first probe
    let mut first = None;
    h.eventually("the probe to start", Duration::from_secs(20), || {
        first = Harness::procs_with_cmdline(&needle).first().copied();
        first.is_some()
    })
    .await;
    let first = first.unwrap();
    // its timeout is one second
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !Harness::procs_with_cmdline(&needle).contains(&first),
        "the probe's child {first} outlived the probe timeout"
    );
    h.stop("probe-hang").await.expect("stop");
}

/// A script probe runs in the app's directory with the base environment plus the app's own
/// `env`; the daemon's other variables are not passed on.
#[test]
fn script_probe_runs_in_app_dir_without_daemon_environment() {
    let tmp = TempDir::new().expect("temp dir");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let out = workdir.join("probe.out");
    let daemon =
        DaemonProc::spawn_with_env(&tmp.path().join("home"), &[("OPM_TEST_SECRET", "hunter2")]);

    let mut spec = AppSpec::new(
        "probe-env".to_string(),
        Runtime::CMD,
        "sh".to_string(),
        workdir.clone(),
    );
    spec.args = vec!["-c".to_string(), "exec sleep 300".to_string()];
    spec.env.insert("FROM_SPEC".to_string(), "yes".to_string());
    let mut value = serde_json::to_value(&spec).unwrap();
    value["health_check"] = json!({
        "script": format!("{{ pwd; env; }} > '{o}.tmp'; mv '{o}.tmp' '{o}'", o = out.display()),
        "expected_status": [200],
        "interval_secs": 2,
        "timeout_secs": 1,
        "retries": 1
    });
    let started = daemon.rpc(json!({ "type": "start", "spec": value }));
    assert_eq!(started["type"], "started", "{started}");

    // the legacy checker waits 5 s before its first probe
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let text = loop {
        if let Ok(text) = std::fs::read_to_string(&out) {
            break text;
        }
        assert!(std::time::Instant::now() < deadline, "the probe never ran");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut lines = text.lines();
    assert_eq!(
        std::fs::canonicalize(lines.next().unwrap()).unwrap(),
        std::fs::canonicalize(&workdir).unwrap(),
        "the probe runs in the app's directory"
    );
    let env: Vec<&str> = lines.collect();
    assert!(
        !env.iter().any(|l| l.starts_with("OPM_TEST_SECRET")),
        "the daemon's variable leaked: {env:?}"
    );
    assert!(env.contains(&"FROM_SPEC=yes"), "{env:?}");
    assert!(env.contains(&"OPM_APP_NAME=probe-env"), "{env:?}");
}

/// A local HTTP server: `/` answers 301 to `/ok`, `/ok` answers 200. Returns its address; the
/// server dies with the test's runtime.
async fn redirecting_server() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let reply = if request.starts_with("GET /ok ") {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                } else {
                    "HTTP/1.1 301 Moved Permanently\r\nlocation: /ok\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                };
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_probe_does_not_follow_redirect() {
    let h = Harness::new().await;
    let m = h.marker();
    let addr = redirecting_server().await;
    h.start(h.cmd_with(
        "http-redirect",
        &format!("exec sleep {m}"),
        json!({ "health_check": {
            "http_url": format!("http://{addr}/"),
            "expected_status": [200],
            "interval_secs": 2,
            "timeout_secs": 1,
            "retries": 1
        } }),
    ))
    .await
    .expect("start");

    // followed, the 301 would end at /ok and the app would stay healthy forever
    h.wait_until_for("http-redirect", "unhealthy", Duration::from_secs(20), |s| {
        h.is_unhealthy(s)
    })
    .await;
    h.stop("http-redirect").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_probe_closed_port_marks_unhealthy() {
    let h = Harness::new().await;
    let m = h.marker();
    // bound but never listening: connections are refused and the port stays ours
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let closed = socket.local_addr().unwrap();
    h.start(h.cmd_with(
        "tcp-closed",
        &format!("exec sleep {m}"),
        json!({ "health_check": {
            "tcp": closed.to_string(),
            "expected_status": [200],
            "interval_secs": 2,
            "timeout_secs": 1,
            "retries": 1
        } }),
    ))
    .await
    .expect("start");

    let state = h
        .wait_until_for("tcp-closed", "unhealthy", Duration::from_secs(20), |s| {
            h.is_unhealthy(s)
        })
        .await;
    let pid = state.pid.expect("an unhealthy app keeps its pid");
    assert!(
        Harness::pid_alive(pid),
        "the child is alive, only the probe fails"
    );
    h.stop("tcp-closed").await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_probe_open_port_stays_healthy() {
    let h = Harness::new().await;
    let m = h.marker();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let open = listener.local_addr().unwrap();
    h.start(h.cmd_with(
        "tcp-open",
        &format!("exec sleep {m}"),
        json!({ "health_check": {
            "tcp": open.to_string(),
            "expected_status": [200],
            "interval_secs": 2,
            "timeout_secs": 1,
            "retries": 1
        } }),
    ))
    .await
    .expect("start");

    // past the first probe (5 s after the start plus one interval)
    tokio::time::sleep(Duration::from_secs(9)).await;
    let state = h.state("tcp-open").await;
    assert!(state.healthy && !h.is_unhealthy(&state), "{state:?}");
    assert!(state.last_health_check.is_some(), "a probe did run");
    h.stop("tcp-open").await.expect("stop");
    drop(listener);
}
