//! Error types for the notification system

use std::path::PathBuf;

/// Notification error type
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Telegram API error: {0}")]
    TelegramError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParseError(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSerializeError(#[from] toml::ser::Error),

    #[error("Notifier not configured")]
    NotConfigured,
}

/// Result type alias for notification operations
pub type Result<T> = std::result::Result<T, NotifyError>;

impl NotifyError {
    pub fn config<S: Into<String>>(msg: S) -> Self {
        NotifyError::ConfigError(msg.into())
    }

    pub fn telegram<S: Into<String>>(msg: S) -> Self {
        NotifyError::TelegramError(msg.into())
    }
}
