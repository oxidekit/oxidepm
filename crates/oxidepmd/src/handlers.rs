//! IPC request handlers

use oxidepm_core::{constants, AppSpec, Result, Selector};
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
                    env: spec.env,
                    mode: spec.mode.to_string(),
                }
            }
            Ok(None) => Response::error("App not found"),
            Err(e) => Response::error(e.to_string()),
        }
    }
}
