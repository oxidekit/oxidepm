//! Core types for OxidePM

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::str::FromStr;

use crate::constants::*;
use crate::error::{Error, Result};

// Default value functions for serde
fn default_instances() -> u32 {
    1
}

fn default_kill_timeout() -> u64 {
    DEFAULT_KILL_TIMEOUT_MS
}

fn default_depends_timeout() -> u64 {
    DEFAULT_DEPENDS_TIMEOUT_SECS
}

fn default_stopped_status() -> AppStatus {
    AppStatus::Stopped
}

/// Check an app name (see [`crate::validate::check_app_name`] for the rules:
/// `^[A-Za-z0-9_][A-Za-z0-9_-]{0,63}$`, not all digits, not `all`).
pub fn validate_app_name(name: &str) -> bool {
    crate::validate::check_app_name(name).is_ok()
}

/// Event hooks configuration - scripts to run when process events occur
#[derive(Clone, Serialize, Deserialize, Default)]
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

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |script: &Option<String>| script.as_ref().map(|_| crate::redact::SET);
        f.debug_struct("Hooks")
            .field("on_start", &set(&self.on_start))
            .field("on_stop", &set(&self.on_stop))
            .field("on_restart", &set(&self.on_restart))
            .field("on_crash", &set(&self.on_crash))
            .field("on_error", &set(&self.on_error))
            .finish()
    }
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

/// What the supervisor does when a health check fails `retries` times in a row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HealthAction {
    /// Mark the app `unhealthy`, tell the hooks and notifications, leave the child alone.
    #[default]
    Notify,
    /// Replace the child (an automatic restart).
    Restart,
}

impl std::fmt::Display for HealthAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HealthAction::Notify => "notify",
            HealthAction::Restart => "restart",
        })
    }
}

/// Seconds a launched app is given before its first probe.
pub const DEFAULT_HEALTH_START_PERIOD_SECS: u64 = 5;

fn default_health_start_period() -> u64 {
    DEFAULT_HEALTH_START_PERIOD_SECS
}

/// Health check configuration
#[derive(Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// HTTP endpoint to check (e.g., "http://localhost:3000/health")
    pub http_url: Option<String>,
    /// TCP address (`host:port`) that must accept a connection
    #[serde(default)]
    pub tcp: Option<String>,
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
    /// Seconds after the launch before the first probe; the app is `starting` until a probe
    /// has passed.
    #[serde(default = "default_health_start_period")]
    pub start_period_secs: u64,
    /// What to do once the app is unhealthy
    #[serde(default)]
    pub action: HealthAction,
}

