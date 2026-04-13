//! IPC request handlers

use oxidepm_core::{constants, AppMode, AppSpec, Result, Selector};
use oxidepm_health::CosmosHealthChecker;
use oxidepm_ipc::Response;
use oxidepm_logs::{stderr_path, stdout_path};
use std::fs::OpenOptions;
use tracing::{error, info, warn};

use crate::supervisor::Supervisor;

/// Request handler for IPC commands
pub struct RequestHandler {
    supervisor: Supervisor,
}

impl RequestHandler {
    pub fn new(supervisor: Supervisor) -> Self {
        Self { supervisor }
    }

    /// Handle start request
    pub async fn start(&mut self, spec: AppSpec) -> Response {
        info!("Handling start request for: {}", spec.name);

        match self.supervisor.start(spec.clone()).await {
            Ok(id) => Response::Started {
                id,
                name: spec.name,
            },
            Err(e) => {
                error!("Start failed: {}", e);
                Response::error(e.to_string())
            }
        }
    }

    /// Handle stop request
    pub async fn stop(&mut self, selector: Selector) -> Response {
        info!("Handling stop request for: {}", selector);

        match self.supervisor.resolve_selector(&selector).await {
            Ok(ids) => {
                let mut count = 0;
                for id in ids {
                    match self.supervisor.stop(id).await {
                        Ok(true) => count += 1,
                        Ok(false) => {}
                        Err(e) => error!("Error stopping {}: {}", id, e),
                    }
                }
                Response::Stopped { count }
            }
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle restart request
    pub async fn restart(&mut self, selector: Selector) -> Response {
        info!("Handling restart request for: {}", selector);

        match self.supervisor.resolve_selector(&selector).await {
            Ok(ids) => {
                let mut count = 0;
                for id in ids {
                    match self.supervisor.restart(id).await {
                        Ok(true) => count += 1,
                        Ok(false) => {}
                        Err(e) => error!("Error restarting {}: {}", id, e),
                    }
                }
                Response::Restarted { count }
            }
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle delete request
    pub async fn delete(&mut self, selector: Selector) -> Response {
        info!("Handling delete request for: {}", selector);

        match self.supervisor.resolve_selector(&selector).await {
            Ok(ids) => {
                let mut count = 0;
                for id in ids {
                    match self.supervisor.delete(id).await {
                        Ok(true) => count += 1,
                        Ok(false) => {}
                        Err(e) => error!("Error deleting {}: {}", id, e),
                    }
                }
                Response::Deleted { count }
            }
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle status request
    pub async fn status(&self) -> Response {
        match self.supervisor.status().await {
            Ok(apps) => Response::Status { apps },
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle show request
    pub async fn show(&self, selector: Selector) -> Response {
        match self.supervisor.show(&selector).await {
            Ok(Some(app)) => Response::Show { app },
            Ok(None) => Response::error("App not found"),
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle logs request
    pub async fn logs(
        &self,
        selector: Selector,
        lines: usize,
        stdout: bool,
        stderr: bool,
    ) -> Response {
        match self.supervisor.logs(&selector, lines, stdout, stderr).await {
            Ok(log_lines) => Response::LogLines { lines: log_lines },
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle save request
    pub async fn save(&self) -> Response {
        match self.supervisor.save().await {
            Ok(count) => Response::Saved {
                count,
                path: constants::saved_path().to_string_lossy().to_string(),
            },
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle resurrect request
    pub async fn resurrect(&mut self) -> Response {
        match self.supervisor.resurrect().await {
            Ok(count) => Response::Resurrected { count },
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle reload request (graceful zero-downtime restart)
    pub async fn reload(&mut self, selector: Selector) -> Response {
        info!("Handling reload request for: {}", selector);

        match self.supervisor.resolve_selector(&selector).await {
            Ok(ids) => {
                let mut count = 0;
                for id in ids {
                    match self.supervisor.reload(id).await {
                        Ok(true) => count += 1,
                        Ok(false) => {}
                        Err(e) => error!("Error reloading {}: {}", id, e),
                    }
                }
                Response::Reloaded { count }
            }
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Handle flush request (truncate log files)
    pub async fn flush(&self, selector: Selector) -> Response {
        info!("Handling flush request for: {}", selector);

        match self.supervisor.resolve_selector(&selector).await {
            Ok(ids) => {
                let mut count = 0;
                for id in ids {
                    match self.flush_logs_for_app(id).await {
                        Ok(true) => count += 1,
                        Ok(false) => {}
                        Err(e) => error!("Error flushing logs for {}: {}", id, e),
                    }
                }
                Response::Flushed { count }
            }
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Flush logs for a single app
    async fn flush_logs_for_app(&self, id: u32) -> Result<bool> {
        // Get app info to get the name
        match self.supervisor.show(&Selector::ById(id)).await? {
            Some(app_info) => {
                let name = &app_info.spec.name;

                // Truncate stdout log
                let stdout = stdout_path(name);
                if stdout.exists() {
                    if let Err(e) = OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&stdout)
                    {
                        warn!("Failed to truncate stdout log for {}: {}", name, e);
                    }
                }

                // Truncate stderr log
                let stderr = stderr_path(name);
                if stderr.exists() {
                    if let Err(e) = OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&stderr)
                    {
                        warn!("Failed to truncate stderr log for {}: {}", name, e);
                    }
                }

                info!("Flushed logs for {}", name);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Handle describe request (show what command would run)
    pub async fn describe(&self, selector: Selector) -> Response {
        info!("Handling describe request for: {}", selector);

        match self.supervisor.show(&selector).await {
            Ok(Some(app_info)) => {
                let spec = app_info.spec;
                Response::Described {
                    name: spec.name,
                    command: spec.command,
                    args: spec.args,
                    cwd: spec.cwd.to_string_lossy().to_string(),
                    env: spec.env.iter().map(|(k, v)| {
                        if is_sensitive(k) {
                            (k.clone(), "****".to_string())
                        } else {
                            (k.clone(), v.clone())
                        }
                    }).collect(),
                    mode: spec.mode.to_string(),
                }
            }
            Ok(None) => Response::error("App not found"),
            Err(e) => Response::error(e.to_string()),
        }
    }

    /// Stop all running processes and save state (for daemon shutdown)
    pub async fn stop_all_and_save(&mut self) -> Response {
        info!("Stopping all processes before daemon shutdown...");
        let stopped = self.supervisor.stop_all().await;
        let _ = self.supervisor.save().await;
        Response::ok(format!("Stopped {} process(es), daemon shutting down", stopped))
    }

    /// Handle cosmos-status request
    pub async fn cosmos_status(&self, selector: Selector) -> Response {
        info!("Handling cosmos-status request for: {}", selector);

        match self.supervisor.show(&selector).await {
            Ok(Some(app_info)) => {
                let spec = &app_info.spec;
                let state = &app_info.state;

                if spec.mode != AppMode::Cosmos {
                    return Response::error(format!(
                        "'{}' is not a cosmos process\n\nUsage: oxidepm cosmos-status <name>\nExample: oxidepm cosmos-status monod",
                        spec.name
                    ));
                }

                let cosmos = match spec.cosmos_config.as_ref() {
                    Some(c) => c,
                    None => {
                        return Response::error(format!(
                            "'{}' has no cosmos config\n\nUsage: oxidepm cosmos-status <name>\nExample: oxidepm cosmos-status monod",
                            spec.name
                        ));
                    }
                };

                // Query the node's RPC endpoint for live data
                let checker = CosmosHealthChecker::new(&cosmos.rpc_endpoint);
                let health = checker.check(cosmos).await;

                let (catching_up, block_height, seconds_since_block) =
                    if let Some(ref node_status) = health.node_status {
                        (
                            Some(node_status.catching_up),
                            Some(node_status.latest_block_height),
                            node_status.seconds_since_block,
                        )
                    } else {
                        (None, None, None)
                    };

                // Derive lifecycle state from node status and config
                let lifecycle_state = if !health.reachable {
                    Some("unreachable".to_string())
                } else if let Some(ref ns) = health.node_status {
                    if state.status == oxidepm_core::AppStatus::UpgradeHalted {
                        Some("upgrade_halted".to_string())
                    } else if cosmos.relay_until_synced
                        && cosmos.node_mode == oxidepm_core::CosmosNodeMode::Validator
                        && ns.catching_up
                    {
                        Some("relaying_syncing".to_string())
                    } else if cosmos.node_mode == oxidepm_core::CosmosNodeMode::Validator {
                        if ns.catching_up {
                            Some("validator_catching_up".to_string())
                        } else {
                            Some("validator_active".to_string())
                        }
                    } else {
                        if ns.catching_up {
                            Some("syncing".to_string())
                        } else {
                            Some("synced".to_string())
                        }
                    }
                } else {
                    None
                };

                Response::CosmosStatus {
                    name: spec.name.clone(),
                    lifecycle_state,
                    catching_up,
                    block_height,
                    seconds_since_block,
                    chain_id: Some(cosmos.chain_id.clone()),
                    node_mode: Some(cosmos.node_mode.to_string()),
                    upgrade_name: state.upgrade_name.clone(),
                    upgrade_halt_height: state.upgrade_halt_height,
                }
            }
            Ok(None) => Response::error(
                "App not found\n\nUsage: oxidepm cosmos-status <name>\nExample: oxidepm cosmos-status monod"
            ),
            Err(e) => Response::error(e.to_string()),
        }
    }
}

/// Check if an environment variable key likely contains sensitive data.
fn is_sensitive(key: &str) -> bool {
    let key_upper = key.to_uppercase();
    key_upper.contains("SECRET")
        || key_upper.contains("TOKEN")
        || key_upper.contains("PASSWORD")
        || key_upper.contains("KEY")
        || key_upper.contains("PRIVATE")
}
