//! Constants and default values for OxidePM

use std::path::PathBuf;

/// Default OxidePM home directory name
pub const OXIDEPM_DIR: &str = ".oxidepm";

/// Default socket file name
pub const SOCKET_FILE: &str = "daemon.sock";

/// Default database file name
pub const DB_FILE: &str = "oxidepm.db";

/// Default saved processes file
pub const SAVED_FILE: &str = "saved.json";

/// Default log directory name
pub const LOGS_DIR: &str = "logs";

/// Default repos directory name (for --git clones)
pub const REPOS_DIR: &str = "repos";

/// Default config file names to search for (in priority order)
pub const CONFIG_FILES: &[&str] = &[
    // TOML formats
    "oxidepm.config.toml",
    "oxidepm.toml",
    "ecosystem.config.toml",
    // YAML formats
    "oxidepm.config.yaml",
    "oxidepm.config.yml",
    "oxidepm.yaml",
    "oxidepm.yml",
    "ecosystem.config.yaml",
    "ecosystem.config.yml",
    // JSON formats
    "oxidepm.config.json",
    "oxidepm.json",
    "ecosystem.config.json",
];

/// Default ignore patterns for watch mode
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".oxidepm",
    "*.swp",
    "*.swo",
    ".DS_Store",
];

/// Default restart delay in milliseconds
pub const DEFAULT_RESTART_DELAY_MS: u64 = 500;

/// Default max restarts before errored state
pub const DEFAULT_MAX_RESTARTS: u32 = 15;

/// Default kill timeout in milliseconds
pub const DEFAULT_KILL_TIMEOUT_MS: u64 = 3000;

/// Default crash window in seconds (for crash loop detection)
pub const DEFAULT_CRASH_WINDOW_SECS: u64 = 60;

/// Default log max size in bytes (10MB)
pub const DEFAULT_LOG_MAX_SIZE: u64 = 10 * 1024 * 1024;

/// Default max log files to keep
pub const DEFAULT_LOG_MAX_FILES: usize = 5;

/// Default debounce time for watch mode in milliseconds
pub const DEFAULT_DEBOUNCE_MS: u64 = 200;

/// Default metrics polling interval in seconds
pub const DEFAULT_METRICS_INTERVAL_SECS: u64 = 2;

/// Get the OxidePM home directory
pub fn oxidepm_home() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(OXIDEPM_DIR))
        .unwrap_or_else(|| PathBuf::from(OXIDEPM_DIR))
}

/// Get the socket path
pub fn socket_path() -> PathBuf {
    oxidepm_home().join(SOCKET_FILE)
}

/// Get the database path
pub fn db_path() -> PathBuf {
    oxidepm_home().join(DB_FILE)
}

/// Get the saved file path
pub fn saved_path() -> PathBuf {
    oxidepm_home().join(SAVED_FILE)
}

/// Get the logs directory
pub fn logs_dir() -> PathBuf {
    oxidepm_home().join(LOGS_DIR)
}

/// Get the repos directory (for --git clones)
pub fn repos_dir() -> PathBuf {
    oxidepm_home().join(REPOS_DIR)
}

/// Get log file path for an app
pub fn log_path(app_name: &str, stream: &str) -> PathBuf {
    logs_dir().join(format!("{}-{}.log", app_name, stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oxidepm_home() {
        let home = oxidepm_home();
        assert!(home.to_string_lossy().contains(".oxidepm"));
    }

    #[test]
    fn test_socket_path() {
        let path = socket_path();
        assert!(path.to_string_lossy().contains("daemon.sock"));
    }

    #[test]
    fn test_log_path() {
        let path = log_path("myapp", "out");
        assert!(path.to_string_lossy().contains("myapp-out.log"));
    }
}