impl std::fmt::Debug for HealthCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthCheck")
            .field("http_url", &self.http_url)
            .field("tcp", &self.tcp)
            .field("script", &self.script.as_ref().map(|_| crate::redact::SET))
            .field("expected_status", &self.expected_status)
            .field("interval_secs", &self.interval_secs)
            .field("timeout_secs", &self.timeout_secs)
            .field("retries", &self.retries)
            .field("start_period_secs", &self.start_period_secs)
            .field("action", &self.action)
            .finish()
    }
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            http_url: None,
            tcp: None,
            script: None,
            expected_status: vec![200],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: DEFAULT_HEALTH_START_PERIOD_SECS,
            action: HealthAction::Notify,
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

    pub fn tcp(addr: impl Into<String>) -> Self {
        Self {
            tcp: Some(addr.into()),
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
///
/// `Debug` is written by hand: it prints environment keys but never their values, and shows
/// hook and probe scripts only as set or unset, so a `{:?}` in a log line cannot leak secrets.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    pub id: u32,
    pub name: String,
    pub mode: Runtime,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: HashMap<String, String>,
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
    // Restart mode (on-crash, always, never)
    #[serde(default)]
    pub restart_mode: RestartMode,
    // Process dependencies: wait for these processes to be online before starting
    #[serde(default)]
    pub depends_on: Vec<String>,
    // Seconds to wait for the dependencies to be running before the start fails (1 to 3600)
    #[serde(default = "default_depends_timeout")]
    pub depends_timeout_secs: u64,
    // Discouraged: launch anyway (with a warning) when the dependencies are not running in time
    #[serde(default)]
    pub depends_fail_open: bool,
    // Cron expression for scheduled restarts (e.g., "0 4 * * *" for daily at 4am)
    #[serde(default)]
    pub restart_cron: Option<String>,
    // Log rotation: max file size in bytes (default: 10MB)
    #[serde(default)]
    pub log_max_size: Option<u64>,
    // Log rotation: max rotated files to keep (default: 5)
    #[serde(default)]
    pub log_max_files: Option<usize>,
    // Log rotation: gzip compress rotated files (default: false)
    #[serde(default)]
    pub log_compress: bool,
    /// Options for adapters, one table per adapter name (`adapters.<name>`). Only the adapter
    /// that owns a table receives its values; every client sees the option names only (see
    /// `redact_spec`).
    #[serde(default)]
    pub adapters: BTreeMap<String, serde_json::Value>,
    /// Read-only for every non-human actor, whatever `ai_control` says: they can look at the app
    /// but not stop, restart, change or delete it. The daemon's own supervision (crash
    /// restarts, cron, limits, the boot resurrect) is not affected.
    #[serde(default)]
    pub protected: bool,
    /// The opt-in for caution operations (stop, restart, reload) by AI actors on this app.
    #[serde(default)]
    pub ai_control: bool,
}

impl std::fmt::Debug for AppSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every field is named (no `..`), so adding a field to `AppSpec` fails to compile here
        // until someone decides how it is printed.
        let AppSpec {
            id,
            name,
            mode,
            command,
            args,
            cwd,
            env,
            restart_policy,
            kill_timeout_ms,
            created_at,
            instances,
            instance_id,
            port,
            port_range,
            health_check,
            max_memory_mb,
            startup_delay_ms,
            env_inherit,
            hooks,
            tags,
            max_uptime_secs,
            restart_mode,
            depends_on,
            depends_timeout_secs,
            depends_fail_open,
            restart_cron,
            log_max_size,
            log_max_files,
            log_compress,
            adapters,
            protected,
            ai_control,
        } = self;
        let mut env_keys: Vec<&String> = env.keys().collect();
        env_keys.sort();
        let set = |script: &Option<String>| script.as_ref().map(|_| "<set>");
        f.debug_struct("AppSpec")
            .field("id", id)
            .field("name", name)
            .field("mode", mode)
            .field("command", command)
            .field("args", args)
            .field("cwd", cwd)
            .field("env_keys", &env_keys)
            .field("restart_policy", restart_policy)
            .field("kill_timeout_ms", kill_timeout_ms)
            .field("created_at", created_at)
            .field("instances", instances)
            .field("instance_id", instance_id)
            .field("port", port)
            .field("port_range", port_range)
            .field("health_check", health_check)
            .field("max_memory_mb", max_memory_mb)
            .field("startup_delay_ms", startup_delay_ms)
            .field("env_inherit", env_inherit)
            .field("hook_on_start", &set(&hooks.on_start))
            .field("hook_on_stop", &set(&hooks.on_stop))
            .field("hook_on_restart", &set(&hooks.on_restart))
            .field("hook_on_crash", &set(&hooks.on_crash))
            .field("hook_on_error", &set(&hooks.on_error))
            .field("tags", tags)
            .field("max_uptime_secs", max_uptime_secs)
            .field("restart_mode", restart_mode)
            .field("depends_on", depends_on)
            .field("depends_timeout_secs", depends_timeout_secs)
            .field("depends_fail_open", depends_fail_open)
            .field("restart_cron", restart_cron)
            .field("log_max_size", log_max_size)
            .field("log_max_files", log_max_files)
            .field("log_compress", log_compress)
            .field("adapters", &adapters.keys().collect::<Vec<_>>())
            .field("protected", protected)
            .field("ai_control", ai_control)
            .finish()
    }
}

impl AppSpec {
    /// Deserialise a spec from a JSON value.
    pub fn from_json_value(value: serde_json::Value) -> serde_json::Result<Self> {
        serde_json::from_value(value)
    }

    /// Create a new AppSpec. Panics if the name is not a valid app name.
    /// Use `try_new` for a fallible version.
    pub fn new(name: String, mode: Runtime, command: String, cwd: PathBuf) -> Self {
        Self::try_new(name, mode, command, cwd).expect("invalid app name")
    }

