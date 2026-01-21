//! Node.js runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// Node.js script runner
pub struct NodeRunner;

#[async_trait]
impl Runner for NodeRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Check if node is available
        let node_path = match which::which("node") {
            Ok(path) => path,
            Err(_) => {
                return Ok(PrepareResult::failure(
                    "Node.js not found in PATH. Please install Node.js.",
                ));
            }
        };

        // Validate script exists
        let script_path = if std::path::Path::new(&spec.command).is_absolute() {
            std::path::PathBuf::from(&spec.command)
        } else {
            spec.cwd.join(&spec.command)
        };

        if !script_path.exists() {
            return Ok(PrepareResult::failure(format!(
                "Script not found: {}",
                script_path.display()
            )));
        }

        // Check file extension
        let ext = script_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        if !matches!(ext, "js" | "mjs" | "cjs" | "ts" | "mts" | "cts") {
            return Ok(PrepareResult::failure(format!(
                "Invalid script extension: .{} (expected .js, .mjs, .cjs, .ts)",
                ext
            )));
        }

        Ok(PrepareResult::success(format!(
            "Using node at {}",
            node_path.display()
        )))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        let script_path = if std::path::Path::new(&spec.command).is_absolute() {
            spec.command.clone()
        } else {
            spec.cwd.join(&spec.command).to_string_lossy().to_string()
        };

        info!("Starting Node.js script: {}", script_path);

        let mut cmd = Command::new("node");
        cmd.arg(&script_path)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start node: {}", e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Node process started but no PID available".to_string())
        })?;

        info!("Started Node.js process {} with PID {}", spec.name, pid);
        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        let mut parts = vec!["node".to_string(), spec.command.clone()];
        parts.extend(spec.args.clone());
        parts.join(" ")
    }

    fn mode_name(&self) -> &'static str {
        "node"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_prepare_missing_node() {
        // This test only works if node is NOT installed
        // Skip if node is available
        if which::which("node").is_ok() {
            return;
        }

        let runner = NodeRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "app.js".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn test_prepare_missing_script() {
        let runner = NodeRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "nonexistent.js".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        // If node is not installed, it will fail on that first
        // If node is installed, it will fail on missing script
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_prepare_valid_script() {
        // Skip if node is not installed
        if which::which("node").is_err() {
            return;
        }

        let mut file = NamedTempFile::with_suffix(".js").unwrap();
        file.write_all(b"console.log('hello');").unwrap();

        let runner = NodeRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            file.path().to_string_lossy().to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success);
    }
}
