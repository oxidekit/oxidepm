//! Cargo project runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Error, Result};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, warn};

use crate::traits::{PrepareResult, Runner, RunningProcess};

/// Cargo project runner - builds and runs Rust projects
pub struct CargoRunner;

#[async_trait]
impl Runner for CargoRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // Check if cargo is available
        let cargo_path = match which::which("cargo") {
            Ok(path) => path,
            Err(_) => {
                return Ok(PrepareResult::failure(
                    "Cargo not found in PATH. Please install Rust.",
                ));
            }
        };

        // Check for Cargo.toml
        let cargo_toml = spec.cwd.join("Cargo.toml");
        if !cargo_toml.exists() {
            return Ok(PrepareResult::failure(format!(
                "Cargo.toml not found in {}",
                spec.cwd.display()
            )));
        }

        info!("Building Cargo project in {}", spec.cwd.display());

        // Run cargo build --release
        let mut cmd = Command::new(&cargo_path);
        cmd.arg("build")
            .arg("--release")
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.map_err(|e| {
            Error::BuildFailed(format!("Failed to run cargo build: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(PrepareResult::failure(format!(
                "Build failed:\n{}",
                stderr
            )));
        }

        // Find the binary
        let binary_name = find_binary_name(&spec.cwd, &spec.command)?;
        let binary_path = spec.cwd.join("target/release").join(&binary_name);

        if !binary_path.exists() {
            return Ok(PrepareResult::failure(format!(
                "Binary not found at {}. Available binaries: {:?}",
                binary_path.display(),
                list_release_binaries(&spec.cwd)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(PrepareResult::success_with_binary(
            format!("Build successful\n{}", stdout),
            binary_path,
        ))
    }

    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess> {
        let binary_name = find_binary_name(&spec.cwd, &spec.command)?;
        let binary_path = spec.cwd.join("target/release").join(&binary_name);

        if !binary_path.exists() {
            return Err(Error::ProcessStartFailed(format!(
                "Binary not found: {}. Run prepare first.",
                binary_path.display()
            )));
        }

        info!("Starting Cargo binary: {}", binary_path.display());

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
            Error::ProcessStartFailed("Cargo process started but no PID available".to_string())
        })?;

        info!("Started Cargo process {} with PID {}", spec.name, pid);
        Ok(RunningProcess::new(pid, child))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        let binary_name = find_binary_name(&spec.cwd, &spec.command).unwrap_or_else(|_| spec.command.clone());
        let binary_path = spec.cwd.join("target/release").join(&binary_name);

        let mut parts = vec![binary_path.to_string_lossy().to_string()];
        parts.extend(spec.args.clone());
        parts.join(" ")
    }

    fn mode_name(&self) -> &'static str {
        "cargo"
    }
}

/// Find the binary name from Cargo.toml or use the provided name
fn find_binary_name(cwd: &std::path::Path, hint: &str) -> Result<String> {
    // If hint is not empty and doesn't look like a default, use it
    if !hint.is_empty() && hint != "." && hint != "./" {
        return Ok(hint.to_string());
    }

    // Try to read Cargo.toml
    let cargo_toml = cwd.join("Cargo.toml");
    if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
        // Simple TOML parsing for package name
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("name") {
                if let Some(name) = line.split('=').nth(1) {
                    let name = name.trim().trim_matches('"').trim_matches('\'');
                    return Ok(name.to_string());
                }
            }
        }
    }

    // Fall back to directory name
    if let Some(name) = cwd.file_name().and_then(|n| n.to_str()) {
        warn!("Could not determine binary name, using directory name: {}", name);
        return Ok(name.to_string());
    }

    Err(Error::ConfigError(
        "Could not determine binary name".to_string(),
    ))
}

/// List available binaries in target/release
fn list_release_binaries(cwd: &std::path::Path) -> Vec<String> {
    let release_dir = cwd.join("target/release");
    if !release_dir.exists() {
        return vec![];
    }

    std::fs::read_dir(&release_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && e.path().extension().is_none() // No extension = likely binary
                })
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_cargo_project(dir: &TempDir, name: &str) {
        let cargo_toml = format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "{}"
path = "src/main.rs"
"#,
            name, name
        );

        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut file = std::fs::File::create(dir.path().join("Cargo.toml")).unwrap();
        file.write_all(cargo_toml.as_bytes()).unwrap();

        let mut main = std::fs::File::create(dir.path().join("src/main.rs")).unwrap();
        main.write_all(b"fn main() { println!(\"hello\"); }")
            .unwrap();
    }

    #[tokio::test]
    async fn test_prepare_no_cargo_toml() {
        let dir = TempDir::new().unwrap();
        let runner = CargoRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Cargo,
            "myapp".to_string(),
            dir.path().to_path_buf(),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Cargo.toml not found"));
    }

    #[test]
    fn test_find_binary_name_with_hint() {
        let dir = TempDir::new().unwrap();
        let result = find_binary_name(&dir.path().to_path_buf(), "myapp").unwrap();
        assert_eq!(result, "myapp");
    }

    #[test]
    fn test_find_binary_name_from_cargo_toml() {
        let dir = TempDir::new().unwrap();
        create_cargo_project(&dir, "test-app");

        let result = find_binary_name(&dir.path().to_path_buf(), "").unwrap();
        assert_eq!(result, "test-app");
    }
}
