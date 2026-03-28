//! Cosmos SDK node health checking
//!
//! Polls the node's RPC endpoint for sync status, block heights,
//! stale detection, and validator signing info (missed blocks, jailed status).
//! Also checks that expected ports are listening.

use chrono::{DateTime, Utc};
use oxidepm_core::{CosmosConfig, CosmosNodeMode};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, warn};

/// Cosmos node sync/health status
#[derive(Debug, Clone)]
pub struct CosmosNodeStatus {
    /// Whether the node is currently catching up
    pub catching_up: bool,
    /// Latest block height
    pub latest_block_height: u64,
    /// Latest block time
    pub latest_block_time: Option<DateTime<Utc>>,
    /// Seconds since last block (stale detection)
    pub seconds_since_block: Option<u64>,
    /// Node ID from /status
    pub node_id: Option<String>,
    /// Network name from /status
    pub network: Option<String>,
}

/// Validator signing info from slashing module
#[derive(Debug, Clone)]
pub struct ValidatorSigningInfo {
    /// Number of missed blocks in the current window
    pub missed_blocks: u64,
    /// Signing window size
    pub window_size: u64,
    /// Whether the validator is jailed
    pub jailed: bool,
    /// Jail threshold (number of missed blocks that triggers jailing)
    pub jail_threshold: u64,
}

/// Combined cosmos health check result
#[derive(Debug, Clone)]
pub struct CosmosHealthResult {
    pub reachable: bool,
    pub node_status: Option<CosmosNodeStatus>,
    pub signing_info: Option<ValidatorSigningInfo>,
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub message: Option<String>,
}

impl CosmosHealthResult {
    pub fn unreachable(duration_ms: u64, msg: impl Into<String>) -> Self {
        Self {
            reachable: false,
            node_status: None,
            signing_info: None,
            timestamp: Utc::now(),
            duration_ms,
            message: Some(msg.into()),
        }
    }

    /// Check if the node is stale (no new blocks in threshold seconds)
    pub fn is_stale(&self, threshold_secs: u64) -> bool {
        self.node_status
            .as_ref()
            .and_then(|s| s.seconds_since_block)
            .map(|secs| secs > threshold_secs)
            .unwrap_or(false)
    }
}

/// Health checker for Cosmos SDK nodes
pub struct CosmosHealthChecker {
    client: Client,
    rpc_endpoint: String,
}

