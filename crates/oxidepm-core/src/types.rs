//! Core types for OxidePM

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use crate::constants::*;
use crate::error::{Error, Result};
use once_cell::sync::Lazy;
use regex::Regex;

// Default value functions for serde
fn default_instances() -> u32 {
    1
}

fn default_kill_timeout() -> u64 {
    DEFAULT_KILL_TIMEOUT_MS
}

fn default_ignore_patterns() -> Vec<String> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_stopped_status() -> AppStatus {
    AppStatus::Stopped
}

/// Regex pattern for valid app names: only alphanumeric, underscore, and hyphen
static APP_NAME_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid app name regex")
});

/// Validate an app name to prevent path traversal attacks
/// Only allows alphanumeric characters, underscores, and hyphens
pub fn validate_app_name(name: &str) -> bool {
    !name.is_empty() && APP_NAME_REGEX.is_match(name)
}

/// Event hooks configuration - scripts to run when process events occur
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hooks {
    /// Script to run after process starts
    pub on_start: Option<String>,
    /// Script to run after process stops
    pub on_stop: Option<String>,
    /// Script to run after restart
    pub on_restart: Option<String>,
    /// Script to run when process crashes
    pub on_crash: Option<String>,
    /// Script to run on error
    pub on_error: Option<String>,
}

impl Hooks {
    /// Check if any hooks are configured
    pub fn is_empty(&self) -> bool {
        self.on_start.is_none()
            && self.on_stop.is_none()
            && self.on_restart.is_none()
            && self.on_crash.is_none()
            && self.on_error.is_none()
    }

    /// Get the hook script for a specific event
    pub fn get(&self, event: HookEvent) -> Option<&str> {
        match event {
            HookEvent::Start => self.on_start.as_deref(),
            HookEvent::Stop => self.on_stop.as_deref(),
            HookEvent::Restart => self.on_restart.as_deref(),
            HookEvent::Crash => self.on_crash.as_deref(),
            HookEvent::Error => self.on_error.as_deref(),
        }
    }
}

/// Hook event types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Start,
    Stop,
    Restart,
    Crash,
    Error,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::Start => "start",
            HookEvent::Stop => "stop",
            HookEvent::Restart => "restart",
            HookEvent::Crash => "crash",
            HookEvent::Error => "error",
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// HTTP endpoint to check (e.g., "http://localhost:3000/health")
    pub http_url: Option<String>,
    /// Script to execute for health check
    pub script: Option<String>,
    /// Expected HTTP status codes (default: 200)
    pub expected_status: Vec<u16>,
    /// Interval between checks in seconds
    pub interval_secs: u64,
    /// Timeout for each check in seconds
    pub timeout_secs: u64,
    /// Number of consecutive failures before marking unhealthy
    pub retries: u32,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            http_url: None,
            script: None,
            expected_status: vec![200],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
        }
    }
}

impl HealthCheck {
    pub fn http(url: impl Into<String>) -> Self {
        Self {
            http_url: Some(url.into()),
            ..Default::default()
        }
    }

    pub fn script(cmd: impl Into<String>) -> Self {
        Self {
            script: Some(cmd.into()),
            ..Default::default()
        }
    }
}

/// Application specification - defines how to run a process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSpec {
    pub id: u32,
    pub name: String,
    pub mode: AppMode,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub watch: bool,
    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    #[serde(default = "default_kill_timeout")]
    pub kill_timeout_ms: u64,
    pub created_at: DateTime<Utc>,
    // Clustering
    #[serde(default = "default_instances")]
    pub instances: u32,
    #[serde(default)]
    pub instance_id: Option<u32>,
    // Port management
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub port_range: Option<(u16, u16)>,
    // Health checks
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    // Memory limit (auto-restart if exceeded)
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    // Startup delay in milliseconds (wait before starting)
    #[serde(default)]
    pub startup_delay_ms: Option<u64>,
    // Inherit environment from parent process
    #[serde(default)]
    pub env_inherit: bool,
    // Event hooks
    #[serde(default)]
    pub hooks: Hooks,
    // Process tags for grouping (use @tag selector syntax)
    #[serde(default)]
    pub tags: Vec<String>,
    // Maximum uptime in seconds before auto-restart (prevents memory leaks)
    #[serde(default)]
    pub max_uptime_secs: Option<u64>,
    // Cosmos-specific configuration (only used when mode = Cosmos)
    #[serde(default)]
    pub cosmos_config: Option<CosmosConfig>,
    // Restart mode (on-crash, always, never)
    #[serde(default)]
    pub restart_mode: RestartMode,
    // Log rotation: max file size in bytes (default: 10MB)
    #[serde(default)]
    pub log_max_size: Option<u64>,
    // Log rotation: max rotated files to keep (default: 5)
    #[serde(default)]
    pub log_max_files: Option<usize>,
    // Log rotation: gzip compress rotated files (default: false)
    #[serde(default)]
    pub log_compress: bool,
}