    /// Create a new AppSpec, validating the app name.
    /// Returns an error if the name contains invalid characters.
    pub fn try_new(name: String, mode: Runtime, command: String, cwd: PathBuf) -> Result<Self> {
        crate::validate::check_app_name(&name).map_err(Error::InvalidSpec)?;

        Ok(Self {
            id: 0,
            name,
            mode,
            command,
            args: Vec::new(),
            cwd,
            env: HashMap::new(),
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
            restart_mode: RestartMode::default(),
            depends_on: Vec::new(),
            depends_timeout_secs: DEFAULT_DEPENDS_TIMEOUT_SECS,
            depends_fail_open: false,
            restart_cron: None,
            log_max_size: None,
            log_max_files: None,
            log_compress: false,
            adapters: BTreeMap::new(),
            protected: false,
            ai_control: false,
        })
    }

    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
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

    /// Create the app of one instance of a cluster: named `{name}-{instance_id}`, a single
    /// instance itself (so it can never be mistaken for a parent), with its own port if it has
    /// one (a port range is the parent's: the instance gets the one port out of it).
    pub fn for_instance(&self, instance_id: u32, port: Option<u16>) -> Self {
        let mut instance = self.clone();
        instance.instance_id = Some(instance_id);
        instance.instances = 1;
        instance.name = format!("{}-{}", self.name, instance_id);
        if let Some(p) = port {
            instance.port = Some(p);
            instance.port_range = None;
            instance.env.insert("PORT".to_string(), p.to_string());
        }
        instance
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}

/// How much damage a request can do, which decides who may make it (`docs/AI-SAFETY.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Reads. Every actor may make them.
    Safe,
    /// Changes the state of an app that exists (stop, restart): an AI needs the app's
    /// `ai_control` marker.
    Caution,
    /// Runs new code, destroys data or acts on everything: humans only.
    Dangerous,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Safe => "safe",
            Tier::Caution => "caution",
            Tier::Dangerous => "dangerous",
        }
    }
}

/// What the operator last asked for, persisted with the app (`desired_state`
/// column): `Running` apps are started again by `resurrect`, `Stopped` ones are not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    #[default]
    Running,
    Stopped,
}

impl Desired {
    pub fn as_str(&self) -> &'static str {
        match self {
            Desired::Running => "running",
            Desired::Stopped => "stopped",
        }
    }
}

impl std::str::FromStr for Desired {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(Desired::Running),
            "stopped" => Ok(Desired::Stopped),
            other => Err(Error::InvalidSpec(format!(
                "invalid desired state '{}'",
                other
            ))),
        }
    }
}

