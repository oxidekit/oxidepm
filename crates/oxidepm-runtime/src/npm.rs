//! npm/pnpm/yarn script runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// npm/pnpm/yarn script runner
pub struct NpmRunner {
    tool: &'static str,
}

impl NpmRunner {
    pub fn new(tool: &'static str) -> Self {
        Self { tool }
    }
}

#[async_trait]
impl Runner for NpmRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Check if the tool is available
        let tool_path = match which::which(self.tool) {
            Ok(path) => path,
            Err(_) => {
                return Ok(PrepareResult::failure(format!(
                    "{} not found in PATH. Please install {}.",
                    self.tool, self.tool
                )));
            }
        };

        // Check for package.json
        let package_json = spec.cwd.join("package.json");
        if !package_json.exists() {
            return Ok(PrepareResult::failure(format!(
                "package.json not found in {}",
                spec.cwd.display()
            )));
        }

        // Verify the script exists in package.json
        let content = std::fs::read_to_string(&package_json)
            .map_err(|e| Error::ConfigError(format!("Failed to read package.json: {}", e)))?;

        let package: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| Error::ConfigError(format!("Invalid package.json: {}", e)))?;

        let script_name = &spec.command;
        let has_script = package
            .get("scripts")
            .and_then(|s| s.get(script_name))
            .is_some();

        if !has_script {
            return Ok(PrepareResult::failure(format!(
                "Script '{}' not found in package.json scripts",
                script_name
            )));
        }

        Ok(PrepareResult::success(format!(
            "Using {} at {} to run script '{}'",
            self.tool,
            tool_path.display(),
            script_name
        )))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        info!(
            "Starting {} run {} in {}",
            self.tool,
            spec.command,
            spec.cwd.display()
        );

        let mut cmd = Command::new(self.tool);
        cmd.arg("run")
            .arg(&spec.command)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start {}: {}", self.tool, e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed(format!(
                "{} process started but no PID available",
                self.tool
            ))
        })?;

        info!(
            "Started {} process {} with PID {}",
            self.tool, spec.name, pid
        );
        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        let mut parts = vec![self.tool.to_string(), "run".to_string(), spec.command.clone()];
        parts.extend(spec.args.clone());
        parts.join(" ")
    }

    fn mode_name(&self) -> &'static str {
        self.tool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_package_json(dir: &TempDir, scripts: &[(&str, &str)]) {
        let scripts_obj: serde_json::Map<String, serde_json::Value> = scripts
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();

        let package = serde_json::json!({
            "name": "test",
            "version": "1.0.0",
            "scripts": scripts_obj
        });

        let mut file = std::fs::File::create(dir.path().join("package.json")).unwrap();
        file.write_all(package.to_string().as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn test_prepare_no_package_json() {
        let dir = TempDir::new().unwrap();
        let runner = NpmRunner::new("npm");
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Npm,
            "start".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("package.json not found"));
    }

    #[tokio::test]
    async fn test_prepare_missing_script() {
        // Skip if npm is not installed
        if which::which("npm").is_err() {
            return;
        }

        let dir = TempDir::new().unwrap();
        create_package_json(&dir, &[("dev", "echo dev")]);

        let runner = NpmRunner::new("npm");
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Npm,
            "start".to_string(), // Not in scripts
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found in package.json"));
    }

    #[tokio::test]
    async fn test_prepare_valid_script() {
        // Skip if npm is not installed
        if which::which("npm").is_err() {
            return;
        }

        let dir = TempDir::new().unwrap();
        create_package_json(&dir, &[("start", "node index.js")]);

        let runner = NpmRunner::new("npm");
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Npm,
            "start".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success);
    }
}