impl AppSpec {
    /// Create a new AppSpec. Panics if the name contains invalid characters.
    /// Use `try_new` for a fallible version.
    pub fn new(name: String, mode: AppMode, command: String, cwd: PathBuf) -> Self {
        Self::try_new(name, mode, command, cwd)
            .expect("Invalid app name: only alphanumeric characters, underscores, and hyphens are allowed")
    }

    /// Create a new AppSpec, validating the app name.
    /// Returns an error if the name contains invalid characters.
    pub fn try_new(name: String, mode: AppMode, command: String, cwd: PathBuf) -> Result<Self> {
        if !validate_app_name(&name) {
            return Err(Error::ConfigError(format!(
                "Invalid app name '{}': only alphanumeric characters, underscores, and hyphens are allowed",
                name
            )));
        }

        Ok(Self {
            id: 0,
            name,
            mode,
            command,
            args: Vec::new(),
            cwd,
            env: HashMap::new(),
            watch: false,
            ignore_patterns: DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            restart_policy: RestartPolicy::default(),
            kill_timeout_ms: DEFAULT_KILL_TIMEOUT_MS,
            created_at: Utc::now(),
            instances: 1,
            instance_id: None,
            port: None,
            port_range: None,
            health_check: None,
            max_memory_mb: None,
            startup_delay_ms: None,
            env_inherit: false,
            hooks: Hooks::default(),
            tags: Vec::new(),
            max_uptime_secs: None,
            cosmos_config: None,
            restart_mode: RestartMode::default(),
            log_max_size: None,
            log_max_files: None,
            log_compress: false,
        })
    }

    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_instances(mut self, instances: u32) -> Self {
        self.instances = instances;
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_port_range(mut self, start: u16, end: u16) -> Self {
        self.port_range = Some((start, end));
        self
    }

    pub fn with_health_check(mut self, health_check: HealthCheck) -> Self {
        self.health_check = Some(health_check);
        self
    }

    pub fn with_max_memory(mut self, max_mb: u64) -> Self {
        self.max_memory_mb = Some(max_mb);
        self
    }

    pub fn with_startup_delay(mut self, delay_ms: u64) -> Self {
        self.startup_delay_ms = Some(delay_ms);
        self
    }

    pub fn with_env_inherit(mut self, inherit: bool) -> Self {
        self.env_inherit = inherit;
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_max_uptime(mut self, secs: u64) -> Self {
        self.max_uptime_secs = Some(secs);
        self
    }

    /// Create a clone for a specific instance in a cluster
    pub fn for_instance(&self, instance_id: u32, port: Option<u16>) -> Self {
        let mut instance = self.clone();
        instance.instance_id = Some(instance_id);
        instance.name = format!("{}-{}", self.name, instance_id);
        if let Some(p) = port {
            instance.port = Some(p);
            instance.env.insert("PORT".to_string(), p.to_string());
        }
        instance
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_watch(mut self, watch: bool) -> Self {
        self.watch = watch;
        self
    }

    pub fn with_ignore_patterns(mut self, patterns: Vec<String>) -> Self {
        self.ignore_patterns = patterns;
        self
    }
}

/// Cosmos node operating mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CosmosNodeMode {
    /// Full validator node (signs blocks)
    Validator,
    /// Seed node (peer discovery only)
    Seed,
    /// Relay node (no signing)
    Relay,
    /// Sentry node (shields validator from public network)
    Sentry,
    /// Archive node (full history, pruning = "nothing")
    Archive,
}

impl CosmosNodeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CosmosNodeMode::Validator => "validator",
            CosmosNodeMode::Seed => "seed",
            CosmosNodeMode::Relay => "relay",
            CosmosNodeMode::Sentry => "sentry",
            CosmosNodeMode::Archive => "archive",
        }
    }
}

