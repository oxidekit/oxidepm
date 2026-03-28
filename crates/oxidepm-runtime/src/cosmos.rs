//! Cosmos SDK node runner
//!
//! Manages Cosmos blockchain nodes (monod, gaiad, etc.) with:
//! - RLIMIT_NOFILE configuration via pre_exec
//! - Validator key file existence check (never reads contents)
//! - World-readable config directory warning

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, warn};

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// Cosmos SDK blockchain node runner
pub struct CosmosRunner;

#[async_trait]
impl Runner for CosmosRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Verify binary exists
        let binary = &spec.command;
        let binary_path = Path::new(binary);

        let found = if binary_path.is_absolute() {
            binary_path.exists()
        } else {
            which::which(binary).is_ok()
        };

        if !found {
            return Ok(PrepareResult::failure(format!(
                "Cosmos binary not found: {}",
                binary
            )));
        }

        // Check cosmos config
        if let Some(ref cosmos) = spec.cosmos_config {
            // Check validator key file exists if path is configured (never read contents)
            if let Some(ref key_path) = cosmos.validator_key_path {
                if !key_path.exists() {
                    // Also check for .disabled variant (relay-until-synced may have renamed it)
                    let disabled = key_path.with_extension("json.disabled");
                    if !disabled.exists() {
                        warn!(
                            "Validator key file not found (checked both enabled and disabled paths)"
                        );
                    }
                }
            }

            // Warn on world-readable config directory
            #[cfg(unix)]
            {
                if let Some(ref key_path) = cosmos.validator_key_path {
                    if let Some(config_dir) = key_path.parent() {
                        check_permissions_warning(config_dir);
                    }
                }
            }
        }

        Ok(PrepareResult::success(format!("Cosmos binary ready: {}", binary)))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        info!("Starting Cosmos node: {} {}", spec.command, spec.args.join(" "));

        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        // Set RLIMIT_NOFILE via pre_exec if configured
        #[cfg(unix)]
        {
            let nofile = spec
                .cosmos_config
                .as_ref()
                .and_then(|c| c.nofile_limit)
                .unwrap_or(oxidepm_core::COSMOS_DEFAULT_NOFILE);

            unsafe {
                cmd.pre_exec(move || {
                    // Set RLIMIT_NOFILE for the child process
                    // Use raw libc call to avoid dependency issues in pre_exec context
                    #[repr(C)]
                    struct Rlimit {
                        rlim_cur: u64,
                        rlim_max: u64,
                    }
                    extern "C" {
                        fn setrlimit(resource: i32, rlim: *const Rlimit) -> i32;
                    }
                    const RLIMIT_NOFILE: i32 = {
                        #[cfg(target_os = "macos")]
                        { 8 }
                        #[cfg(target_os = "linux")]
                        { 7 }
                    };
                    let rlim = Rlimit {
                        rlim_cur: nofile,
                        rlim_max: nofile,
                    };
                    setrlimit(RLIMIT_NOFILE, &rlim);
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start Cosmos node '{}': {}", spec.command, e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Cosmos node started but no PID available".to_string())
        })?;

        info!("Started Cosmos node {} with PID {}", spec.name, pid);
        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        if spec.args.is_empty() {
            spec.command.clone()
        } else {
            format!("{} {}", spec.command, spec.args.join(" "))
        }
    }

    fn mode_name(&self) -> &'static str {
        "cosmos"
    }
}

/// Check if a directory has world-readable permissions and warn
#[cfg(unix)]
fn check_permissions_warning(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o004 != 0 {
            warn!(
                "Config directory is world-readable, consider restricting permissions"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::{AppMode, CosmosConfig};
    use std::path::PathBuf;

    fn cosmos_spec() -> AppSpec {
        let mut spec = AppSpec::new(
            "monod".to_string(),
            AppMode::Cosmos,
            "echo".to_string(), // use echo as dummy binary for tests
            PathBuf::from("/tmp"),
        );
        spec.cosmos_config = Some(CosmosConfig {
            node_mode: oxidepm_core::CosmosNodeMode::Validator,
            rpc_endpoint: "http://localhost:26657".to_string(),
            chain_id: "mono_6940-1".to_string(),
            relay_until_synced: true,
            validator_key_path: None,
            nofile_limit: Some(65535),
            shutdown_timeout: 30,
        });
        spec
    }

    #[tokio::test]
    async fn test_prepare_valid_binary() {
        let runner = CosmosRunner;
        let spec = cosmos_spec();
        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_prepare_missing_binary() {
        let runner = CosmosRunner;
        let mut spec = cosmos_spec();
        spec.command = "nonexistent_cosmos_binary_xyz".to_string();
        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
    }

    #[test]
    fn test_command_string() {
        let runner = CosmosRunner;
        let spec = AppSpec::new(
            "monod".to_string(),
            AppMode::Cosmos,
            "/usr/local/bin/monod".to_string(),
            PathBuf::from("/root"),
        )
        .with_args(vec!["start".to_string(), "--home".to_string(), "/root/.mono".to_string()]);

        assert_eq!(
            runner.command_string(&spec),
            "/usr/local/bin/monod start --home /root/.mono"
        );
    }

    #[test]
    fn test_mode_name() {
        assert_eq!(CosmosRunner.mode_name(), "cosmos");
    }
}
