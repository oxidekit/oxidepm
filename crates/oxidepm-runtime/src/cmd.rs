//! Generic command runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// Generic command runner - runs any command
pub struct CmdRunner;

#[async_trait]
impl Runner for CmdRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // For generic commands, just verify the command exists
        let cmd_parts: Vec<&str> = spec.command.split_whitespace().collect();
        if cmd_parts.is_empty() {
            return Ok(PrepareResult::failure("Empty command"));
        }

        let program = cmd_parts[0];
        match which::which(program) {
            Ok(path) => Ok(PrepareResult::success(format!(
                "Found {} at {}",
                program,
                path.display()
            ))),
            Err(_) => {
                // Check if it's an absolute path
                if std::path::Path::new(program).exists() {
                    Ok(PrepareResult::success(format!("Using {}", program)))
                } else {
                    Ok(PrepareResult::failure(format!(
                        "Command not found: {}",
                        program
                    )))
                }
            }
        }
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        info!("Starting command: {} {:?}", spec.command, spec.args);

        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start '{}': {}", spec.command, e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Process started but no PID available".to_string())
        })?;

        info!("Started process {} with PID {}", spec.name, pid);
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
        "cmd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_prepare_valid_command() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Cmd,
            "echo".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_prepare_invalid_command() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Cmd,
            "nonexistent_command_12345".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_start_echo() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Cmd,
            "sleep".to_string(),
            PathBuf::from("/tmp"),
        )
        .with_args(vec!["1".to_string()]);

        let mut process = runner.start(&spec).await.unwrap();
        assert!(process.pid > 0);

        // Clean up
        process.child.kill().await.ok();
    }
}
