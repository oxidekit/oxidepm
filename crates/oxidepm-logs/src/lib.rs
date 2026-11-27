#![forbid(unsafe_code)]

//! OxidePM Logs - Log management, rotation, and streaming

mod lines;
mod reader;
mod rotation;
mod writer;

pub use reader::{tail_streams, LogFollower, LogLine, LogReader, LogStream};
pub use rotation::RotationConfig;
pub use writer::{CaptureTasks, LogCapture, LogControl, LogWriter};

use oxidepm_core::constants;
use std::path::PathBuf;

/// Get the stdout log path for an app
pub fn stdout_path(app_name: &str) -> PathBuf {
    constants::log_path(app_name, "out")
}

/// Get the stderr log path for an app
pub fn stderr_path(app_name: &str) -> PathBuf {
    constants::log_path(app_name, "err")
}
