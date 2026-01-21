//! OxidePM Logs - Log management, rotation, and streaming

mod reader;
mod rotation;
mod writer;

pub use reader::LogReader;
pub use rotation::RotationConfig;
pub use writer::{LogCapture, LogWriter};

use oxidepm_core::{constants, Result};
use std::path::PathBuf;

/// Get the log directory for an app
pub fn log_dir() -> PathBuf {
    constants::logs_dir()
}

/// Get the stdout log path for an app
pub fn stdout_path(app_name: &str) -> PathBuf {
    constants::log_path(app_name, "out")
}

/// Get the stderr log path for an app
pub fn stderr_path(app_name: &str) -> PathBuf {
    constants::log_path(app_name, "err")
}

/// Ensure log directory exists
pub fn ensure_log_dir() -> Result<()> {
    let dir = log_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}