impl FromStr for CosmosNodeMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "validator" => Ok(CosmosNodeMode::Validator),
            "seed" => Ok(CosmosNodeMode::Seed),
            "relay" => Ok(CosmosNodeMode::Relay),
            "sentry" => Ok(CosmosNodeMode::Sentry),
            "archive" => Ok(CosmosNodeMode::Archive),
            _ => Err(Error::ConfigError(format!("Invalid cosmos node mode: {}", s))),
        }
    }
}

impl std::fmt::Display for CosmosNodeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Cosmos-specific configuration for blockchain node management
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CosmosConfig {
    /// Node operating mode
    pub node_mode: CosmosNodeMode,
    /// RPC endpoint for health checks (e.g., "http://localhost:26657")
    pub rpc_endpoint: String,
    /// Chain ID (e.g., "mono_6940-1")
    pub chain_id: String,
    /// Start as relay, auto-enable validator key when synced
    pub relay_until_synced: bool,
    /// Path to priv_validator_key.json (oxidepm only renames, NEVER reads contents)
    pub validator_key_path: Option<PathBuf>,
    /// RLIMIT_NOFILE value for the node process
    pub nofile_limit: Option<u64>,
    /// Graceful shutdown timeout in seconds
    pub shutdown_timeout: u32,
    /// Detect x/upgrade halt messages in stdout/stderr (default: true)
    pub detect_upgrade_halt: bool,
    /// Auto-upgrade: download and swap binary on upgrade halt (default: false)
    pub auto_upgrade: bool,
    /// Trusted GitHub repo for auto-upgrade downloads (e.g., "monolythium/mono-chain")
    pub auto_upgrade_source: Option<String>,
    /// Require SHA256 checksum verification for auto-upgrade binaries (default: true)
    pub auto_upgrade_require_checksum: bool,
}

impl Default for CosmosConfig {
    fn default() -> Self {
        Self {
            node_mode: CosmosNodeMode::Relay,
            rpc_endpoint: "http://localhost:26657".to_string(),
            chain_id: String::new(),
            relay_until_synced: false,
            validator_key_path: None,
            nofile_limit: None,
            shutdown_timeout: 30,
            detect_upgrade_halt: true,
            auto_upgrade: false,
            auto_upgrade_source: None,
            auto_upgrade_require_checksum: true,
        }
    }
}

/// Restart mode for process crash recovery
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartMode {
    /// Restart on non-zero exit code
    OnCrash,
    /// Always restart regardless of exit code
    Always,
    /// Never auto-restart
    Never,
}

impl Default for RestartMode {
    fn default() -> Self {
        RestartMode::OnCrash
    }
}

impl RestartMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RestartMode::OnCrash => "on-crash",
            RestartMode::Always => "always",
            RestartMode::Never => "never",
        }
    }

    pub fn should_restart(&self, exit_code: Option<i32>) -> bool {
        match self {
            RestartMode::Never => false,
            RestartMode::Always => true,
            RestartMode::OnCrash => exit_code.map(|c| c != 0).unwrap_or(true),
        }
    }
}

impl FromStr for RestartMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "on-crash" | "on_crash" => Ok(RestartMode::OnCrash),
            "always" => Ok(RestartMode::Always),
            "never" => Ok(RestartMode::Never),
            _ => Err(Error::ConfigError(format!("Invalid restart mode: {}", s))),
        }
    }
}

impl std::fmt::Display for RestartMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Application runtime mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AppMode {
    Cmd,
    Node,
    Npm,
    Pnpm,
    Yarn,
    Cargo,
    Rust,
    Cosmos,
}

impl AppMode {
    /// Detect mode from file extension
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(AppMode::Rust),
            "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" => Some(AppMode::Node),
            _ => None,
        }
    }

    /// Detect mode from path
    pub fn detect(path: &std::path::Path) -> Option<Self> {
        if path.is_dir() {
            // Check for Cargo.toml
            if path.join("Cargo.toml").exists() {
                return Some(AppMode::Cargo);
            }
            // Check for package.json
            if path.join("package.json").exists() {
                return Some(AppMode::Npm);
            }
            return None;
        }

        // Check extension
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AppMode::Cmd => "cmd",
            AppMode::Node => "node",
            AppMode::Npm => "npm",
            AppMode::Pnpm => "pnpm",
            AppMode::Yarn => "yarn",
            AppMode::Cargo => "cargo",
            AppMode::Rust => "rust",
            AppMode::Cosmos => "cosmos",
        }
    }
}

