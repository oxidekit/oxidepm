#![forbid(unsafe_code)]

//! OxidePM Health Check System
//!
//! Provides HTTP endpoint, TCP connect and script-based health checks for processes.

mod http;

use chrono::{DateTime, Utc};
use oxidepm_core::HealthCheck;
use oxidepm_runtime::{run_script, script_base_env, ChildEnv, ScriptOutcome, ScriptSpec};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Health check result
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    pub healthy: bool,
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub message: Option<String>,
}

impl HealthCheckResult {
    pub fn healthy(duration_ms: u64) -> Self {
        Self {
            healthy: true,
            timestamp: Utc::now(),
            duration_ms,
            message: None,
        }
    }

    pub fn unhealthy(duration_ms: u64, message: impl Into<String>) -> Self {
        Self {
            healthy: false,
            timestamp: Utc::now(),
            duration_ms,
            message: Some(message.into()),
        }
    }
}

/// Where and with what environment a script probe runs.
#[derive(Debug, Clone)]
pub struct ScriptContext {
    /// Working directory (the app's); `None` is the daemon's own.
    pub cwd: Option<PathBuf>,
    /// The environment of the script.
    pub env: ChildEnv,
}

impl Default for ScriptContext {
    /// The base environment only (never the daemon's whole environment), the daemon's directory.
    fn default() -> Self {
        Self {
            cwd: None,
            env: script_base_env(),
        }
    }
}

/// Health checker that performs HTTP, TCP and script-based health checks
pub struct HealthChecker {
    script: ScriptContext,
}

impl HealthChecker {
    pub fn new() -> Self {
        // A redirect is an answer, not a pass: following it would let a 301 to some page that
        // returns 200 count as healthy. The probe's client (`http`) has no way to follow one.
        Self {
            script: ScriptContext::default(),
        }
    }

    /// Run script probes in this directory and environment.
    pub fn with_script_context(mut self, script: ScriptContext) -> Self {
        self.script = script;
        self
    }

    /// Perform a health check based on configuration
    pub async fn check(&self, config: &HealthCheck) -> HealthCheckResult {
        let start = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(config.timeout_secs);

        // Try HTTP check first if configured
        if let Some(url) = &config.http_url {
            return self
                .check_http(url, &config.expected_status, timeout_duration)
                .await;
        }

        if let Some(addr) = &config.tcp {
            return self.check_tcp(addr, timeout_duration).await;
        }

        // Try script check if configured
        if let Some(script) = &config.script {
            return self.check_script(script, timeout_duration).await;
        }

        // No check configured, assume healthy
        HealthCheckResult::healthy(start.elapsed().as_millis() as u64)
    }

    /// Perform HTTP health check
    async fn check_http(
        &self,
        url: &str,
        expected_status: &[u16],
        timeout_duration: Duration,
    ) -> HealthCheckResult {
        let start = std::time::Instant::now();

        let result = timeout(timeout_duration, http::get_status(url)).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(status)) => {
                if expected_status.contains(&status)
                    || (expected_status.is_empty() && status == 200)
                {
                    debug!("Health check passed: {} returned {}", url, status);
                    HealthCheckResult::healthy(duration_ms)
                } else {
                    warn!("Health check failed: {} returned {}", url, status);
                    HealthCheckResult::unhealthy(
                        duration_ms,
                        format!("Unexpected status: {}", status),
                    )
                }
            }
            Ok(Err(e)) => {
                warn!("Health check failed: {} - {}", url, e);
                HealthCheckResult::unhealthy(duration_ms, format!("Request failed: {}", e))
            }
            Err(_) => {
                warn!("Health check timed out: {}", url);
                HealthCheckResult::unhealthy(duration_ms, "Timeout")
            }
        }
    }

    /// Perform TCP health check: the address must accept a connection within the timeout.
    async fn check_tcp(&self, addr: &str, timeout_duration: Duration) -> HealthCheckResult {
        let start = std::time::Instant::now();

        let result = timeout(timeout_duration, TcpStream::connect(addr)).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(_stream)) => {
                debug!("Health check passed: {} accepts connections", addr);
                HealthCheckResult::healthy(duration_ms)
            }
            Ok(Err(e)) => {
                warn!("Health check failed: {} - {}", addr, e);
                HealthCheckResult::unhealthy(duration_ms, format!("Connection failed: {}", e))
            }
            Err(_) => {
                warn!("Health check timed out: {}", addr);
                HealthCheckResult::unhealthy(duration_ms, "Timeout")
            }
        }
    }

    /// Perform script-based health check. The script runs in its own process group, which is
    /// killed (with everything the script started) when the timeout is up.
    async fn check_script(&self, script: &str, timeout_duration: Duration) -> HealthCheckResult {
        let start = std::time::Instant::now();

        let outcome = run_script(ScriptSpec {
            script: script.to_string(),
            cwd: self.script.cwd.clone(),
            env: self.script.env.clone(),
            timeout: timeout_duration,
            output: None,
        })
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match outcome {
            ScriptOutcome::Exited(status) => {
                if status.success() {
                    debug!("Health check script passed");
                    HealthCheckResult::healthy(duration_ms)
                } else {
                    let code = status.code().unwrap_or(-1);
                    warn!("Health check script failed with code: {}", code);
                    HealthCheckResult::unhealthy(duration_ms, format!("Exit code: {}", code))
                }
            }
            ScriptOutcome::SpawnFailed(e) => {
                warn!("Health check script error: {}", e);
                HealthCheckResult::unhealthy(duration_ms, format!("Script error: {}", e))
            }
            ScriptOutcome::TimedOut => {
                warn!("Health check script timed out");
                HealthCheckResult::unhealthy(duration_ms, "Timeout")
            }
        }
    }
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// Health monitor that runs periodic health checks for a process
pub struct HealthMonitor {
    checker: HealthChecker,
    config: HealthCheck,
    consecutive_failures: u32,
}

