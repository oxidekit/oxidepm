//! Daemon-level settings: `{home}/daemon.toml`.
//!
//! The file is optional. Parsing is strict (an unknown key is an error that names it) and
//! a value outside its range is refused with the range, so a typo never turns into a silent
//! default.

use std::ops::RangeInclusive;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Seconds the daemon spends stopping its apps on shutdown, unless `daemon.toml` says otherwise.
pub const DEFAULT_SHUTDOWN_BUDGET_SECS: u64 = 120;

/// Accepted values of `shutdown_budget_secs`.
pub const SHUTDOWN_BUDGET_RANGE: RangeInclusive<u64> = 10..=900;

/// Apps whose build and spawn may run at the same time unless `daemon.toml` says otherwise.
pub const DEFAULT_LAUNCH_CONCURRENCY: usize = 4;

/// Accepted values of `launch_concurrency`.
pub const LAUNCH_CONCURRENCY_RANGE: RangeInclusive<usize> = 1..=64;

/// What `daemon.toml` may contain.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDaemonConfig {
    shutdown_budget_secs: Option<u64>,
    launch_concurrency: Option<usize>,
    agent_socket: Option<bool>,
    reuse_plan: Option<bool>,
}

/// Daemon settings with defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Time budget for stopping all apps on shutdown (the apps are stopped in dependency
    /// layers; the budget is shared out between the layers). Range 10..=900 seconds.
    pub shutdown_budget_secs: u64,
    /// How many apps may be in their build-and-spawn phase at once, so that a reboot does not
    /// run twenty builds together. An app that waits for its dependencies holds no slot.
    /// Range 1..=64.
    pub launch_concurrency: usize,
    /// Bind `{home}/agent.sock`, the socket whose every connection is class `ai`. Default true.
    pub agent_socket: bool,
    /// After a reboot, an app of a runtime adapter that cannot be asked may start from the plan
    /// the adapter made for it earlier. Default true.
    pub reuse_plan: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            shutdown_budget_secs: DEFAULT_SHUTDOWN_BUDGET_SECS,
            launch_concurrency: DEFAULT_LAUNCH_CONCURRENCY,
            agent_socket: true,
            reuse_plan: true,
        }
    }
}

impl DaemonConfig {
    /// Read `path`; a missing file means all defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| match e {
                Error::ConfigError(message) => {
                    Error::ConfigError(format!("{}: {}", path.display(), message))
                }
                other => other,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::IoError(e)),
        }
    }

    /// Parse the text of a `daemon.toml`.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawDaemonConfig =
            toml::from_str(text).map_err(|e| Error::ConfigError(e.to_string()))?;
        let shutdown_budget_secs = raw
            .shutdown_budget_secs
            .unwrap_or(DEFAULT_SHUTDOWN_BUDGET_SECS);
        if !SHUTDOWN_BUDGET_RANGE.contains(&shutdown_budget_secs) {
            return Err(Error::ConfigError(format!(
                "shutdown_budget_secs is {}, it must be between {} and {}",
                shutdown_budget_secs,
                SHUTDOWN_BUDGET_RANGE.start(),
                SHUTDOWN_BUDGET_RANGE.end()
            )));
        }
        let launch_concurrency = raw.launch_concurrency.unwrap_or(DEFAULT_LAUNCH_CONCURRENCY);
        if !LAUNCH_CONCURRENCY_RANGE.contains(&launch_concurrency) {
            return Err(Error::ConfigError(format!(
                "launch_concurrency is {}, it must be between {} and {}",
                launch_concurrency,
                LAUNCH_CONCURRENCY_RANGE.start(),
                LAUNCH_CONCURRENCY_RANGE.end()
            )));
        }
        Ok(Self {
            shutdown_budget_secs,
            launch_concurrency,
            agent_socket: raw.agent_socket.unwrap_or(true),
            reuse_plan: raw.reuse_plan.unwrap_or(true),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(DaemonConfig::parse("").unwrap(), DaemonConfig::default());
        assert_eq!(DaemonConfig::default().shutdown_budget_secs, 120);
    }

    #[test]
    fn reuse_plan_is_on_by_default_and_can_be_turned_off() {
        assert!(DaemonConfig::default().reuse_plan);
        assert!(DaemonConfig::parse("").unwrap().reuse_plan);
        assert!(
            !DaemonConfig::parse("reuse_plan = false")
                .unwrap()
                .reuse_plan
        );
        assert!(DaemonConfig::parse("reuse_plan = 3").is_err());
    }

    #[test]
    fn agent_socket_is_on_by_default_and_can_be_turned_off() {
        assert!(DaemonConfig::default().agent_socket);
        assert!(DaemonConfig::parse("").unwrap().agent_socket);
        assert!(
            DaemonConfig::parse("agent_socket = true")
                .unwrap()
                .agent_socket
        );
        assert!(
            !DaemonConfig::parse("agent_socket = false")
                .unwrap()
                .agent_socket
        );
        let err = DaemonConfig::parse("agent_socket = \"yes\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent_socket"), "{err}");
    }

    #[test]
    fn budget_is_read() {
        assert_eq!(
            DaemonConfig::parse("shutdown_budget_secs = 45")
                .unwrap()
                .shutdown_budget_secs,
            45
        );
    }

    #[test]
    fn budget_range_is_enforced_at_both_ends() {
        for ok in [10, 900] {
            DaemonConfig::parse(&format!("shutdown_budget_secs = {ok}")).unwrap();
        }
        for bad in [0, 9, 901, 100000] {
            let err = DaemonConfig::parse(&format!("shutdown_budget_secs = {bad}")).unwrap_err();
            assert!(err.to_string().contains("between 10 and 900"), "{err}");
        }
    }

    #[test]
    fn launch_concurrency_defaults_to_four_and_is_range_checked() {
        assert_eq!(DaemonConfig::default().launch_concurrency, 4);
        assert_eq!(
            DaemonConfig::parse("launch_concurrency = 9")
                .unwrap()
                .launch_concurrency,
            9
        );
        for ok in [1, 64] {
            DaemonConfig::parse(&format!("launch_concurrency = {ok}")).unwrap();
        }
        for bad in [0, 65] {
            let err = DaemonConfig::parse(&format!("launch_concurrency = {bad}")).unwrap_err();
            assert!(err.to_string().contains("between 1 and 64"), "{err}");
        }
    }

    #[test]
    fn unknown_keys_are_errors_that_name_them() {
        let err = DaemonConfig::parse("shutdown_budget = 30").unwrap_err();
        assert!(err.to_string().contains("shutdown_budget"), "{err}");
    }

    #[test]
    fn missing_file_is_defaults_and_a_bad_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        assert_eq!(DaemonConfig::load(&path).unwrap(), DaemonConfig::default());
        std::fs::write(&path, "shutdown_budget_secs = 5").unwrap();
        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }
}
