//! Error types for OxidePM

use std::path::PathBuf;

/// OxidePM error type
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("App already exists: {0}")]
    AppAlreadyExists(String),

    #[error("Daemon not running")]
    DaemonNotRunning,

    #[error("Daemon already running")]
    DaemonAlreadyRunning,

    #[error("Build failed: {0}")]
    BuildFailed(String),

    #[error("Process failed to start: {0}")]
    ProcessStartFailed(String),

    #[error("Process not running: {0}")]
    ProcessNotRunning(String),

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("IPC error: {0}")]
    IpcError(String),

    #[error("IPC connection failed: {0}")]
    IpcConnectionFailed(String),

    #[error("Database error: {0}")]
    DbError(String),

    #[error("Invalid selector: {0}")]
    InvalidSelector(String),

    #[error("Invalid mode: {0}")]
    InvalidMode(String),

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Health check failed")]
    HealthCheckFailed,

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlError(#[from] toml::de::Error),

    #[error("YAML parse error: {0}")]
    YamlError(#[from] serde_yaml::Error),
}

/// Result type alias for OxidePM
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn config<S: Into<String>>(msg: S) -> Self {
        Error::ConfigError(msg.into())
    }

    pub fn ipc<S: Into<String>>(msg: S) -> Self {
        Error::IpcError(msg.into())
    }

    pub fn db<S: Into<String>>(msg: S) -> Self {
        Error::DbError(msg.into())
    }

    pub fn process_start<S: Into<String>>(msg: S) -> Self {
        Error::ProcessStartFailed(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::AppNotFound("myapp".to_string());
        assert_eq!(err.to_string(), "App not found: myapp");
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::IoError(_)));
    }
}