impl HealthMonitor {
    pub fn new(config: HealthCheck) -> Self {
        Self {
            checker: HealthChecker::new(),
            config,
            consecutive_failures: 0,
        }
    }

    /// Run script probes in this directory and environment.
    pub fn with_script_context(mut self, script: ScriptContext) -> Self {
        self.checker = self.checker.with_script_context(script);
        self
    }

    /// Perform a single health check and update failure count
    pub async fn check(&mut self) -> HealthCheckResult {
        let result = self.checker.check(&self.config).await;

        if result.healthy {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
        }

        result
    }

    /// Check if the process should be considered unhealthy
    pub fn is_unhealthy(&self) -> bool {
        self.consecutive_failures >= self.config.retries
    }

    /// Get the check interval
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.interval_secs)
    }

    /// Reset failure counter
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_checker_no_config() {
        let checker = HealthChecker::new();
        let config = HealthCheck::default();
        let result = checker.check(&config).await;
        assert!(result.healthy);
    }

    /// A local HTTP server: `/` answers 301 to `/ok`, `/ok` answers 200. Returns its address.
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

    #[tokio::test]
    async fn http_probe_does_not_follow_redirect() {
        let addr = redirecting_server().await;
        let checker = HealthChecker::new();

        let mut config = HealthCheck::http(format!("http://{addr}/"));
        config.expected_status = vec![200];
        let result = checker.check(&config).await;
        assert!(
            !result.healthy,
            "a 301 is not a 200 even when it points at one"
        );
        assert_eq!(result.message.as_deref(), Some("Unexpected status: 301"));

        // the target itself is fine, and a redirect is healthy when it is what is expected
        config.http_url = Some(format!("http://{addr}/ok"));
        assert!(checker.check(&config).await.healthy);
        config.http_url = Some(format!("http://{addr}/"));
        config.expected_status = vec![301];
        assert!(checker.check(&config).await.healthy);
    }

    /// A server that answers every connection with `reply` (as raw bytes) after reading the request head.
    async fn canned_server(
        reply: Vec<u8>,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let reply = reply.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let _ = seen.send(String::from_utf8_lossy(&buf[..n]).to_string());
                    let _ = socket.write_all(&reply).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (addr, requests)
    }

    #[tokio::test]
    async fn http_probe_sends_a_plain_get_with_host_and_credentials_of_the_url() {
        let (addr, mut requests) =
            canned_server(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec()).await;
        let checker = HealthChecker::new();
        let mut config = HealthCheck::http(format!("http://user:pw@{addr}/h?x=1"));
        config.expected_status = vec![200];
        assert!(checker.check(&config).await.healthy);
        let request = requests.recv().await.unwrap();
        assert!(request.starts_with("GET /h?x=1 HTTP/1.1\r\n"), "{request}");
        assert!(request.contains(&format!("Host: {addr}\r\n")), "{request}");
        assert!(
            request.contains("Authorization: Basic dXNlcjpwdw==\r\n"),
            "{request}"
        );
        assert!(request.contains("Connection: close\r\n"), "{request}");
    }

    #[tokio::test]
    async fn http_probe_default_expects_200_only() {
        let (addr, _requests) = canned_server(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()).await;
        let checker = HealthChecker::new();
        let mut config = HealthCheck::http(format!("http://{addr}/"));
        config.expected_status = Vec::new();
        let result = checker.check(&config).await;
        assert!(!result.healthy);
        assert_eq!(result.message.as_deref(), Some("Unexpected status: 204"));
    }

    #[tokio::test]
    async fn http_probe_answer_that_is_not_http_is_unhealthy() {
        for reply in [
            b"SSH-2.0-OpenSSH_9.0\r\n".to_vec(),
            Vec::new(),
            vec![0xff; 100],
            b"HTTP/1.1 2x0 OK\r\n".to_vec(),
        ] {
            let (addr, _requests) = canned_server(reply.clone()).await;
            let result = HealthChecker::new()
                .check(&HealthCheck::http(format!("http://{addr}/")))
                .await;
            assert!(!result.healthy, "{reply:?}");
            assert!(result.message.unwrap().starts_with("Request failed"));
        }
    }

    #[tokio::test]
    async fn http_probe_endless_header_is_cut_off() {
        // no newline at all: the probe reads at most a few KiB and gives up instead of growing
        let (addr, _requests) = canned_server(vec![b'a'; 1024 * 1024]).await;
        let result = HealthChecker::new()
            .check(&HealthCheck::http(format!("http://{addr}/")))
            .await;
        assert!(!result.healthy);
    }

    #[tokio::test]
    async fn http_probe_that_is_never_answered_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _keep = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let mut config = HealthCheck::http(format!("http://{addr}/"));
        config.timeout_secs = 1;
        let started = std::time::Instant::now();
        let result = HealthChecker::new().check(&config).await;
        assert!(!result.healthy);
        assert_eq!(result.message.as_deref(), Some("Timeout"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn https_probe_of_a_port_that_does_not_speak_tls_is_unhealthy() {
        let (addr, _requests) = canned_server(b"HTTP/1.1 200 OK\r\n\r\n".to_vec()).await;
        let result = HealthChecker::new()
            .check(&HealthCheck::http(format!("https://{addr}/")))
            .await;
        assert!(
            !result.healthy,
            "plain HTTP behind an https URL is not a pass"
        );
    }

    #[tokio::test]
    async fn http_probe_closed_port_is_unhealthy() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut config = HealthCheck::http(format!("http://{}/", socket.local_addr().unwrap()));
        config.timeout_secs = 1;
        let result = timeout(Duration::from_secs(5), HealthChecker::new().check(&config))
            .await
            .expect("the probe must complete within its timeout");
        assert!(!result.healthy);
        let message = result.message.unwrap();
        assert!(message.contains("connection failed"), "{message}");
    }

    #[tokio::test]
    async fn tcp_probe_closed_port_marks_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let checker = HealthChecker::new();
        let open = HealthCheck::tcp(listener.local_addr().unwrap().to_string());
        assert!(
            checker.check(&open).await.healthy,
            "an open port is healthy"
        );

        // Keep the port reserved without listening so no other test can use it.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut closed = HealthCheck::tcp(socket.local_addr().unwrap().to_string());
        closed.timeout_secs = 1;
        let result = timeout(Duration::from_secs(5), checker.check(&closed))
            .await
            .expect("the probe must complete within its timeout");
        assert!(!result.healthy);
        let message = result.message.unwrap();
        assert!(message.starts_with("Connection failed"), "{message}");
    }

    #[tokio::test]
    async fn test_health_check_script_success() {
        let checker = HealthChecker::new();
        let config = HealthCheck::script("exit 0");
        let result = checker.check(&config).await;
        assert!(result.healthy);
    }

    #[tokio::test]
    async fn test_health_check_script_failure() {
        let checker = HealthChecker::new();
        let config = HealthCheck::script("exit 1");
        let result = checker.check(&config).await;
        assert!(!result.healthy);
    }

    #[test]
    fn test_health_monitor_failure_counting() {
        let config = HealthCheck {
            retries: 3,
            ..Default::default()
        };
        let mut monitor = HealthMonitor::new(config);

        assert!(!monitor.is_unhealthy());
        monitor.consecutive_failures = 2;
        assert!(!monitor.is_unhealthy());
        monitor.consecutive_failures = 3;
        assert!(monitor.is_unhealthy());
    }
}