impl std::fmt::Display for Desired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Restart mode for process crash recovery
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartMode {
    /// Restart on non-zero exit code
    #[default]
    OnCrash,
    /// Always restart regardless of exit code
    Always,
    /// Never auto-restart
    Never,
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

/// The runtime of an app: the name of the thing that knows how to start it. `cmd` is the only
/// one the core runs itself; every other name is served by an adapter, so a new runtime needs no
/// change here.
///
/// It is a string with `#[serde(transparent)]`: the JSON key stays `mode`, the stored bytes are
/// exactly what the old enum wrote (`"node"`, `"cmd"`, ...), and there is no data migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct Runtime(Cow<'static, str>);

impl Runtime {
    pub const CMD: Runtime = Runtime(Cow::Borrowed("cmd"));

    /// Does `name` look like a runtime name: `^[a-z][a-z0-9]{1,15}$`? That is all `validate`
    /// checks.
    pub fn is_valid_name(name: &str) -> bool {
        let b = name.as_bytes();
        (2..=16).contains(&b.len())
            && b[0].is_ascii_lowercase()
            && b[1..]
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Is this the runtime the core executes itself?
    pub fn is_cmd(&self) -> bool {
        self.as_str() == "cmd"
    }

    /// A runtime with a name that is known at compile time.
    pub const fn from_static(name: &'static str) -> Self {
        Runtime(Cow::Borrowed(name))
    }

    /// The runtime a file extension means (the table is in `runtime_hints`).
    pub fn from_extension(ext: &str) -> Option<Self> {
        crate::runtime_hints::runtime_for_extension(ext).map(Runtime::from_static)
    }

    /// The runtime a path means: a project directory by its marker file, a file by its extension.
    pub fn detect(path: &std::path::Path) -> Option<Self> {
        if path.is_dir() {
            return crate::runtime_hints::runtime_for_dir(path).map(Runtime::from_static);
        }
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }
}

impl FromStr for Runtime {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let lower = s.to_lowercase();
        match lower.as_str() {
            "python" | "py" => Ok(Runtime::from_static("python")),
            "go" | "golang" => Ok(Runtime::from_static("go")),
            name if Runtime::is_valid_name(name) => Ok(Runtime(Cow::Owned(lower))),
            _ => Err(Error::InvalidMode(s.to_string())),
        }
    }
}

impl std::fmt::Display for Runtime {
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
    /// A start or restart is accepted but not launched yet: waiting for `depends_on` or
    /// honouring `startup_delay_ms`. No child.
    Waiting,
    /// The child died and an automatic restart is scheduled (`RunState::backoff_until`). No child.
    Backoff,
    /// The child is alive but its health check failed `retries` times in a row. The child is
    /// not touched unless the check's `action` is `restart`.
    Unhealthy,
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
            AppStatus::Waiting => "waiting",
            AppStatus::Backoff => "backoff",
            AppStatus::Unhealthy => "unhealthy",
        }
    }

    /// Is there a child process? (starting, running, unhealthy, stopping)
    pub fn has_child(&self) -> bool {
        matches!(
            self,
            AppStatus::Starting | AppStatus::Running | AppStatus::Unhealthy | AppStatus::Stopping
        )
    }

    /// Is the app doing something, or about to? Stop, delete and stop-all act on every active
    /// app; `Start` on an active app is refused. (waiting, building, starting, running,
    /// unhealthy, backoff, stopping)
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            AppStatus::Waiting
                | AppStatus::Building
                | AppStatus::Starting
                | AppStatus::Running
                | AppStatus::Unhealthy
                | AppStatus::Backoff
                | AppStatus::Stopping
        )
    }

    /// Is the app up and ready: what a dependent waits for. (running)
    pub fn is_ready(&self) -> bool {
        matches!(self, AppStatus::Running)
    }

    /// Is the app at rest until an operator acts? (stopped, errored)
    pub fn is_terminal(&self) -> bool {
        matches!(self, AppStatus::Stopped | AppStatus::Errored)
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
            "waiting" => Ok(AppStatus::Waiting),
            "backoff" => Ok(AppStatus::Backoff),
            "unhealthy" => Ok(AppStatus::Unhealthy),
            _ => Err(Error::ConfigError(format!("Invalid status: {}", s))),
        }
    }
}

/// Resource use summed across the processes belonging to an application.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcessTreeStats {
    pub process_count: u32,
    /// 100 percent represents one fully occupied CPU core.
    pub cpu_percent: f32,
    /// Summed resident memory, including shared pages once per process.
    pub memory_bytes: u64,
}

/// The supervisor's live measurements for an application.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AppStats {
    pub tree: ProcessTreeStats,
    pub output: OutputStats,
}

/// Output activity for the current capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputStats {
    pub last_output_at: Option<DateTime<Utc>>,
    /// Counts from capture start until the first bytes arrive.
    pub seconds_since_output: Option<u64>,
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
    /// Which automatic restart this backoff leads to (1 for the first). 0 outside a backoff.
    #[serde(default)]
    pub backoff_attempt: u32,
    /// When the automatic restart of a `backoff` app fires.
    #[serde(default)]
    pub backoff_until: Option<DateTime<Utc>>,
    /// Why the app is `errored` (or why the last launch failed).
    #[serde(default)]
    pub last_error: Option<String>,
    /// The signal that killed the last child, if it died by one.
    #[serde(default)]
    pub last_exit_signal: Option<i32>,
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
            backoff_attempt: 0,
            backoff_until: None,
            last_error: None,
            last_exit_signal: None,
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
            backoff_attempt: 0,
            backoff_until: None,
            last_error: None,
            last_exit_signal: None,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }
}

/// Restart policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub auto_restart: bool,
    /// Automatic restarts allowed inside any rolling `crash_window_secs`.
    pub max_restarts: u32,
    /// Delay before the first automatic restart; it doubles for every further crash in a row.
    pub restart_delay_ms: u64,
    pub crash_window_secs: u64,
    /// Upper bound of the doubling delay.
    #[serde(default = "default_max_restart_delay_ms")]
    pub max_restart_delay_ms: u64,
    /// A run at least this long starts the doubling over.
    #[serde(default = "default_stable_uptime_secs")]
    pub stable_uptime_secs: u64,
}