impl FromStr for AppMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "cmd" => Ok(AppMode::Cmd),
            "node" => Ok(AppMode::Node),
            "npm" => Ok(AppMode::Npm),
            "pnpm" => Ok(AppMode::Pnpm),
            "yarn" => Ok(AppMode::Yarn),
            "cargo" => Ok(AppMode::Cargo),
            "rust" => Ok(AppMode::Rust),
            "cosmos" => Ok(AppMode::Cosmos),
            _ => Err(Error::InvalidMode(s.to_string())),
        }
    }
}

impl std::fmt::Display for AppMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Application status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Errored,
    Building,
    /// Cosmos node halted for a planned upgrade (not a crash)
    UpgradeHalted,
}

impl AppStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppStatus::Starting => "starting",
            AppStatus::Running => "running",
            AppStatus::Stopping => "stopping",
            AppStatus::Stopped => "stopped",
            AppStatus::Errored => "errored",
            AppStatus::Building => "building",
            AppStatus::UpgradeHalted => "upgrade_halted",
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, AppStatus::Running | AppStatus::Starting | AppStatus::Building)
    }
}

impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for AppStatus {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "starting" => Ok(AppStatus::Starting),
            "running" => Ok(AppStatus::Running),
            "stopping" => Ok(AppStatus::Stopping),
            "stopped" => Ok(AppStatus::Stopped),
            "errored" => Ok(AppStatus::Errored),
            "building" => Ok(AppStatus::Building),
            "upgrade_halted" => Ok(AppStatus::UpgradeHalted),
            _ => Err(Error::ConfigError(format!("Invalid status: {}", s))),
        }
    }
}

/// Runtime state of an application
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub app_id: u32,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default = "default_stopped_status")]
    pub status: AppStatus,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub cpu_percent: f32,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    // Health check status
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub last_health_check: Option<DateTime<Utc>>,
    #[serde(default)]
    pub health_check_failures: u32,
    // Port info
    #[serde(default)]
    pub port: Option<u16>,
    // Instance info for clusters
    #[serde(default)]
    pub instance_id: Option<u32>,
    // Cosmos upgrade halt info (populated when status = UpgradeHalted)
    #[serde(default)]
    pub upgrade_name: Option<String>,
    #[serde(default)]
    pub upgrade_halt_height: Option<u64>,
}

impl RunState {
    pub fn new(app_id: u32) -> Self {
        Self {
            app_id,
            pid: None,
            status: AppStatus::Stopped,
            restarts: 0,
            uptime_secs: 0,
            cpu_percent: 0.0,
            memory_bytes: 0,
            last_exit_code: None,
            started_at: None,
            healthy: false,
            last_health_check: None,
            health_check_failures: 0,
            port: None,
            instance_id: None,
            upgrade_name: None,
            upgrade_halt_height: None,
        }
    }

    pub fn running(app_id: u32, pid: u32) -> Self {
        Self {
            app_id,
            pid: Some(pid),
            status: AppStatus::Running,
            restarts: 0,
            uptime_secs: 0,
            cpu_percent: 0.0,
            memory_bytes: 0,
            last_exit_code: None,
            started_at: Some(Utc::now()),
            healthy: true,
            last_health_check: None,
            health_check_failures: 0,
            port: None,
            instance_id: None,
            upgrade_name: None,
            upgrade_halt_height: None,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_instance(mut self, instance_id: u32) -> Self {
        self.instance_id = Some(instance_id);
        self
    }
}

/// Restart policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub auto_restart: bool,
    pub max_restarts: u32,
    pub restart_delay_ms: u64,
    pub crash_window_secs: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            auto_restart: true,
            max_restarts: DEFAULT_MAX_RESTARTS,
            restart_delay_ms: DEFAULT_RESTART_DELAY_MS,
            crash_window_secs: DEFAULT_CRASH_WINDOW_SECS,
        }
    }
}

/// Selector for targeting apps by id, name, tag, or all
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Selector {
    All,
    ById(u32),
    ByName(String),
    /// Select by tag using @tagname syntax
    ByTag(String),
}

