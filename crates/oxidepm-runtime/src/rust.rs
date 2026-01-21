//! Single-file Rust runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// Single-file Rust runner - compiles and runs .rs files
pub struct RustRunner;

#[async_trait]
impl Runner for RustRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Check if rustc is available
        let rustc_path = match which::which("rustc") {
            Ok(path) => path,
            Err(_) => {
                return Ok(PrepareResult::failure(
                    "rustc not found in PATH. Please install Rust.",
                ));
            }
        };

        // Validate source file exists
        let source_path = if std::path::Path::new(&spec.command).is_absolute() {
            PathBuf::from(&spec.command)
        } else {
            spec.cwd.join(&spec.command)
        };

        if !source_path.exists() {
            return Ok(PrepareResult::failure(format!(
                "Source file not found: {}",
                source_path.display()
            )));
        }

        // Check file extension
        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        if ext != "rs" {
            return Ok(PrepareResult::failure(format!(
                "Invalid file extension: .{} (expected .rs)",
                ext
            )));
        }

        // Determine output path
        let binary_name = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");

        let output_dir = spec.cwd.join(".oxidepm/bin");
        std::fs::create_dir_all(&output_dir)?;

        let output_path = output_dir.join(binary_name);

        info!(
            "Compiling {} to {}",
            source_path.display(),
            output_path.display()
        );

        // Run rustc
        let mut cmd = Command::new(&rustc_path);
        cmd.arg(&source_path)
            .arg("-o")
            .arg(&output_path)
            .arg("-O") // Optimize
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.map_err(|e| {
            Error::BuildFailed(format!("Failed to run rustc: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(PrepareResult::failure(format!(
                "Compilation failed:\n{}",
                stderr
            )));
        }

        Ok(PrepareResult::success_with_binary(
            format!(
                "Compiled {} successfully",
                source_path.file_name().unwrap().to_string_lossy()
            ),
            output_path,
        ))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        let source_path = if std::path::Path::new(&spec.command).is_absolute() {
            PathBuf::from(&spec.command)
        } else {
            spec.cwd.join(&spec.command)
        };

        let binary_name = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");

        let binary_path = spec.cwd.join(".oxidepm/bin").join(binary_name);

        if !binary_path.exists() {
            return Err(Error::ProcessStartFailed(format!(
                "Binary not found: {}. Run prepare first.",
                binary_path.display()
            )));
        }

        info!("Starting Rust binary: {}", binary_path.display());

        let mut cmd = Command::new(&binary_path);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(&spec.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let child = cmd.spawn().map_err(|e| {
            Error::ProcessStartFailed(format!(
                "Failed to start {}: {}",
                binary_path.display(),
                e
            ))
        })?;

        let pid = child.id().ok_or_else(|| {
            Error::ProcessStartFailed("Rust process started but no PID available".to_string())
        })?;

        info!("Started Rust process {} with PID {}", spec.name, pid);
        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        let source_path = if std::path::Path::new(&spec.command).is_absolute() {
            PathBuf::from(&spec.command)
        } else {
            spec.cwd.join(&spec.command)
        };

        let binary_name = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");

        let binary_path = spec.cwd.join(".oxidepm/bin").join(binary_name);

        let mut parts = vec![binary_path.to_string_lossy().to_string()];
        parts.extend(spec.args.clone());
        parts.join(" ")
    }

    fn mode_name(&self) -> &'static str {
        "rust"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_prepare_missing_file() {
        let dir = TempDir::new().unwrap();
        let runner = RustRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Rust,
            "nonexistent.rs".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn test_prepare_wrong_extension() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("app.txt");
        std::fs::write(&file_path, "not rust").unwrap();

        let runner = RustRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Rust,
            "app.txt".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Invalid file extension"));
    }

    #[tokio::test]
    async fn test_prepare_valid_rust_file() {
        // Skip if rustc is not installed
        if which::which("rustc").is_err() {
            return;
        }

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("app.rs");
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(b"fn main() { println!(\"hello\"); }")
            .unwrap();

        let runner = RustRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Rust,
            "app.rs".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success, "Failed: {}", result.output);
        assert!(result.binary_path.is_some());
    }
}