fn default_max_restart_delay_ms() -> u64 {
    DEFAULT_MAX_RESTART_DELAY_MS
}

fn default_stable_uptime_secs() -> u64 {
    DEFAULT_STABLE_UPTIME_SECS
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            auto_restart: true,
            max_restarts: DEFAULT_MAX_RESTARTS,
            restart_delay_ms: DEFAULT_RESTART_DELAY_MS,
            crash_window_secs: DEFAULT_CRASH_WINDOW_SECS,
            max_restart_delay_ms: DEFAULT_MAX_RESTART_DELAY_MS,
            stable_uptime_secs: DEFAULT_STABLE_UPTIME_SECS,
        }
    }
}

/// Selector for targeting apps by id, name, tag, or all
///
/// On the wire: `{"by":"all"}`, `{"by":"id","value":3}`, `{"by":"name","value":"web"}`,
/// `{"by":"tag","value":"frontend"}`. (The variant names in Rust are kept.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "by", content = "value")]
pub enum Selector {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "id")]
    ById(u32),
    #[serde(rename = "name")]
    ByName(String),
    /// Select by tag using @tagname syntax
    #[serde(rename = "tag")]
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
    fn debug_hides_secrets_in_config_and_its_nested_fields() {
        let config: crate::config::ConfigFile = toml::from_str(
            r#"
[adapters]
tools = "top-level-secret"
[[apps]]
name = "web"
[apps.env]
API_TOKEN = "environment-secret"
[apps.hooks]
on_start = "hook-secret"
[apps.health_check]
script = "probe-secret"
[apps.adapters.tools]
token = "adapter-secret"
"#,
        )
        .unwrap();
        let app = &config.apps[0];
        for shown in [
            format!("{config:?}"),
            format!("{app:?}"),
            format!("{:?}", app.hooks),
            format!("{:?}", app.health_check),
        ] {
            for secret in [
                "top-level-secret",
                "environment-secret",
                "hook-secret",
                "probe-secret",
                "adapter-secret",
            ] {
                assert!(!shown.contains(secret), "{secret} appeared in debug output");
            }
        }
        let hooks = app.hooks.as_ref().unwrap();
        let runtime_hooks = Hooks {
            on_start: hooks.on_start.clone(),
            ..Hooks::default()
        };
        let probe = HealthCheck::script("probe-secret");
        assert!(!format!("{runtime_hooks:?}").contains("hook-secret"));
        assert!(!format!("{probe:?}").contains("probe-secret"));
    }

    #[test]
    fn markers_default_false() {
        let spec = AppSpec::new("m".into(), Runtime::CMD, "true".into(), "/".into());
        assert!(!spec.protected && !spec.ai_control);
        // a spec written before the markers existed has neither
        let mut value = serde_json::to_value(&spec).unwrap();
        let map = value.as_object_mut().unwrap();
        map.remove("protected");
        map.remove("ai_control");
        let old = AppSpec::from_json_value(value).unwrap();
        assert!(!old.protected && !old.ai_control);
        // they roundtrip when set
        let mut set = spec.clone();
        set.protected = true;
        set.ai_control = true;
        let back = AppSpec::from_json_value(serde_json::to_value(&set).unwrap()).unwrap();
        assert!(back.protected && back.ai_control);
        // the Debug form shows them
        assert!(format!("{set:?}").contains("protected: true"));
    }

    #[test]
    fn test_app_mode_from_extension() {
        assert_eq!(
            Runtime::from_extension("rs"),
            Some(Runtime::from_static("rust"))
        );
        assert_eq!(
            Runtime::from_extension("js"),
            Some(Runtime::from_static("node"))
        );
        assert_eq!(
            Runtime::from_extension("mjs"),
            Some(Runtime::from_static("node"))
        );
        assert_eq!(
            Runtime::from_extension("ts"),
            Some(Runtime::from_static("node"))
        );
        assert_eq!(
            Runtime::from_extension("py"),
            Some(Runtime::from_static("python"))
        );
        assert_eq!(
            Runtime::from_extension("go"),
            Some(Runtime::from_static("go"))
        );
        assert_eq!(Runtime::from_extension("rb"), None);
    }

    #[test]
    fn test_app_mode_from_str() {
        assert_eq!(
            "cargo".parse::<Runtime>().unwrap(),
            Runtime::from_static("cargo")
        );
        assert_eq!(
            "node".parse::<Runtime>().unwrap(),
            Runtime::from_static("node")
        );
        assert!("Not A Runtime!".parse::<Runtime>().is_err());
        assert!("x".parse::<Runtime>().is_err(), "one letter is too short");
        assert!("9lives".parse::<Runtime>().is_err());
        // the old aliases still work, and so does a name no built-in runner knows
        assert_eq!(
            "py".parse::<Runtime>().unwrap(),
            Runtime::from_static("python")
        );
        assert_eq!(
            "golang".parse::<Runtime>().unwrap(),
            Runtime::from_static("go")
        );
        assert_eq!("Ruby".parse::<Runtime>().unwrap().as_str(), "ruby");
    }

    #[test]
    fn runtime_serialises_as_the_old_mode_string() {
        // what the old enum wrote, name by name
        for name in [
            "cmd", "node", "npm", "pnpm", "yarn", "cargo", "rust", "python", "go",
        ] {
            let runtime: Runtime = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(runtime.as_str(), name);
            assert_eq!(
                serde_json::to_string(&runtime).unwrap(),
                format!("\"{name}\"")
            );
        }
        assert_eq!(
            serde_json::to_string(&Runtime::from_static("node")).unwrap(),
            "\"node\""
        );
        // inside a spec the key is still `mode`, and nothing else was added
        let spec = AppSpec::new(
            "m".into(),
            Runtime::from_static("pnpm"),
            "start".into(),
            "/".into(),
        );
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value["mode"], "pnpm");
        assert!(value.get("runtime").is_none());
        let back: AppSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back.mode, Runtime::from_static("pnpm"));
        // a name this build has never heard of is carried through unchanged
        let odd: Runtime = serde_json::from_str("\"ruby\"").unwrap();
        assert_eq!(serde_json::to_string(&odd).unwrap(), "\"ruby\"");
    }

    #[test]
    fn test_restart_policy_default() {
        let policy = RestartPolicy::default();
        assert!(policy.auto_restart);
        assert_eq!(policy.max_restarts, DEFAULT_MAX_RESTARTS);
        assert_eq!(policy.restart_delay_ms, DEFAULT_RESTART_DELAY_MS);
    }

    #[test]
    fn restart_policy_without_the_new_fields_still_loads() {
        let old = r#"{"auto_restart":true,"max_restarts":4,"restart_delay_ms":250,"crash_window_secs":60}"#;
        let p: RestartPolicy = serde_json::from_str(old).unwrap();
        assert_eq!(p.max_restarts, 4);
        assert_eq!(p.crash_window_secs, 60);
        assert_eq!(p.max_restart_delay_ms, DEFAULT_MAX_RESTART_DELAY_MS);
        assert_eq!(p.stable_uptime_secs, DEFAULT_STABLE_UPTIME_SECS);
    }

    #[test]
    fn test_selector_parse() {
        assert_eq!(Selector::parse("all"), Selector::All);
        assert_eq!(Selector::parse("ALL"), Selector::All);
        assert_eq!(Selector::parse("123"), Selector::ById(123));
        assert_eq!(
            Selector::parse("myapp"),
            Selector::ByName("myapp".to_string())
        );
        assert_eq!(
            Selector::parse("@production"),
            Selector::ByTag("production".to_string())
        );
        assert_eq!(
            Selector::parse("@web-servers"),
            Selector::ByTag("web-servers".to_string())
        );
    }

    #[test]
    fn test_selector_matches() {
        let spec = AppSpec::new(
            "test-app".to_string(),
            Runtime::from_static("node"),
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
        assert_eq!(
            Selector::ByTag("production".to_string()).to_string(),
            "@production"
        );
    }

    #[test]
    fn test_app_spec_with_tags() {
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::from_static("node"),
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
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_max_uptime(86400); // 24 hours

        assert_eq!(spec.max_uptime_secs, Some(86400));
    }

    #[test]
    fn status_predicates_cover_every_process_state() {
        use AppStatus::*;
        let all = [
            Starting, Running, Stopping, Stopped, Errored, Building, Waiting, Backoff, Unhealthy,
        ];
        let set = |f: fn(&AppStatus) -> bool| -> Vec<AppStatus> {
            all.iter().copied().filter(f).collect()
        };
        assert_eq!(
            set(AppStatus::has_child),
            vec![Starting, Running, Stopping, Unhealthy]
        );
        assert_eq!(
            set(AppStatus::is_active),
            vec![Starting, Running, Stopping, Building, Waiting, Backoff, Unhealthy]
        );
        assert_eq!(set(AppStatus::is_ready), vec![Running]);
        assert_eq!(set(AppStatus::is_terminal), vec![Stopped, Errored]);
    }

    #[test]
    fn waiting_and_backoff_have_wire_names() {
        assert_eq!(
            serde_json::to_string(&AppStatus::Waiting).unwrap(),
            "\"waiting\""
        );
        assert_eq!(
            serde_json::to_string(&AppStatus::Backoff).unwrap(),
            "\"backoff\""
        );
        assert_eq!("backoff".parse::<AppStatus>().unwrap(), AppStatus::Backoff);
        assert_eq!("waiting".parse::<AppStatus>().unwrap(), AppStatus::Waiting);
        assert_eq!(
            serde_json::to_string(&AppStatus::Unhealthy).unwrap(),
            "\"unhealthy\""
        );
        assert_eq!(
            "unhealthy".parse::<AppStatus>().unwrap(),
            AppStatus::Unhealthy
        );
    }

    #[test]
    fn health_check_without_the_new_fields_still_loads() {
        let hc: HealthCheck = serde_json::from_str(
            r#"{"http_url":"http://localhost/h","script":null,"expected_status":[200],
                "interval_secs":30,"timeout_secs":5,"retries":3}"#,
        )
        .unwrap();
        assert_eq!(hc.start_period_secs, DEFAULT_HEALTH_START_PERIOD_SECS);
        assert_eq!(hc.action, HealthAction::Notify);
        let hc: HealthCheck = serde_json::from_str(
            r#"{"script":"true","expected_status":[200],"interval_secs":30,"timeout_secs":5,
                "retries":3,"start_period_secs":0,"action":"restart"}"#,
        )
        .unwrap();
        assert_eq!(hc.start_period_secs, 0);
        assert_eq!(hc.action, HealthAction::Restart);
    }

    #[test]
    fn run_state_without_the_new_fields_still_loads() {
        let state: RunState = serde_json::from_str(r#"{"app_id": 3}"#).unwrap();
        assert_eq!(state.backoff_attempt, 0);
        assert_eq!(state.backoff_until, None);
        assert_eq!(state.last_error, None);
        assert_eq!(state.last_exit_signal, None);
    }

    #[test]
    fn test_app_spec_builder() {
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_args(vec!["--port".to_string(), "3000".to_string()]);

        assert_eq!(spec.name, "test");
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
            Runtime::from_static("node"),
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
            Runtime::from_static("node"),
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
            Runtime::from_static("node"),
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
            Runtime::from_static("node"),
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

    fn legacy_spec_json() -> serde_json::Value {
        let spec = AppSpec::new(
            "legacy".to_string(),
            Runtime::CMD,
            "sleep".to_string(),
            PathBuf::from("/tmp"),
        );
        serde_json::to_value(spec).unwrap()
    }

    #[test]
    fn app_spec_from_json_value_still_rejects_other_unknown_keys() {
        let mut v = legacy_spec_json();
        v.as_object_mut()
            .unwrap()
            .insert("bogus_key".to_string(), serde_json::json!(true));
        let e = AppSpec::from_json_value(v).unwrap_err().to_string();
        assert!(e.contains("bogus_key"), "{e}");
    }

    #[test]
    fn test_validate_app_name_tightened_rules() {
        assert!(validate_app_name("_x"));
        assert!(validate_app_name("1a"));
        assert!(validate_app_name(&"a".repeat(64)));
        assert!(!validate_app_name(&"a".repeat(65)));
        assert!(!validate_app_name("-x"));
        assert!(!validate_app_name("123"));
        assert!(!validate_app_name("all"));
        assert!(!validate_app_name("ALL"));
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
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_app_spec_try_new_invalid() {
        let result = AppSpec::try_new(
            "../evil".to_string(),
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        );
        assert!(result.is_err());
    }
}
