//! Cosmos node lifecycle management — relay-until-synced state machine
//!
//! State machine for validator nodes:
//!   RelayingSyncing → TransitioningToValidator → ValidatorActive → ValidatorCatchingUp
//!
//! SECURITY: Validator key operations only rename the file, NEVER read its contents.

use oxidepm_core::{CosmosConfig, CosmosNodeMode};
use oxidepm_health::{CosmosHealthChecker, CosmosHealthResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// Cosmos validator lifecycle state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CosmosLifecycleState {
    /// Running as relay, waiting for sync to complete
    RelayingSyncing,
    /// Synced, stopping process to restore validator key
    TransitioningToValidator,
    /// Actively signing blocks as validator
    ValidatorActive,
    /// Validator fell behind, alerting but NOT disabling key
    ValidatorCatchingUp,
}

impl CosmosLifecycleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RelayingSyncing => "relaying_syncing",
            Self::TransitioningToValidator => "transitioning",
            Self::ValidatorActive => "validator_active",
            Self::ValidatorCatchingUp => "validator_catching_up",
        }
    }
}

impl std::fmt::Display for CosmosLifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Manages the relay-until-synced lifecycle for a Cosmos validator node
pub struct CosmosLifecycle {
    pub state: CosmosLifecycleState,
    pub config: CosmosConfig,
    health_checker: CosmosHealthChecker,
    /// Last health check result
    pub last_health: Option<CosmosHealthResult>,
}

impl CosmosLifecycle {
    /// Create a new lifecycle manager
    pub fn new(config: CosmosConfig) -> Self {
        let health_checker = CosmosHealthChecker::new(&config.rpc_endpoint);

        // Determine initial state
        let initial_state = if config.relay_until_synced
            && config.node_mode == CosmosNodeMode::Validator
        {
            CosmosLifecycleState::RelayingSyncing
        } else if config.node_mode == CosmosNodeMode::Validator {
            CosmosLifecycleState::ValidatorActive
        } else {
            // Seeds and relays don't use the lifecycle state machine
            CosmosLifecycleState::RelayingSyncing
        };

        Self {
            state: initial_state,
            config,
            health_checker,
            last_health: None,
        }
    }

    /// Run one health check cycle and return updated state
    /// Returns (new_state, needs_restart) — caller handles the restart
    pub async fn tick(&mut self) -> (CosmosLifecycleState, bool) {
        let health = self.health_checker.check(&self.config).await;
        self.last_health = Some(health.clone());

        if !health.reachable {
            return (self.state, false);
        }

        let node_status = match &health.node_status {
            Some(s) => s,
            None => return (self.state, false),
        };

        let old_state = self.state;
        let mut needs_restart = false;

        match self.state {
            CosmosLifecycleState::RelayingSyncing => {
                if !node_status.catching_up {
                    info!(
                        "Node synced at height {} — transitioning to validator",
                        node_status.latest_block_height
                    );
                    self.state = CosmosLifecycleState::TransitioningToValidator;
                    needs_restart = true; // Caller should: stop, enable key, start
                }
            }

            CosmosLifecycleState::TransitioningToValidator => {
                // This state is transient — caller handles the restart
                // After restart with key enabled, we move to ValidatorActive
                if !node_status.catching_up {
                    self.state = CosmosLifecycleState::ValidatorActive;
                    info!("Validator active at height {}", node_status.latest_block_height);
                }
            }

            CosmosLifecycleState::ValidatorActive => {
                if node_status.catching_up {
                    warn!(
                        "Validator fell behind at height {} — alerting only, NOT disabling key",
                        node_status.latest_block_height
                    );
                    self.state = CosmosLifecycleState::ValidatorCatchingUp;
                }
            }

            CosmosLifecycleState::ValidatorCatchingUp => {
                // Alert only — DON'T disable the key
                if !node_status.catching_up {
                    info!(
                        "Validator caught up at height {}",
                        node_status.latest_block_height
                    );
                    self.state = CosmosLifecycleState::ValidatorActive;
                }
            }
        }

        if old_state != self.state {
            info!(
                "Cosmos lifecycle: {} → {}",
                old_state, self.state
            );
        }

        (self.state, needs_restart)
    }

    /// Get the health checker (for port monitoring etc.)
    pub fn health_checker(&self) -> &CosmosHealthChecker {
        &self.health_checker
    }

    /// Get the last known block height
    pub fn block_height(&self) -> Option<u64> {
        self.last_health
            .as_ref()
            .and_then(|h| h.node_status.as_ref())
            .map(|s| s.latest_block_height)
    }

    /// Check if the node is stale
    pub fn is_stale(&self) -> bool {
        self.last_health
            .as_ref()
            .map(|h| h.is_stale(oxidepm_core::COSMOS_STALE_THRESHOLD_SECS))
            .unwrap_or(false)
    }
}

/// Disable the validator key by renaming .json → .json.disabled
///
/// SECURITY: Only renames the file, NEVER reads its contents.
pub fn disable_validator_key(key_path: &Path) -> std::io::Result<PathBuf> {
    let disabled_path = append_extension(key_path, "disabled");
    if key_path.exists() {
        info!("Disabling validator key (renaming file)");
        std::fs::rename(key_path, &disabled_path)?;
        Ok(disabled_path)
    } else if disabled_path.exists() {
        info!("Validator key already disabled");
        Ok(disabled_path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Validator key file not found (checked both enabled and disabled paths)",
        ))
    }
}