impl Selector {
    /// Parse a selector string
    /// - "all" -> All
    /// - "123" -> ById(123)
    /// - "@tagname" -> ByTag("tagname")
    /// - "appname" -> ByName("appname")
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("all") {
            Selector::All
        } else if let Some(tag) = s.strip_prefix('@') {
            Selector::ByTag(tag.to_string())
        } else if let Ok(id) = s.parse::<u32>() {
            Selector::ById(id)
        } else {
            Selector::ByName(s.to_string())
        }
    }

    /// Check if this selector matches an app spec
    pub fn matches(&self, spec: &AppSpec) -> bool {
        match self {
            Selector::All => true,
            Selector::ById(id) => spec.id == *id,
            Selector::ByName(name) => spec.name == *name,
            Selector::ByTag(tag) => spec.tags.contains(tag),
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Selector::All => write!(f, "all"),
            Selector::ById(id) => write!(f, "{}", id),
            Selector::ByName(name) => write!(f, "{}", name),
            Selector::ByTag(tag) => write!(f, "@{}", tag),
        }
    }
}

/// Full application info (spec + state) for status display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub spec: AppSpec,
    pub state: RunState,
}

impl AppInfo {
    pub fn new(spec: AppSpec, state: RunState) -> Self {
        Self { spec, state }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_mode_from_extension() {
        assert_eq!(AppMode::from_extension("rs"), Some(AppMode::Rust));
        assert_eq!(AppMode::from_extension("js"), Some(AppMode::Node));
        assert_eq!(AppMode::from_extension("mjs"), Some(AppMode::Node));
        assert_eq!(AppMode::from_extension("ts"), Some(AppMode::Node));
        assert_eq!(AppMode::from_extension("py"), None);
    }

    #[test]
    fn test_app_mode_from_str() {
        assert_eq!("cargo".parse::<AppMode>().unwrap(), AppMode::Cargo);
        assert_eq!("node".parse::<AppMode>().unwrap(), AppMode::Node);
        assert!("invalid".parse::<AppMode>().is_err());
    }

    #[test]
    fn test_restart_policy_default() {
        let policy = RestartPolicy::default();
        assert!(policy.auto_restart);
        assert_eq!(policy.max_restarts, DEFAULT_MAX_RESTARTS);
        assert_eq!(policy.restart_delay_ms, DEFAULT_RESTART_DELAY_MS);
    }

    #[test]
    fn test_selector_parse() {
        assert_eq!(Selector::parse("all"), Selector::All);
        assert_eq!(Selector::parse("ALL"), Selector::All);
        assert_eq!(Selector::parse("123"), Selector::ById(123));
        assert_eq!(Selector::parse("myapp"), Selector::ByName("myapp".to_string()));
        assert_eq!(Selector::parse("@production"), Selector::ByTag("production".to_string()));
        assert_eq!(Selector::parse("@web-servers"), Selector::ByTag("web-servers".to_string()));
    }

    #[test]
    fn test_selector_matches() {
        let spec = AppSpec::new(
            "test-app".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_tags(vec!["production".to_string(), "web".to_string()]);

        // Create a spec with a specific ID for testing
        let mut spec_with_id = spec.clone();
        spec_with_id.id = 42;

        assert!(Selector::All.matches(&spec_with_id));
        assert!(Selector::ById(42).matches(&spec_with_id));
        assert!(!Selector::ById(99).matches(&spec_with_id));
        assert!(Selector::ByName("test-app".to_string()).matches(&spec_with_id));
        assert!(!Selector::ByName("other-app".to_string()).matches(&spec_with_id));
        assert!(Selector::ByTag("production".to_string()).matches(&spec_with_id));
        assert!(Selector::ByTag("web".to_string()).matches(&spec_with_id));
        assert!(!Selector::ByTag("staging".to_string()).matches(&spec_with_id));
    }

    #[test]
    fn test_selector_display() {
        assert_eq!(Selector::All.to_string(), "all");
        assert_eq!(Selector::ById(123).to_string(), "123");
        assert_eq!(Selector::ByName("myapp".to_string()).to_string(), "myapp");
        assert_eq!(Selector::ByTag("production".to_string()).to_string(), "@production");
    }

    #[test]
    fn test_app_spec_with_tags() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_tags(vec!["web".to_string(), "production".to_string()]);

        assert_eq!(spec.tags.len(), 2);
        assert!(spec.tags.contains(&"web".to_string()));
        assert!(spec.tags.contains(&"production".to_string()));
    }

    #[test]
    fn test_app_spec_with_max_uptime() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_max_uptime(86400); // 24 hours

        assert_eq!(spec.max_uptime_secs, Some(86400));
    }

