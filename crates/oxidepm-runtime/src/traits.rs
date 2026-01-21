//! Runner trait and common types

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Result};
use std::path::PathBuf;
use tokio::process::Child;

/// Result of the prepare phase (build/validate)
#[derive(Debug)]
pub struct PrepareResult {
    pub success: bool,
    pub output: String,
    pub binary_path: Option<PathBuf>,
}

impl PrepareResult {
    pub fn success<S: Into<String>>(output: S) -> Self {
        Self {
            success: true,
            output: output.into(),
            binary_path: None,
        }
    }

    pub fn success_with_binary<S: Into<String>>(output: S, path: PathBuf) -> Self {
        Self {
            success: true,
            output: output.into(),
            binary_path: Some(path),
        }
    }

    pub fn failure<S: Into<String>>(output: S) -> Self {
        Self {
            success: false,
            output: output.into(),
            binary_path: None,
        }
    }
}

/// A running process with its handles
pub struct RunningProcess {
    pub pid: u32,
    pub child: Child,
}

impl RunningProcess {
    pub fn new(pid: u32, child: Child) -> Self {
        Self { pid, child }
    }
}

/// Trait for process runners (Node, Cargo, etc.)
#[async_trait]
pub trait Runner: Send + Sync {
    /// Prepare the process (build for Rust, validate for Node)
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult>;

    /// Start the process and return the child handle
    async fn start(&self, spec: &AppSpec) -> Result<RunningProcess>;

    /// Get the command that will be executed (for display)
    fn command_string(&self, spec: &AppSpec) -> String;

    /// Get the mode name
    fn mode_name(&self) -> &'static str;
}
