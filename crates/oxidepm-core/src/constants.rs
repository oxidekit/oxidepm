//! Constants and default values for OxidePM

use std::path::PathBuf;

use crate::paths::Paths;

/// Default OxidePM home directory name
pub const OXIDEPM_DIR: &str = ".oxidepm";

/// Default socket file name
pub const SOCKET_FILE: &str = "daemon.sock";

/// Agent socket file name: every connection on it is class `ai`, whatever it claims
pub const AGENT_SOCKET_FILE: &str = "agent.sock";

/// Directory of the audit log, inside the home.
pub const AUDIT_DIR: &str = "audit";

/// The file that holds every recorded request (rotated to `audit.1.jsonl` and so on).
pub const AUDIT_LOG_FILE: &str = "audit.jsonl";

/// The file that holds denials, dangerous requests and what non-humans changed.
pub const AUDIT_SECURITY_FILE: &str = "audit-security.jsonl";

/// The adapter database: the installed adapters, their grants and the plan cache. Its own file
/// with its own migrator, so a home with no adapters never has an adapter schema in `state.db`.
pub const ADAPTERS_DB_FILE: &str = "adapters.db";

/// Directory of installed adapters, their state and the cached index, inside the home. Created
/// lazily on the first import or enable, never at daemon start.
pub const ADAPTERS_DIR: &str = "adapters";

/// File with the process groups of the running adapters, inside the home.
pub const ADAPTER_PIDS_FILE: &str = "adapter-pids.json";

/// Database file name (schema v1: the full app spec is stored)
pub const DB_FILE: &str = "state.db";

/// Database file written by releases before schema v1. Never read or migrated;
/// its presence without `state.db` makes the daemon refuse to start.
pub const LEGACY_DB_FILE: &str = "oxidepm.db";

/// Daemon log file (fatal startup errors, auto-started daemon stderr)
pub const DAEMON_LOG_FILE: &str = "daemon.log";

/// Daemon settings file (`shutdown_budget_secs`); optional
pub const DAEMON_CONFIG_FILE: &str = "daemon.toml";

/// Default saved processes file
pub const SAVED_FILE: &str = "saved.json";

/// Default daemon lock file name
pub const LOCK_FILE: &str = "oxidepmd.lock";

/// Exit code of an `oxidepmd` that found another daemon already running on its home.
pub const EXIT_ALREADY_RUNNING: i32 = 3;

/// Default file that records live app processes
pub const PIDS_FILE: &str = "pids.json";

/// Environment variable that overrides the OxidePM home directory
pub const OXIDEPM_HOME_ENV: &str = "OXIDEPM_HOME";

/// Environment variable that overrides the socket the CLI talks to (for example the agent
/// socket); the daemon ignores it
pub const OXIDEPM_SOCKET_ENV: &str = "OXIDEPM_SOCKET";

/// Environment variable with the actor claim the CLI sends (`human:<label>` or `ai:<label>`)
pub const OXIDEPM_ACTOR_ENV: &str = "OXIDEPM_ACTOR";

/// Default log directory name
pub const LOGS_DIR: &str = "logs";

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

/// Default restart delay in milliseconds
pub const DEFAULT_RESTART_DELAY_MS: u64 = 500;

/// Default max restarts before errored state
pub const DEFAULT_MAX_RESTARTS: u32 = 15;

/// Default kill timeout in milliseconds
pub const DEFAULT_KILL_TIMEOUT_MS: u64 = 3000;

/// Default crash window in seconds: `max_restarts` automatic restarts inside any window of this
/// length are a crash loop. With the backoff capped at `DEFAULT_MAX_RESTART_DELAY_MS`, a shorter
/// window could never hold the default 15 restarts.
pub const DEFAULT_CRASH_WINDOW_SECS: u64 = 600;

/// Default time in seconds an app waits for its `depends_on` apps to be running before it fails
/// closed (`depends_fail_open` turns that into a warning and a launch).
pub const DEFAULT_DEPENDS_TIMEOUT_SECS: u64 = 60;

/// Default cap of the restart backoff in milliseconds
pub const DEFAULT_MAX_RESTART_DELAY_MS: u64 = 60_000;

/// Default stable uptime in seconds: a run this long starts the backoff over
pub const DEFAULT_STABLE_UPTIME_SECS: u64 = 60;

/// Default log max size in bytes (10MB)
pub const DEFAULT_LOG_MAX_SIZE: u64 = 10 * 1024 * 1024;

/// Default max log files to keep
pub const DEFAULT_LOG_MAX_FILES: usize = 5;

/// Default metrics polling interval in seconds
pub const DEFAULT_METRICS_INTERVAL_SECS: u64 = 2;

/// The paths for this process's environment. These functions keep an
/// infallible signature for the CLI; when no home directory can be found
/// (`OXIDEPM_HOME` unset and no `HOME`) they report that and exit, because
/// nothing can work without one.
fn env_paths() -> Paths {
    Paths::from_env().unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1)
    })
}

/// Get the OxidePM home directory
pub fn oxidepm_home() -> PathBuf {
    env_paths().home().to_path_buf()
}

/// Get the socket path the CLI talks to: `OXIDEPM_SOCKET` if it is set and not empty, else the
/// daemon socket of the home
pub fn socket_path() -> PathBuf {
    match std::env::var_os(OXIDEPM_SOCKET_ENV).filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => env_paths().socket(),
    }
}

/// The actor claim in `OXIDEPM_ACTOR` (unset and empty mean no claim)
pub fn actor_claim() -> Option<String> {
    std::env::var(OXIDEPM_ACTOR_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Get log file path for an app
pub fn log_path(app_name: &str, stream: &str) -> PathBuf {
    env_paths().log_path(app_name, stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oxidepm_home() {
        let expected = std::env::var_os(OXIDEPM_HOME_ENV)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap().join(OXIDEPM_DIR));
        assert_eq!(oxidepm_home(), expected);
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