/// Enable the validator key by renaming .json.disabled → .json
///
/// SECURITY: Only renames the file, NEVER reads its contents.
pub fn enable_validator_key(key_path: &Path) -> std::io::Result<PathBuf> {
    let disabled_path = append_extension(key_path, "disabled");
    if disabled_path.exists() {
        info!("Enabling validator key (renaming file)");
        std::fs::rename(&disabled_path, key_path)?;
        Ok(key_path.to_path_buf())
    } else if key_path.exists() {
        info!("Validator key already enabled");
        Ok(key_path.to_path_buf())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Validator key file not found (checked both enabled and disabled paths)",
        ))
    }
}

/// Check for .disabled state on startup and warn
pub fn check_disabled_key_on_startup(key_path: &Path) {
    let disabled_path = append_extension(key_path, "disabled");
    if disabled_path.exists() && !key_path.exists() {
        warn!(
            "Validator key is in disabled state — relay-until-synced may have been interrupted. \
             The key will be re-enabled when the node syncs."
        );
    }
}

/// Append an extension to a path (e.g., foo.json → foo.json.disabled)
fn append_extension(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_lifecycle_state_display() {
        assert_eq!(
            CosmosLifecycleState::RelayingSyncing.to_string(),
            "relaying_syncing"
        );
        assert_eq!(
            CosmosLifecycleState::ValidatorActive.to_string(),
            "validator_active"
        );
    }

    #[test]
    fn test_disable_enable_validator_key() {
        let dir = TempDir::new().unwrap();
        let key_path = dir.path().join("priv_validator_key.json");

        // Create a dummy key file (we never read its contents)
        std::fs::write(&key_path, "dummy").unwrap();
        assert!(key_path.exists());

        // Disable
        let disabled = disable_validator_key(&key_path).unwrap();
        assert!(!key_path.exists());
        assert!(disabled.exists());
        assert!(disabled.to_str().unwrap().ends_with(".disabled"));

        // Enable
        let enabled = enable_validator_key(&key_path).unwrap();
        assert!(enabled.exists());
        assert!(key_path.exists());
        assert!(!disabled.exists());
    }

    #[test]
    fn test_disable_already_disabled() {
        let dir = TempDir::new().unwrap();
        let key_path = dir.path().join("priv_validator_key.json");
        let disabled_path = dir.path().join("priv_validator_key.json.disabled");

        std::fs::write(&disabled_path, "dummy").unwrap();

        let result = disable_validator_key(&key_path).unwrap();
        assert_eq!(result, disabled_path);
    }

    #[test]
    fn test_enable_already_enabled() {
        let dir = TempDir::new().unwrap();
        let key_path = dir.path().join("priv_validator_key.json");

        std::fs::write(&key_path, "dummy").unwrap();

        let result = enable_validator_key(&key_path).unwrap();
        assert_eq!(result, key_path);
    }

    #[test]
    fn test_disable_not_found() {
        let dir = TempDir::new().unwrap();
        let key_path = dir.path().join("nonexistent.json");

        let result = disable_validator_key(&key_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_append_extension() {
        let path = PathBuf::from("/root/.mono/config/priv_validator_key.json");
        let result = append_extension(&path, "disabled");
        assert_eq!(
            result,
            PathBuf::from("/root/.mono/config/priv_validator_key.json.disabled")
        );
    }

    #[test]
    fn test_initial_state_relay_until_synced() {
        let config = CosmosConfig {
            node_mode: CosmosNodeMode::Validator,
            relay_until_synced: true,
            rpc_endpoint: "http://localhost:26657".to_string(),
            chain_id: "test-1".to_string(),
            validator_key_path: None,
            nofile_limit: None,
            shutdown_timeout: 30,
            detect_upgrade_halt: true,
        };
        let lifecycle = CosmosLifecycle::new(config);
        assert_eq!(lifecycle.state, CosmosLifecycleState::RelayingSyncing);
    }

    #[test]
    fn test_initial_state_validator_no_relay() {
        let config = CosmosConfig {
            node_mode: CosmosNodeMode::Validator,
            relay_until_synced: false,
            rpc_endpoint: "http://localhost:26657".to_string(),
            chain_id: "test-1".to_string(),
            validator_key_path: None,
            nofile_limit: None,
            shutdown_timeout: 30,
            detect_upgrade_halt: true,
        };
        let lifecycle = CosmosLifecycle::new(config);
        assert_eq!(lifecycle.state, CosmosLifecycleState::ValidatorActive);
    }

    #[test]
    fn test_check_disabled_key_on_startup() {
        let dir = TempDir::new().unwrap();
        let key_path = dir.path().join("priv_validator_key.json");
        let disabled_path = dir.path().join("priv_validator_key.json.disabled");

        // Create disabled key file
        std::fs::write(&disabled_path, "dummy").unwrap();

        // Should warn but not panic
        check_disabled_key_on_startup(&key_path);
    }
}