    #[test]
    fn test_app_status_is_running() {
        assert!(AppStatus::Running.is_running());
        assert!(AppStatus::Starting.is_running());
        assert!(!AppStatus::Stopped.is_running());
        assert!(!AppStatus::Errored.is_running());
    }

    #[test]
    fn test_app_spec_builder() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_watch(true)
        .with_args(vec!["--port".to_string(), "3000".to_string()]);

        assert_eq!(spec.name, "test");
        assert!(spec.watch);
        assert_eq!(spec.args.len(), 2);
    }

    #[test]
    fn test_hooks_default() {
        let hooks = Hooks::default();
        assert!(hooks.is_empty());
        assert!(hooks.on_start.is_none());
        assert!(hooks.on_stop.is_none());
        assert!(hooks.on_restart.is_none());
        assert!(hooks.on_crash.is_none());
        assert!(hooks.on_error.is_none());
    }

    #[test]
    fn test_hooks_is_empty() {
        let mut hooks = Hooks::default();
        assert!(hooks.is_empty());

        hooks.on_start = Some("echo 'started'".to_string());
        assert!(!hooks.is_empty());
    }

    #[test]
    fn test_hooks_get() {
        let hooks = Hooks {
            on_start: Some("start.sh".to_string()),
            on_stop: Some("stop.sh".to_string()),
            on_restart: None,
            on_crash: Some("crash.sh".to_string()),
            on_error: None,
        };

        assert_eq!(hooks.get(HookEvent::Start), Some("start.sh"));
        assert_eq!(hooks.get(HookEvent::Stop), Some("stop.sh"));
        assert_eq!(hooks.get(HookEvent::Restart), None);
        assert_eq!(hooks.get(HookEvent::Crash), Some("crash.sh"));
        assert_eq!(hooks.get(HookEvent::Error), None);
    }

    #[test]
    fn test_hook_event_as_str() {
        assert_eq!(HookEvent::Start.as_str(), "start");
        assert_eq!(HookEvent::Stop.as_str(), "stop");
        assert_eq!(HookEvent::Restart.as_str(), "restart");
        assert_eq!(HookEvent::Crash.as_str(), "crash");
        assert_eq!(HookEvent::Error.as_str(), "error");
    }

    #[test]
    fn test_app_spec_with_hooks() {
        let hooks = Hooks {
            on_start: Some("echo 'started'".to_string()),
            on_crash: Some("/scripts/notify.sh".to_string()),
            ..Default::default()
        };

        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_hooks(hooks);

        assert_eq!(spec.hooks.on_start, Some("echo 'started'".to_string()));
        assert_eq!(spec.hooks.on_crash, Some("/scripts/notify.sh".to_string()));
        assert!(spec.hooks.on_stop.is_none());
    }

    #[test]
    fn test_app_spec_with_startup_delay() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_startup_delay(5000);

        assert_eq!(spec.startup_delay_ms, Some(5000));
    }

    #[test]
    fn test_app_spec_default_no_startup_delay() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        );

        assert_eq!(spec.startup_delay_ms, None);
        assert!(!spec.env_inherit);
    }

    #[test]
    fn test_app_spec_with_env_inherit() {
        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_env_inherit(true);

        assert!(spec.env_inherit);
    }

    #[test]
    fn test_validate_app_name_valid() {
        assert!(validate_app_name("myapp"));
        assert!(validate_app_name("my-app"));
        assert!(validate_app_name("my_app"));
        assert!(validate_app_name("MyApp123"));
        assert!(validate_app_name("app-123_test"));
    }

    #[test]
    fn test_validate_app_name_invalid() {
        assert!(!validate_app_name(""));
        assert!(!validate_app_name("../etc/passwd"));
        assert!(!validate_app_name("my app"));
        assert!(!validate_app_name("my.app"));
        assert!(!validate_app_name("my/app"));
        assert!(!validate_app_name("my\\app"));
        assert!(!validate_app_name("app@server"));
        assert!(!validate_app_name("app:8080"));
    }

    #[test]
    fn test_app_spec_try_new_valid() {
        let result = AppSpec::try_new(
            "valid-app".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_app_spec_try_new_invalid() {
        let result = AppSpec::try_new(
            "../evil".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        );
        assert!(result.is_err());
    }
}
