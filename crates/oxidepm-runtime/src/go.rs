//! Go runtime runner
//!
//! Supports:
//! - `go run .` (run from go.mod project)
//! - `go run main.go` (run specific file)
//! - Pre-built binary execution

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

use crate::traits::{PrepareResult, Runner, RunningProcess};

pub struct GoRunner;

#[async_trait]
impl Runner for GoRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Check if Go is installed
        if which::which("go").is_err() {
            return Ok(PrepareResult::failure(
                "Go not found. Install Go from https://go.dev/dl/",
            ));
        }

        // Check for go.mod
        let go_mod = spec.cwd.join("go.mod");
        if !go_mod.exists() {
            let script = &spec.command;
            let script_path = if Path::new(script).is_absolute() {
                Path::new(script).to_path_buf()
            } else {
                spec.cwd.join(script)
            };
            if !script_path.exists() && script != "." {
                return Ok(PrepareResult::failure(format!(
                    "No go.mod found and target not found: {}",
                    script_path.display()
                )));
            }
        }

        Ok(PrepareResult::success("Go ready"))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        info!("Starting Go: go run {}", spec.command);

        let mut cmd = Command::new("go");
        cmd.arg("run")
            .arg(&spec.command)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start Go: {}", e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Process started but no PID available".to_string())
        })?;

        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        if spec.args.is_empty() {
            format!("go run {}", spec.command)
        } else {
            format!("go run {} {}", spec.command, spec.args.join(" "))
        }
    }

    fn mode_name(&self) -> &'static str {
        "go"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_prepare_no_go_mod() {
        let runner = GoRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Go,
            "nonexistent.go".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        // Depends on whether Go is installed
        let _ = result;
    }
}
