//! OxidePM Health Check System
//!
//! Provides HTTP endpoint and script-based health checks for processes.

use chrono::{DateTime, Utc};
use oxidepm_core::HealthCheck;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
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

/// Health checker that performs HTTP and script-based health checks
pub struct HealthChecker {
    client: reqwest::Client,
}

impl HealthChecker {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self { client }
    }

    /// Perform a health check based on configuration
    pub async fn check(&self, config: &HealthCheck) -> HealthCheckResult {
        let start = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(config.timeout_secs);

        // Try HTTP check first if configured
        if let Some(url) = &config.http_url {
            return self.check_http(url, &config.expected_status, timeout_duration).await;
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

        let result = timeout(timeout_duration, self.client.get(url).send()).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                if expected_status.contains(&status) || (expected_status.is_empty() && status == 200) {
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

    /// Perform script-based health check
    async fn check_script(&self, script: &str, timeout_duration: Duration) -> HealthCheckResult {
        let start = std::time::Instant::now();

        let result = timeout(
            timeout_duration,
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(status)) => {
                if status.success() {
                    debug!("Health check script passed");
                    HealthCheckResult::healthy(duration_ms)
                } else {
                    let code = status.code().unwrap_or(-1);
                    warn!("Health check script failed with code: {}", code);
                    HealthCheckResult::unhealthy(duration_ms, format!("Exit code: {}", code))
                }
            }
            Ok(Err(e)) => {
                warn!("Health check script error: {}", e);
                HealthCheckResult::unhealthy(duration_ms, format!("Script error: {}", e))
            }
            Err(_) => {
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