impl CosmosHealthChecker {
    pub fn new(rpc_endpoint: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            client,
            rpc_endpoint: rpc_endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// Full health check: /status + optional signing info
    pub async fn check(
        &self,
        config: &CosmosConfig,
    ) -> CosmosHealthResult {
        let start = std::time::Instant::now();

        // Poll /status
        let node_status = match self.poll_status().await {
            Ok(status) => Some(status),
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                warn!("Cosmos /status check failed: {}", e);
                return CosmosHealthResult::unreachable(
                    duration_ms,
                    format!("RPC unreachable: {}", e),
                );
            }
        };

        // Poll signing info for validators
        let signing_info = if config.node_mode == CosmosNodeMode::Validator {
            match self.poll_signing_info().await {
                Ok(info) => Some(info),
                Err(e) => {
                    debug!("Could not fetch signing info: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        CosmosHealthResult {
            reachable: true,
            node_status,
            signing_info,
            timestamp: Utc::now(),
            duration_ms,
            message: None,
        }
    }

    /// Poll the /status RPC endpoint
    async fn poll_status(&self) -> Result<CosmosNodeStatus, String> {
        let url = format!("{}/status", self.rpc_endpoint);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let body: RpcStatusResponse = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse failed: {}", e))?;

        let sync_info = &body.result.sync_info;

        let height: u64 = sync_info
            .latest_block_height
            .parse()
            .unwrap_or(0);

        let block_time = sync_info
            .latest_block_time
            .parse::<DateTime<Utc>>()
            .ok();

        let seconds_since_block = block_time
            .map(|t| (Utc::now() - t).num_seconds().max(0) as u64);

        Ok(CosmosNodeStatus {
            catching_up: sync_info.catching_up,
            latest_block_height: height,
            latest_block_time: block_time,
            seconds_since_block,
            node_id: Some(body.result.node_info.id),
            network: Some(body.result.node_info.network),
        })
    }

    /// Poll the slashing signing info endpoint
    async fn poll_signing_info(&self) -> Result<ValidatorSigningInfo, String> {
        // Use the LCD/REST endpoint for signing infos
        // This requires knowing the validator consensus address — we query all and take first
        let url = format!(
            "{}/cosmos/slashing/v1beta1/signing_infos",
            self.rpc_endpoint.replace(":26657", ":1317")
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("signing_infos request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let body: SigningInfosResponse = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse failed: {}", e))?;

        // Take the first signing info (typically the local validator)
        let info = body
            .info
            .into_iter()
            .next()
            .ok_or_else(|| "No signing info found".to_string())?;

        let missed: u64 = info
            .missed_blocks_counter
            .parse()
            .unwrap_or(0);

        Ok(ValidatorSigningInfo {
            missed_blocks: missed,
            window_size: 10000, // Default Cosmos SDK signing window
            jailed: info.jailed_until != "1970-01-01T00:00:00Z",
            jail_threshold: 500, // Default: 5% of 10000
        })
    }

    /// Check if expected ports are listening (basic TCP connect test)
    pub async fn check_ports(&self, ports: &[u16]) -> Vec<(u16, bool)> {
        let mut results = Vec::new();
        for &port in ports {
            let addr = format!("127.0.0.1:{}", port);
            let listening = tokio::net::TcpStream::connect(&addr)
                .await
                .is_ok();
            if !listening {
                debug!("Port {} not listening", port);
            }
            results.push((port, listening));
        }
        results
    }

    /// Warn if internal ports (26656, 26657) are bound to 0.0.0.0
    pub async fn check_bind_warnings(&self, ports: &[u16]) -> Vec<String> {
        let mut warnings = Vec::new();
        for &port in ports {
            // Try connecting from outside localhost
            let addr = format!("0.0.0.0:{}", port);
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                warnings.push(format!(
                    "Port {} appears bound to 0.0.0.0 — consider binding to 127.0.0.1",
                    port
                ));
            }
        }
        warnings
    }
}

// ── JSON response types for CometBFT RPC ──

#[derive(Debug, Deserialize)]
struct RpcStatusResponse {
    result: StatusResult,
}

#[derive(Debug, Deserialize)]
struct StatusResult {
    node_info: NodeInfo,
    sync_info: SyncInfo,
}

#[derive(Debug, Deserialize)]
struct NodeInfo {
    id: String,
    network: String,
}

#[derive(Debug, Deserialize)]
struct SyncInfo {
    latest_block_height: String,
    latest_block_time: String,
    catching_up: bool,
}

// ── JSON response types for Cosmos LCD/REST ──

#[derive(Debug, Deserialize)]
struct SigningInfosResponse {
    info: Vec<SigningInfoEntry>,
}

#[derive(Debug, Deserialize)]
struct SigningInfoEntry {
    missed_blocks_counter: String,
    jailed_until: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosmos_health_result_unreachable() {
        let result = CosmosHealthResult::unreachable(50, "connection refused");
        assert!(!result.reachable);
        assert!(result.message.is_some());
        assert!(result.node_status.is_none());
    }

    #[test]
    fn test_cosmos_health_result_stale() {
        let mut result = CosmosHealthResult::unreachable(0, "");
        result.reachable = true;
        result.node_status = Some(CosmosNodeStatus {
            catching_up: false,
            latest_block_height: 1000,
            latest_block_time: None,
            seconds_since_block: Some(300),
            node_id: None,
            network: None,
        });

        assert!(result.is_stale(120)); // 300 > 120
        assert!(!result.is_stale(600)); // 300 < 600
    }

    #[test]
    fn test_cosmos_health_result_not_stale_when_no_status() {
        let result = CosmosHealthResult::unreachable(0, "");
        assert!(!result.is_stale(120));
    }

    #[test]
    fn test_cosmos_health_checker_new() {
        let checker = CosmosHealthChecker::new("http://localhost:26657/");
        assert_eq!(checker.rpc_endpoint, "http://localhost:26657");
    }

    #[test]
    fn test_validator_signing_info() {
        let info = ValidatorSigningInfo {
            missed_blocks: 50,
            window_size: 10000,
            jailed: false,
            jail_threshold: 500,
        };
        assert!(!info.jailed);
        assert!(info.missed_blocks < info.jail_threshold);
    }
}
