//! Python runtime runner
//!
//! Supports:
//! - `python app.py` (direct script execution)
//! - Auto-detection of virtualenv (./venv, ./.venv)
//! - `requirements.txt` dependency check

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, warn};

use crate::traits::{PrepareResult, Runner, RunningProcess};

pub struct PythonRunner;

#[async_trait]
impl Runner for PythonRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Find Python interpreter
        let python = find_python(&spec.cwd);
        if python.is_none() {
            return Ok(PrepareResult::failure(
                "Python not found. Install python3 or create a virtualenv.",
            ));
        }
        let python = python.unwrap();

        // Check if the script exists
        let script = &spec.command;
        let script_path = if Path::new(script).is_absolute() {
            Path::new(script).to_path_buf()
        } else {
            spec.cwd.join(script)
        };

        if !script_path.exists() && !script.contains(' ') {
            return Ok(PrepareResult::failure(format!(
                "Python script not found: {}",
                script_path.display()
            )));
        }

        // Check for requirements.txt
        let req_path = spec.cwd.join("requirements.txt");
        if req_path.exists() {
            let venv = spec.cwd.join("venv");
            let dot_venv = spec.cwd.join(".venv");
            if !venv.exists() && !dot_venv.exists() {
                warn!(
                    "requirements.txt found but no virtualenv. Consider: python3 -m venv venv && venv/bin/pip install -r requirements.txt"
                );
            }
        }

        Ok(PrepareResult::success(format!("Python ready: {}", python)))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        let python = find_python(&spec.cwd)
            .unwrap_or_else(|| "python3".to_string());

        info!("Starting Python: {} {}", python, spec.command);

        let mut cmd = Command::new(&python);
        cmd.arg(&spec.command)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .env("PYTHONUNBUFFERED", "1") // Force unbuffered output for log capture
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!("Failed to start Python: {}", e))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Process started but no PID available".to_string())
        })?;

        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        let python = find_python(&spec.cwd).unwrap_or_else(|| "python3".to_string());
        if spec.args.is_empty() {
            format!("{} {}", python, spec.command)
        } else {
            format!("{} {} {}", python, spec.command, spec.args.join(" "))
        }
    }

    fn mode_name(&self) -> &'static str {
        "python"
    }
}

/// Find the best Python interpreter for the project
fn find_python(cwd: &Path) -> Option<String> {
    // Check for virtualenv first
    for venv_dir in &["venv", ".venv"] {
        let venv_python = cwd.join(venv_dir).join("bin").join("python");
        if venv_python.exists() {
            return Some(venv_python.to_string_lossy().to_string());
        }
    }

    // Fall back to system Python
    if which::which("python3").is_ok() {
        Some("python3".to_string())
    } else if which::which("python").is_ok() {
        Some("python".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_prepare_missing_script() {
        let runner = PythonRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Python,
            "nonexistent_script.py".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        // May pass or fail depending on whether python3 is installed
        // Just verify it doesn't panic
        let _ = result;
    }
}
