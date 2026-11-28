//! Configuration file parsing for OxidePM
//!
//! Supports multiple configuration file formats:
//! - TOML (.toml)
//! - YAML (.yaml, .yml)
//! - JSON (.json)
//!
//! Parsing is strict: an unknown key is an error that names it, moved options
//! get migration guidance, and every app is validated (the
//! same rules the daemon applies) when the file is converted to specs, so a
//! mistake is reported with the app name at load time rather than at start.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::constants::*;
use crate::envfile;
use crate::error::{Error, Result};
use crate::types::{
    AppSpec, HealthAction, HealthCheck, Hooks, RestartMode, RestartPolicy, Runtime,
};
use crate::validate::{parse_size, validate_set_allow_external};

/// Supported configuration file formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
    Json,
}

impl ConfigFormat {
    /// Detect format from file extension
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "toml" => Some(ConfigFormat::Toml),
            "yaml" | "yml" => Some(ConfigFormat::Yaml),
            "json" => Some(ConfigFormat::Json),
            _ => None,
        }
    }

    /// Detect format from file path
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }
}

/// Health check configuration from config file
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckConfig {
    /// HTTP endpoint to check (e.g., "http://localhost:3000/health")
    pub http_url: Option<String>,
    /// TCP address (`host:port`) that must accept a connection
    pub tcp: Option<String>,
    /// Script to execute for health check
    pub script: Option<String>,
    /// Expected HTTP status codes (default: [200])
    #[serde(default)]
    pub expected_status: Vec<u16>,
    /// Interval between checks in seconds (default: 30)
    pub interval_secs: Option<u64>,
    /// Timeout for each check in seconds (default: 5, or the interval if that
    /// is shorter)
    pub timeout_secs: Option<u64>,
    /// Number of consecutive failures before marking unhealthy (default: 3)
    pub retries: Option<u32>,
    /// Seconds after the launch before the first probe (default: 5)
    pub start_period_secs: Option<u64>,
    /// What to do once the app is unhealthy: "notify" (default) or "restart"
    pub action: Option<HealthAction>,
}

impl HealthCheckConfig {
    /// Convert to HealthCheck type
    pub fn into_health_check(self) -> HealthCheck {
        let default = HealthCheck::default();
        let interval_secs = self.interval_secs.unwrap_or(default.interval_secs);
        HealthCheck {
            http_url: self.http_url,
            tcp: self.tcp,
            script: self.script,
            expected_status: if self.expected_status.is_empty() {
                default.expected_status
            } else {
                self.expected_status
            },
            interval_secs,
            // Someone who only lowers the interval must not trip the
            // "timeout <= interval" rule.
            timeout_secs: self
                .timeout_secs
                .unwrap_or_else(|| default.timeout_secs.min(interval_secs)),
            retries: self.retries.unwrap_or(default.retries),
            start_period_secs: self.start_period_secs.unwrap_or(default.start_period_secs),
            action: self.action.unwrap_or(default.action),
        }
    }
}

/// Event hooks configuration from config file
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
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

impl HooksConfig {
    /// Convert to Hooks type
    pub fn into_hooks(self) -> Hooks {
        Hooks {
            on_start: self.on_start,
            on_stop: self.on_stop,
            on_restart: self.on_restart,
            on_crash: self.on_crash,
            on_error: self.on_error,
        }
    }
}

/// Configuration file structure (oxidepm.config.toml/yaml/json)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub apps: Vec<AppConfig>,
    /// Reserved for adapters; rejected while non-empty.
    #[serde(default)]
    pub adapters: BTreeMap<String, serde_json::Value>,
    /// Reserved for policy; rejected while non-empty.
    #[serde(default)]
    pub policy: BTreeMap<String, serde_json::Value>,
}

/// Single app configuration from config file
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    /// The runtime of the app (`runtime` is the same key under its new name).
    #[serde(alias = "runtime")]
    pub mode: Option<String>,
    pub script: Option<String>,
    pub bin: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub env_file: Option<String>,
    pub restart_delay: Option<u64>,
    pub max_restarts: Option<u32>,
    /// Upper bound of the doubling restart delay, in milliseconds (default 60000).
    pub max_restart_delay: Option<u64>,
    /// A run this long, in seconds, starts the restart delay over (default 60).
    pub stable_uptime: Option<u64>,
    /// `max_restarts` inside this many seconds are a crash loop (default 600).
    pub crash_window: Option<u64>,
    pub kill_timeout: Option<u64>,
    #[serde(default)]
    pub no_autorestart: bool,
    // New fields for clustering and port management
    /// Number of instances to run (default: 1)
    #[serde(default = "default_instances")]
    pub instances: u32,
    /// Port for the application
    pub port: Option<u16>,
    /// Port range for clustered instances (start, end)
    pub port_range: Option<PortRange>,
    /// Health check configuration
    pub health_check: Option<HealthCheckConfig>,
    /// Maximum memory in MB before auto-restart
    pub max_memory_mb: Option<u64>,
    /// Event hooks configuration
    pub hooks: Option<HooksConfig>,
    /// Process tags for grouping (use @tag selector syntax)
    #[serde(default)]
    pub tags: Vec<String>,
    /// Maximum uptime in seconds before auto-restart (prevents memory leaks)
    pub max_uptime_secs: Option<u64>,
    /// Restart mode: "on-crash", "always", "never"
    pub restart: Option<String>,
    /// Path to the binary (alternative to `script`/`bin`)
    pub binary: Option<String>,
    // ── Process dependencies ──
    /// Wait for these processes to be online before starting (by name)
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Seconds to wait for `depends_on` apps to be running before the start fails closed
    /// (default 60, range 1 to 3600).
    pub depends_timeout: Option<u64>,
    /// Discouraged: launch anyway, with a warning, when the dependencies are not running in
    /// time (default false).
    #[serde(default)]
    pub depends_fail_open: bool,
    /// Cron expression for scheduled restarts (5 fields, e.g. "0 4 * * *" for
    /// daily at 4am UTC; 6 or 7 fields are seconds-first)
    pub restart_cron: Option<String>,
    // ── Log rotation settings ──
    /// Max log file size before rotation (e.g., "10mb", "500kb", or bytes)
    pub log_max_size: Option<String>,
    /// Max rotated files to keep (default: 5)
    pub log_max_files: Option<usize>,
    /// Compress rotated files with gzip (default: false)
    #[serde(default)]
    pub log_compress: bool,
    /// Options for adapters, one table per adapter name (`[apps.adapters.<name>]`).
    #[serde(default)]
    pub adapters: BTreeMap<String, serde_json::Value>,
    /// Read-only for AI actors and other non-human callers (default false).
    #[serde(default)]
    pub protected: bool,
    /// AI actors may stop, restart and reload this app (default false).
    #[serde(default)]
    pub ai_control: bool,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("apps", &self.apps)
            .field("adapters", &self.adapters.keys().collect::<Vec<_>>())
            .field("policy", &self.policy.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut env_keys: Vec<_> = self.env.keys().collect();
        env_keys.sort_unstable();
        f.debug_struct("AppConfig")
            .field("name", &self.name)
            .field("mode", &self.mode)
            .field("cwd", &self.cwd)
            .field("instances", &self.instances)
            .field("env_keys", &env_keys)
            .field("hooks", &self.hooks)
            .field("health_check", &self.health_check)
            .field("adapters", &self.adapters.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HooksConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |script: &Option<String>| script.as_ref().map(|_| crate::redact::SET);
        f.debug_struct("HooksConfig")
            .field("on_start", &set(&self.on_start))
            .field("on_stop", &set(&self.on_stop))
            .field("on_restart", &set(&self.on_restart))
            .field("on_crash", &set(&self.on_crash))
            .field("on_error", &set(&self.on_error))
            .finish()
    }
}

impl std::fmt::Debug for HealthCheckConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthCheckConfig")
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

fn default_instances() -> u32 {
    1
}

/// Port range configuration
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

/// The message of a config-related error without its variant prefix, so it can
/// be re-wrapped with context.
fn message_of(e: &Error) -> String {
    match e {
        Error::ConfigError(m) | Error::InvalidSpec(m) => m.clone(),
        other => other.to_string(),
    }
}

impl ConfigFile {
    /// Load config from file, automatically detecting format from extension.
    /// Errors name the file (and, for syntax and unknown-key errors, the line).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }

        let format = ConfigFormat::from_path(path).ok_or_else(|| {
            Error::ConfigError(format!(
                "Unsupported config file extension: {}. Expected .toml, .yaml, .yml, or .json",
                path.display()
            ))
        })?;

        let content = std::fs::read_to_string(path)?;
        Self::parse(&content, format)
            .map_err(|e| Error::ConfigError(format!("{}: {}", path.display(), message_of(&e))))
    }

    /// Parse config content with specified format
    pub fn parse(content: &str, format: ConfigFormat) -> Result<Self> {
        match format {
            ConfigFormat::Toml => Self::from_toml(content),
            ConfigFormat::Yaml => Self::from_yaml(content),
            ConfigFormat::Json => Self::from_json(content),
        }
    }

    /// Parse TOML config content
    pub fn from_toml(content: &str) -> Result<Self> {
        Self::reject_removed(content, ConfigFormat::Toml)?;
        toml::from_str(content).map_err(|e| Error::ConfigError(e.to_string()))
    }

    /// Parse YAML config content
    pub fn from_yaml(content: &str) -> Result<Self> {
        Self::reject_removed(content, ConfigFormat::Yaml)?;
        yaml_serde::from_str(content).map_err(|e| Error::ConfigError(format!("YAML: {}", e)))
    }

    /// Parse JSON config content
    pub fn from_json(content: &str) -> Result<Self> {
        Self::reject_removed(content, ConfigFormat::Json)?;
        serde_json::from_str(content).map_err(|e| Error::ConfigError(format!("JSON: {}", e)))
    }

    /// Scan the untyped document for options that moved to adapters, so
    /// they get a specific message instead of a generic "unknown field". A
    /// document that does not parse at all is left to the typed parse, which
    /// reports the syntax error.
    fn reject_removed(content: &str, format: ConfigFormat) -> Result<()> {
        let doc: Option<serde_json::Value> = match format {
            ConfigFormat::Toml => toml::from_str(content).ok(),
            ConfigFormat::Yaml => yaml_serde::from_str(content).ok(),
            ConfigFormat::Json => serde_json::from_str(content).ok(),
        };
        let Some(apps) = doc
            .as_ref()
            .and_then(|d| d.get("apps"))
            .and_then(|a| a.as_array())
        else {
            return Ok(());
        };
        let mut problems: Vec<String> = Vec::new();
        for (i, app) in apps.iter().enumerate() {
            let Some(table) = app.as_object() else {
                continue;
            };
            let name = table
                .get("name")
                .and_then(|n| n.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("#{}", i + 1));
            for key in ["watch", "ignore", "ignore_patterns"] {
                if table.contains_key(key) {
                    problems.push(format!(
                        "app '{name}': {key} moved: use [apps.adapters.watch]"
                    ));
                }
            }
            for key in ["git", "branch", "clone_dir", "clone-dir"] {
                if table.contains_key(key) {
                    problems.push(format!("app '{name}': {key} moved to the git-start adapter. Use oxidepm git-start <URL> [--branch BRANCH]"));
                }
            }
            if table.contains_key("setup") {
                problems.push(format!(
                    "app '{name}': setup was removed. Use the check adapter (`oxidepm check <target> --fix`) before starting"
                ));
            }
            for key in ["no_check", "no-check"] {
                if table.contains_key(key) {
                    problems.push(format!(
                        "app '{name}': {key} was removed. The check adapter is optional. Checks now run only with --check"
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::ConfigError(problems.join("\n")))
        }
    }

    /// Convert to AppSpec list. Every app is converted and validated; all the
    /// problems found are reported together. Relative paths are resolved
    /// against `base_dir` (made absolute against the current directory first).
    ///
    /// A `depends_on` entry may name an app that is not in the file (it can
    /// already be registered with the daemon), so only duplicate names and
    /// dependency cycles are judged across the file.
    pub fn into_specs(self, base_dir: &Path) -> Result<Vec<AppSpec>> {
        let mut problems: Vec<String> = Vec::new();
        if !self.adapters.is_empty() {
            problems.push("[adapters] is not available yet; remove it".to_string());
        }
        if !self.policy.is_empty() {
            problems.push("[policy] is not available yet; remove it".to_string());
        }

        let base = absolute(base_dir);
        let mut specs = Vec::with_capacity(self.apps.len());
        for app in self.apps {
            match app.into_spec(&base) {
                Ok(spec) => specs.push(spec),
                Err(e) => problems.push(message_of(&e)),
            }
        }
        if !problems.is_empty() {
            return Err(Error::ConfigError(problems.join("\n")));
        }

        validate_set_allow_external(&specs).map_err(|e| Error::ConfigError(message_of(&e)))?;
        Ok(specs)
    }
}

/// `dir` made absolute against the current directory (an empty path is the
/// current directory itself).
fn absolute(dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        return dir.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) if dir.as_os_str().is_empty() => cwd,
        Ok(cwd) => cwd.join(dir),
        Err(_) => dir.to_path_buf(),
    }
}

impl AppConfig {
    /// Convert to AppSpec. Errors name the app.
    pub fn into_spec(self, base_dir: &Path) -> Result<AppSpec> {
        let name = self.name.clone();
        self.build_spec(base_dir)
            .map_err(|e| Error::ConfigError(format!("app '{}': {}", name, message_of(&e))))
    }

    fn build_spec(self, base_dir: &Path) -> Result<AppSpec> {
        if self.mode.is_none() && self.binary.is_some() && self.script.is_some() {
            return Err(Error::ConfigError(
                "both binary and script are set; set mode explicitly".into(),
            ));
        }

        // Determine mode: an explicit mode wins; otherwise infer it only when
        // that is unambiguous.
        let mode = if let Some(mode_str) = &self.mode {
            mode_str.parse::<Runtime>()?
        } else if let Some(script) = &self.script {
            Path::new(script)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Runtime::from_extension)
                .ok_or_else(|| {
                    Error::ConfigError(format!(
                        "cannot infer mode for script '{}': set mode",
                        script
                    ))
                })?
        } else if self.bin.is_some() {
            Runtime::from_static("cargo")
        } else {
            Runtime::CMD
        };

        // Determine command — prefer `binary` field
        let command = self
            .binary
            .clone()
            .or(self.script.clone())
            .or(self.bin.clone())
            .unwrap_or_else(|| self.name.clone());

        // Determine cwd
        let cwd = if let Some(cwd_str) = &self.cwd {
            let p = Path::new(cwd_str);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_dir.join(p)
            }
        } else {
            base_dir.to_path_buf()
        };

        // Environment. Precedence: env file < `env` table (< explicit CLI
        // `--env`, applied by the CLI).
        let mut env = self.env;
        if let Some(env_file) = &self.env_file {
            let env_path = if Path::new(env_file).is_absolute() {
                Path::new(env_file).to_path_buf()
            } else {
                cwd.join(env_file)
            };
            if !env_path.is_file() {
                return Err(Error::ConfigError(format!(
                    "env_file '{}' does not exist",
                    env_path.display()
                )));
            }
            let content = std::fs::read_to_string(&env_path).map_err(|e| {
                Error::ConfigError(format!("env_file '{}': {}", env_path.display(), e))
            })?;
            let entries = envfile::parse(&content).map_err(|e| {
                Error::ConfigError(format!(
                    "env_file '{}': {}",
                    env_path.display(),
                    message_of(&e)
                ))
            })?;
            // A repeated key in the file: the later line wins; the table wins
            // over both.
            let from_file: HashMap<String, String> = entries.into_iter().collect();
            for (key, value) in from_file {
                env.entry(key).or_insert(value);
            }
        }

        // Build restart policy
        let restart_policy = RestartPolicy {
            auto_restart: !self.no_autorestart,
            max_restarts: self.max_restarts.unwrap_or(DEFAULT_MAX_RESTARTS),
            restart_delay_ms: self.restart_delay.unwrap_or(DEFAULT_RESTART_DELAY_MS),
            crash_window_secs: self.crash_window.unwrap_or(DEFAULT_CRASH_WINDOW_SECS),
            max_restart_delay_ms: self
                .max_restart_delay
                .unwrap_or(DEFAULT_MAX_RESTART_DELAY_MS),
            stable_uptime_secs: self.stable_uptime.unwrap_or(DEFAULT_STABLE_UPTIME_SECS),
        };

        // Convert health check config
        let health_check = self.health_check.map(|hc| hc.into_health_check());

        // Convert port range
        let port_range = self.port_range.map(|pr| (pr.start, pr.end));

        // Convert hooks config
        let hooks = self.hooks.map(|h| h.into_hooks()).unwrap_or_default();

        // Parse log max size (supports "10mb", "500kb", or raw bytes)
        let log_max_size = self
            .log_max_size
            .map(|s| {
                parse_size(&s)
                    .map_err(|e| Error::ConfigError(format!("log_max_size: {}", message_of(&e))))
            })
            .transpose()?;

        // Parse restart mode
        let restart_mode = self
            .restart
            .as_deref()
            .map(|s| s.parse::<RestartMode>())
            .transpose()?
            .unwrap_or_default();

        let spec = AppSpec {
            id: 0, // Will be assigned by database
            name: self.name,
            mode,
            command,
            args: self.args,
            cwd,
            env,
            restart_policy,
            kill_timeout_ms: self.kill_timeout.unwrap_or(DEFAULT_KILL_TIMEOUT_MS),
            created_at: chrono::Utc::now(),
            instances: self.instances,
            instance_id: None,
            port: self.port,
            port_range,
            health_check,
            max_memory_mb: self.max_memory_mb,
            startup_delay_ms: None,
            env_inherit: false,
            hooks,
            tags: self.tags,
            max_uptime_secs: self.max_uptime_secs,
            restart_mode,
            depends_on: self.depends_on,
            depends_timeout_secs: self.depends_timeout.unwrap_or(DEFAULT_DEPENDS_TIMEOUT_SECS),
            depends_fail_open: self.depends_fail_open,
            restart_cron: self.restart_cron,
            log_max_size,
            log_max_files: self.log_max_files,
            log_compress: self.log_compress,
            adapters: self.adapters,
            protected: self.protected,
            ai_control: self.ai_control,
        };
        spec.validate()?;
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn binary_and_script_require_an_explicit_mode() {
        let input = "name = 'web'\nbinary = 'server'\nscript = 'server.js'\n";
        let config: AppConfig = toml::from_str(input).unwrap();
        assert!(config
            .into_spec(Path::new("/srv"))
            .unwrap_err()
            .to_string()
            .contains("set mode"));
        let explicit: AppConfig = toml::from_str(&(input.to_string() + "mode = 'cmd'\n")).unwrap();
        assert_eq!(
            explicit.into_spec(Path::new("/srv")).unwrap().command,
            "server"
        );
    }

    #[test]
    fn test_config_format_detection() {
        assert_eq!(
            ConfigFormat::from_extension("toml"),
            Some(ConfigFormat::Toml)
        );
        assert_eq!(
            ConfigFormat::from_extension("yaml"),
            Some(ConfigFormat::Yaml)
        );
        assert_eq!(
            ConfigFormat::from_extension("yml"),
            Some(ConfigFormat::Yaml)
        );
        assert_eq!(
            ConfigFormat::from_extension("json"),
            Some(ConfigFormat::Json)
        );
        assert_eq!(ConfigFormat::from_extension("txt"), None);
    }

    #[test]
    fn test_config_parse_toml() {
        let config_content = r#"
[[apps]]
name = "api"
mode = "cargo"
cwd = "./"

[[apps]]
name = "web"
mode = "node"
script = "server.js"
args = ["--port", "3000"]
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 2);
        assert_eq!(config.apps[0].name, "api");
        assert_eq!(config.apps[0].mode, Some("cargo".to_string()));
        assert_eq!(config.apps[1].name, "web");
        assert_eq!(config.apps[1].script, Some("server.js".to_string()));
    }

    #[test]
    fn test_config_parse_yaml() {
        let config_content = r#"
apps:
  - name: web
    script: server.js
    instances: 4
    port: 3000
    env:
      NODE_ENV: production
    health_check:
      http_url: "http://localhost:3000/health"
      interval_secs: 30
  - name: api
    mode: cargo
    bin: api-server
    instances: 2
"#;
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 2);
        assert_eq!(config.apps[0].name, "web");
        assert_eq!(config.apps[0].instances, 4);
        assert_eq!(config.apps[0].port, Some(3000));
        assert!(config.apps[0].health_check.is_some());
        let hc = config.apps[0].health_check.as_ref().unwrap();
        assert_eq!(
            hc.http_url,
            Some("http://localhost:3000/health".to_string())
        );
        assert_eq!(hc.interval_secs, Some(30));
        assert_eq!(config.apps[1].name, "api");
        assert_eq!(config.apps[1].instances, 2);
    }

    #[test]
    fn test_config_parse_json() {
        let config_content = r#"
{
    "apps": [
        {
            "name": "web",
            "script": "server.js",
            "instances": 4,
            "port": 3000,
            "env": {
                "NODE_ENV": "production"
            },
            "health_check": {
                "http_url": "http://localhost:3000/health",
                "interval_secs": 30
            }
        },
        {
            "name": "api",
            "mode": "cargo",
            "bin": "api-server"
        }
    ]
}
"#;
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 2);
        assert_eq!(config.apps[0].name, "web");
        assert_eq!(config.apps[0].instances, 4);
        assert_eq!(config.apps[0].port, Some(3000));
        assert!(config.apps[0].health_check.is_some());
    }

    #[test]
    fn test_config_not_found() {
        let result = ConfigFile::load(Path::new("/nonexistent/config.toml"));
        assert!(matches!(result, Err(Error::ConfigNotFound(_))));
    }

    #[test]
    fn test_app_config_to_spec_with_new_fields() {
        let app_config: AppConfig = toml::from_str(
            r#"
name = "test"
mode = "node"
script = "app.js"
cwd = "./src"
args = ["--verbose"]
restart_delay = 1000
max_restarts = 5
kill_timeout = 5000
instances = 4
port_range = { start = 3000, end = 3003 }
max_memory_mb = 512
tags = ["web", "production"]
max_uptime_secs = 86400

[env]
NODE_ENV = "production"

[health_check]
http_url = "http://localhost:3000/health"
expected_status = [200, 201]
interval_secs = 30
timeout_secs = 10
retries = 5

[hooks]
on_start = "echo started"
on_crash = "/scripts/notify.sh"
"#,
        )
        .unwrap();

        let base_dir = Path::new("/project");
        let spec = app_config.into_spec(base_dir).unwrap();

        assert_eq!(spec.name, "test");
        assert_eq!(spec.mode, Runtime::from_static("node"));
        assert_eq!(spec.command, "app.js");
        assert_eq!(spec.cwd, Path::new("/project/src"));
        assert_eq!(spec.restart_policy.max_restarts, 5);
        assert_eq!(spec.restart_policy.restart_delay_ms, 1000);
        assert_eq!(spec.instances, 4);
        assert_eq!(spec.port, None);
        assert_eq!(spec.port_range, Some((3000, 3003)));
        assert_eq!(spec.max_memory_mb, Some(512));

        let hc = spec.health_check.unwrap();
        assert_eq!(
            hc.http_url,
            Some("http://localhost:3000/health".to_string())
        );
        assert_eq!(hc.expected_status, vec![200, 201]);
        assert_eq!(hc.interval_secs, 30);
        assert_eq!(hc.timeout_secs, 10);
        assert_eq!(hc.retries, 5);

        // Test hooks
        assert_eq!(spec.hooks.on_start, Some("echo started".to_string()));
        assert_eq!(spec.hooks.on_crash, Some("/scripts/notify.sh".to_string()));
        assert!(spec.hooks.on_stop.is_none());

        // Test tags and max_uptime
        assert_eq!(spec.tags, vec!["web", "production"]);
        assert_eq!(spec.max_uptime_secs, Some(86400));
    }

    #[test]
    fn test_app_config_to_spec() {
        let app_config: AppConfig = toml::from_str(
            r#"
name = "test"
mode = "node"
script = "app.js"
cwd = "./src"
args = ["--verbose"]
restart_delay = 1000
max_restarts = 5
kill_timeout = 5000

[env]
NODE_ENV = "production"
"#,
        )
        .unwrap();

        let base_dir = Path::new("/project");
        let spec = app_config.into_spec(base_dir).unwrap();

        assert_eq!(spec.name, "test");
        assert_eq!(spec.mode, Runtime::from_static("node"));
        assert_eq!(spec.command, "app.js");
        assert_eq!(spec.cwd, Path::new("/project/src"));
        assert_eq!(spec.restart_policy.max_restarts, 5);
        assert_eq!(spec.restart_policy.restart_delay_ms, 1000);
        // Hooks should be default (empty) when not specified
        assert!(spec.hooks.is_empty());
    }

    #[test]
    fn test_config_with_hooks_toml() {
        let config_content = r#"
[[apps]]
name = "api"
script = "server.js"

[apps.hooks]
on_crash = "/scripts/notify.sh"
on_restart = "echo 'Restarted!' >> /tmp/hooks.log"
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 1);
        assert!(config.apps[0].hooks.is_some());
        let hooks = config.apps[0].hooks.as_ref().unwrap();
        assert_eq!(hooks.on_crash, Some("/scripts/notify.sh".to_string()));
        assert_eq!(
            hooks.on_restart,
            Some("echo 'Restarted!' >> /tmp/hooks.log".to_string())
        );
        assert!(hooks.on_start.is_none());
    }

    #[test]
    fn test_config_with_hooks_yaml() {
        let config_content = r#"
apps:
  - name: api
    script: server.js
    hooks:
      on_start: "echo 'starting' >> /tmp/hooks.log"
      on_crash: "/scripts/notify.sh"
"#;
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 1);
        assert!(config.apps[0].hooks.is_some());
        let hooks = config.apps[0].hooks.as_ref().unwrap();
        assert_eq!(
            hooks.on_start,
            Some("echo 'starting' >> /tmp/hooks.log".to_string())
        );
        assert_eq!(hooks.on_crash, Some("/scripts/notify.sh".to_string()));
    }

    #[test]
    fn test_health_check_config_defaults() {
        let hc_config = HealthCheckConfig {
            http_url: Some("http://localhost:3000/health".to_string()),
            tcp: None,
            script: None,
            expected_status: vec![],
            interval_secs: None,
            timeout_secs: None,
            retries: None,
            start_period_secs: None,
            action: None,
        };

        let hc = hc_config.into_health_check();
        assert_eq!(
            hc.http_url,
            Some("http://localhost:3000/health".to_string())
        );
        // Should use defaults
        assert_eq!(hc.expected_status, vec![200]);
        assert_eq!(hc.interval_secs, 30);
        assert_eq!(hc.timeout_secs, 5);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period_secs, 5);
        assert_eq!(hc.action, HealthAction::Notify);
    }

    #[test]
    fn test_yaml_yml_extension() {
        let config_content = r#"
apps:
  - name: test
    script: app.js
"#;
        let mut file = NamedTempFile::with_suffix(".yml").unwrap();
        file.write_all(config_content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        assert_eq!(config.apps.len(), 1);
        assert_eq!(config.apps[0].name, "test");
    }

    #[test]
    fn test_config_with_log_rotation() {
        let content = r#"
[[apps]]
name = "myapp"
binary = "/usr/local/bin/myapp"
args = ["start"]
log_max_size = "50mb"
log_max_files = 10
log_compress = true
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        let spec = config.into_specs(Path::new("/")).unwrap();
        assert_eq!(spec[0].log_max_size, Some(50 * 1024 * 1024));
        assert_eq!(spec[0].log_max_files, Some(10));
        assert!(spec[0].log_compress);
    }

    #[test]
    fn test_config_with_depends_on() {
        let content = r#"
[[apps]]
name = "indexer"
script = "indexer.js"
depends_on = ["postgres", "redis"]
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        let spec = config.into_specs(Path::new("/")).unwrap();
        assert_eq!(spec[0].depends_on, vec!["postgres", "redis"]);
    }

    #[test]
    fn test_config_with_restart_cron() {
        let content = r#"
[[apps]]
name = "myapp"
binary = "/usr/local/bin/myapp"
args = ["start"]
restart_cron = "0 4 * * *"
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let config = ConfigFile::load(file.path()).unwrap();
        let spec = config.into_specs(Path::new("/")).unwrap();
        assert_eq!(spec[0].restart_cron, Some("0 4 * * *".to_string()));
    }
}

#[cfg(test)]
mod strict_tests {
    use super::*;
    use std::path::PathBuf;

    fn specs_toml(content: &str, base: &Path) -> Result<Vec<AppSpec>> {
        ConfigFile::from_toml(content)?.into_specs(base)
    }

    fn err_of(content: &str) -> String {
        specs_toml(content, Path::new("/proj"))
            .unwrap_err()
            .to_string()
    }

    fn one(content: &str) -> AppSpec {
        let mut v = specs_toml(content, Path::new("/proj")).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    #[test]
    fn mode_alias_in_config_means_runtime() {
        // TOML
        let spec = one("[[apps]]\nname = \"x\"\nruntime = \"python\"\nscript = \"app.py\"\n");
        assert_eq!(spec.mode, Runtime::from_static("python"));
        let spec = one("[[apps]]\nname = \"x\"\nmode = \"python\"\nscript = \"app.py\"\n");
        assert_eq!(spec.mode, Runtime::from_static("python"));
        // a runtime no built-in runner knows is a valid name
        let spec = one("[[apps]]\nname = \"x\"\nruntime = \"ruby\"\nbinary = \"app.rb\"\n");
        assert_eq!(spec.mode.as_str(), "ruby");
        // YAML and JSON
        let y = ConfigFile::from_yaml("apps:\n  - name: y\n    runtime: node\n    script: a.js\n")
            .unwrap();
        assert_eq!(
            y.into_specs(Path::new("/proj")).unwrap()[0].mode,
            Runtime::from_static("node")
        );
        let j =
            ConfigFile::from_json(r#"{"apps":[{"name":"j","runtime":"go","script":"main.go"}]}"#)
                .unwrap();
        assert_eq!(
            j.into_specs(Path::new("/proj")).unwrap()[0].mode,
            Runtime::from_static("go")
        );
        // both keys at once is a mistake, not a silent choice
        assert!(ConfigFile::from_toml(
            "[[apps]]\nname = \"x\"\nmode = \"node\"\nruntime = \"node\"\nscript = \"a.js\"\n"
        )
        .is_err());
    }

    #[test]
    fn config_crash_window_reaches_restart_policy() {
        let d = one("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\n").restart_policy;
        assert_eq!(d.crash_window_secs, 600);
        assert_eq!(d.max_restart_delay_ms, 60_000);
        assert_eq!(d.stable_uptime_secs, 60);
        let p = one(
            "[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\ncrash_window = 90\nmax_restart_delay = 4000\nstable_uptime = 7\nrestart_delay = 100\n",
        )
        .restart_policy;
        assert_eq!(p.crash_window_secs, 90);
        assert_eq!(p.max_restart_delay_ms, 4000);
        assert_eq!(p.stable_uptime_secs, 7);
        assert_eq!(p.restart_delay_ms, 100);
        // zero is refused with the app named
        let e = err_of("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\ncrash_window = 0\n");
        assert!(
            e.contains("app 'x'") && e.contains("crash_window_secs"),
            "{e}"
        );
    }

    #[test]
    fn config_depends_keys_reach_the_spec() {
        let d = one("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\n");
        assert_eq!(d.depends_timeout_secs, 60);
        assert!(!d.depends_fail_open);
        let s = one(
            "[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\ndepends_on = [\"db\"]\ndepends_timeout = 5\ndepends_fail_open = true\n",
        );
        assert_eq!(s.depends_timeout_secs, 5);
        assert!(s.depends_fail_open);
        let e = err_of("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\ndepends_timeout = 0\n");
        assert!(
            e.contains("app 'x'") && e.contains("depends_timeout_secs"),
            "{e}"
        );
    }

    #[test]
    fn config_markers_parse() {
        let d = one("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\n");
        assert!(!d.protected && !d.ai_control);
        let s = one(
            "[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\nprotected = true\nai_control = true\n",
        );
        assert!(s.protected && s.ai_control);
        let p = one("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\nprotected = true\n");
        assert!(p.protected && !p.ai_control);
        // yaml and json carry them too
        let y = ConfigFile::parse(
            "apps:\n  - name: x\n    binary: /bin/true\n    ai_control: true\n",
            ConfigFormat::Yaml,
        )
        .unwrap();
        assert!(y.into_specs(Path::new("/")).unwrap()[0].ai_control);
    }

    #[test]
    fn config_unknown_marker_key_names_it() {
        for key in ["protectd", "ai_controll", "ai-control"] {
            let e = err_of(&format!(
                "[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\n{key} = true\n"
            ));
            assert!(e.contains(key) && e.contains("unknown field"), "{key}: {e}");
        }
    }

    #[test]
    fn config_rejects_unknown_key_naming_it() {
        let e =
            err_of("[[apps]]\nname = \"x\"\nbinary = \"/bin/true\"\nmisspelled_setting = true\n");
        assert!(e.contains("misspelled_setting"), "{e}");
        assert!(e.contains("unknown field"), "{e}");
        // Nested tables are strict too.
        let e = err_of("[[apps]]\nname = \"x\"\n[apps.health_check]\nhttp_url = \"http://h\"\nintervall_secs = 3\n");
        assert!(e.contains("intervall_secs"), "{e}");
        let e = err_of("[[apps]]\nname = \"x\"\n[apps.hooks]\non_boot = \"x\"\n");
        assert!(e.contains("on_boot"), "{e}");
        let e = err_of("[[apps]]\nname = \"x\"\n[apps.port_range]\nstart = 1\nend = 2\nstep = 3\n");
        assert!(e.contains("step"), "{e}");
        let e = err_of("surprise = 1\n[[apps]]\nname = \"x\"\n");
        assert!(e.contains("surprise"), "{e}");
    }

    #[test]
    fn config_script_py_infers_python() {
        let s = one("[[apps]]\nname = \"p\"\nscript = \"main.py\"\n");
        assert_eq!(s.mode, Runtime::from_static("python"));
        assert_eq!(
            one("[[apps]]\nname = \"n\"\nscript = \"server.js\"\n").mode,
            Runtime::from_static("node")
        );
        assert_eq!(
            one("[[apps]]\nname = \"g\"\nscript = \"main.go\"\n").mode,
            Runtime::from_static("go")
        );
    }

    #[test]
    fn config_script_without_known_extension_errors() {
        let e = err_of("[[apps]]\nname = \"w\"\nscript = \"worker\"\n");
        assert!(
            e.contains("cannot infer mode for script 'worker'") && e.contains("set mode"),
            "{e}"
        );
        // An explicit mode makes it fine.
        assert_eq!(
            one("[[apps]]\nname = \"w\"\nmode = \"cmd\"\nscript = \"worker\"\n").mode,
            Runtime::CMD
        );
    }

    #[test]
    fn config_mode_inference_for_bin_binary_and_nothing() {
        assert_eq!(
            one("[[apps]]\nname = \"a\"\nbin = \"srv\"\n").mode,
            Runtime::from_static("cargo")
        );
        let b = one("[[apps]]\nname = \"b\"\nbinary = \"/usr/bin/b\"\n");
        assert_eq!((b.mode, b.command.as_str()), (Runtime::CMD, "/usr/bin/b"));
        let n = one("[[apps]]\nname = \"sleep\"\n");
        assert_eq!((n.mode, n.command.as_str()), (Runtime::CMD, "sleep"));
    }

    #[test]
    fn config_log_max_size_negative_and_zero_error() {
        for bad in ["-1mb", "0", "0kb", "1.5mb", "99999999999gb", ""] {
            let e = err_of(&format!(
                "[[apps]]\nname = \"l\"\nbinary = \"/x\"\nlog_max_size = \"{bad}\"\n"
            ));
            assert!(
                e.contains("app 'l'") && e.contains("invalid size"),
                "{bad}: {e}"
            );
        }
        let s = one("[[apps]]\nname = \"l\"\nbinary = \"/x\"\nlog_max_size = \"2 MB\"\n");
        assert_eq!(s.log_max_size, Some(2 << 20));
        let e = err_of("[[apps]]\nname = \"l\"\nbinary = \"/x\"\nlog_max_files = 0\n");
        assert!(e.contains("log_max_files"), "{e}");
    }

    #[test]
    fn config_missing_env_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let toml = "[[apps]]\nname = \"e\"\nbinary = \"/x\"\nenv_file = \".env.nope\"\n";
        let e = specs_toml(toml, dir.path()).unwrap_err().to_string();
        assert!(
            e.contains("app 'e'") && e.contains(".env.nope") && e.contains("does not exist"),
            "{e}"
        );
    }

    #[test]
    fn config_env_file_export_and_inline_comment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "export A=1 # note\nB='two # kept'\nC=\"x\\ny\"\nSHARED=from-file\n",
        )
        .unwrap();
        let toml = "[[apps]]\nname = \"e\"\nbinary = \"/x\"\nenv_file = \".env\"\nenv = { SHARED = \"from-table\", D = \"4\" }\n";
        let s = specs_toml(toml, dir.path()).unwrap().remove(0);
        assert_eq!(s.env["A"], "1");
        assert_eq!(s.env["B"], "two # kept");
        assert_eq!(s.env["C"], "x\ny");
        assert_eq!(s.env["D"], "4");
        assert_eq!(s.env["SHARED"], "from-table", "env table beats env file");
        assert!(!s.env.contains_key("export A"));
        // A malformed env file names the app and the file.
        std::fs::write(dir.path().join(".env"), "1BAD=x\n").unwrap();
        let e = specs_toml(toml, dir.path()).unwrap_err().to_string();
        assert!(
            e.contains("app 'e'") && e.contains(".env") && e.contains("line 1"),
            "{e}"
        );
    }

    #[test]
    fn config_restart_cron_nonsense_errors() {
        let e = err_of("[[apps]]\nname = \"c\"\nbinary = \"/x\"\nrestart_cron = \"nonsense\"\n");
        assert!(e.contains("app 'c'") && e.contains("cron"), "{e}");
    }

    #[test]
    fn config_five_field_cron_accepted() {
        let s = one("[[apps]]\nname = \"c\"\nbinary = \"/x\"\nrestart_cron = \"0 4 * * *\"\n");
        assert_eq!(s.restart_cron.as_deref(), Some("0 4 * * *"));
    }

    #[test]
    fn config_health_defaults_timeout_to_interval() {
        let hc = |extra: &str| {
            one(&format!(
                "[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nhttp_url = \"http://localhost:1/h\"\n{extra}"
            ))
            .health_check
            .unwrap()
        };
        let h = hc("interval_secs = 2\n");
        assert_eq!((h.interval_secs, h.timeout_secs), (2, 2));
        let h = hc("");
        assert_eq!((h.interval_secs, h.timeout_secs), (30, 5));
        let h = hc("interval_secs = 10\n");
        assert_eq!((h.interval_secs, h.timeout_secs), (10, 5));
        let h = hc("interval_secs = 10\ntimeout_secs = 7\n");
        assert_eq!(h.timeout_secs, 7);
        // An explicit timeout above the interval is still an error.
        let e = err_of("[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nhttp_url = \"http://h\"\ninterval_secs = 2\ntimeout_secs = 3\n");
        assert!(
            e.contains("app 'h'") && e.contains("health_check") && e.contains("timeout_secs"),
            "{e}"
        );
        let e = err_of("[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nhttp_url = \"http://h\"\nretries = 0\n");
        assert!(e.contains("retries"), "{e}");
    }

    #[test]
    fn config_health_tcp_key() {
        let toml = "[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\ntcp = \"127.0.0.1:6379\"\n";
        let h = one(toml).health_check.unwrap();
        assert_eq!(h.tcp.as_deref(), Some("127.0.0.1:6379"));
        assert!(h.http_url.is_none() && h.script.is_none());
        let e = err_of(
            "[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\ntcp = \"nope\"\n",
        );
        assert!(
            e.contains("app 'h'") && e.contains("health_check") && e.contains("host:port"),
            "{e}"
        );
        let e = err_of("[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\ntcp = \"h:80\"\nscript = \"true\"\n");
        assert!(e.contains("only one"), "{e}");
    }

    #[test]
    fn config_health_start_period_and_action_keys() {
        let toml = "[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nscript = \"true\"\nstart_period_secs = 12\naction = \"restart\"\n";
        let h = one(toml).health_check.unwrap();
        assert_eq!(h.start_period_secs, 12);
        assert_eq!(h.action, HealthAction::Restart);
        let e = err_of("[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nscript = \"true\"\nstart_period_secs = 99999\n");
        assert!(
            e.contains("app 'h'") && e.contains("start_period_secs"),
            "{e}"
        );
        let e = err_of("[[apps]]\nname = \"h\"\nbinary = \"/x\"\n[apps.health_check]\nscript = \"true\"\naction = \"explode\"\n");
        assert!(e.contains("explode") || e.contains("action"), "{e}");
    }

    #[test]
    fn config_git_keys_report_moved_message() {
        for (parse, input) in [
            (
                ConfigFile::from_toml as fn(&str) -> Result<ConfigFile>,
                "[[apps]]\nname = 'web'\ngit = 'https://example.com/web'\nbranch = 'main'\nclone_dir = 'web'\n",
            ),
            (
                ConfigFile::from_yaml,
                "apps:\n  - name: web\n    git: https://example.com/web\n    branch: main\n    clone-dir: web\n",
            ),
            (
                ConfigFile::from_json,
                r#"{"apps":[{"name":"web","git":"https://example.com/web","branch":"main","clone_dir":"web"}]}"#,
            ),
        ] {
            let error = parse(input).unwrap_err().to_string();
            for key in ["git", "branch", if input.contains("clone-dir") { "clone-dir" } else { "clone_dir" }] {
                assert!(error.contains(&format!("app 'web': {key} moved")) && error.contains("oxidepm git-start"), "{error}");
            }
        }
    }

    #[test]
    fn config_watch_and_ignore_keys_report_moved_message() {
        for (parse, input) in [
            (
                ConfigFile::from_toml as fn(&str) -> Result<ConfigFile>,
                "[[apps]]\nname = 'w'\nbinary = '/x'\nwatch = false\nignore = []\nignore_patterns = []\n",
            ),
            (
                ConfigFile::from_yaml,
                "apps:\n  - name: w\n    binary: /x\n    watch: false\n    ignore: []\n    ignore_patterns: []\n",
            ),
            (
                ConfigFile::from_json,
                r#"{"apps":[{"name":"w","binary":"/x","watch":false,"ignore":[],"ignore_patterns":[]}]}"#,
            ),
        ] {
            let err = parse(input).unwrap_err().to_string();
            for key in ["watch", "ignore", "ignore_patterns"] {
                let message = format!("app 'w': {key} moved: use [apps.adapters.watch]");
                assert!(err.contains(&message), "{err}");
            }
        }
    }

    #[test]
    fn config_top_level_adapters_table_rejected_app_options_accepted() {
        let e = err_of(
            "[adapters.watch]\npaths = [\"src\"]\n[[apps]]\nname = \"a\"\nbinary = \"/x\"\n",
        );
        assert!(
            e.contains("adapters") && e.contains("not available yet"),
            "{e}"
        );
        let e = err_of("[policy]\nmax = 1\n[[apps]]\nname = \"a\"\nbinary = \"/x\"\n");
        assert!(
            e.contains("policy") && e.contains("not available yet"),
            "{e}"
        );
        // An app's own options are accepted (they are redacted for clients, validated for size).
        let spec = one("[[apps]]\nname = \"a\"\nbinary = \"/x\"\n[apps.adapters.watch]\nx = 1\n");
        assert_eq!(spec.adapters["watch"], serde_json::json!({"x": 1}));
        let e = err_of("[[apps]]\nname = \"a\"\nbinary = \"/x\"\n[apps.adapters]\nwatch = 1\n");
        assert!(
            e.contains("app 'a'") && e.contains("adapters.watch must be a table"),
            "{e}"
        );
        // Empty tables are accepted: the format is reserved, not in use.
        one("[adapters]\n[policy]\n[[apps]]\nname = \"a\"\nbinary = \"/x\"\n[apps.adapters]\n");
    }

    #[test]
    fn config_toml_yaml_json_all_strict() {
        let yaml = "apps:\n  - name: y\n    binary: /x\n    misspelled_setting: true\n";
        let e = ConfigFile::from_yaml(yaml).unwrap_err().to_string();
        assert!(e.contains("misspelled_setting"), "{e}");
        let json = r#"{"apps":[{"name":"j","binary":"/x","misspelled_setting":true}]}"#;
        let e = ConfigFile::from_json(json).unwrap_err().to_string();
        assert!(e.contains("misspelled_setting"), "{e}");
        // And valid documents still load.
        let s = ConfigFile::from_yaml("apps:\n  - name: y\n    binary: /x\n")
            .unwrap()
            .into_specs(Path::new("/p"))
            .unwrap();
        assert_eq!(s[0].name, "y");
    }

    #[test]
    fn config_load_error_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "[[apps]]\nname = \"x\"\nnope = 1\n").unwrap();
        let e = ConfigFile::load(&path).unwrap_err().to_string();
        assert!(
            e.contains("bad.toml") && e.contains("nope") && e.contains("line 3"),
            "{e}"
        );
    }

    #[test]
    fn config_validation_errors_name_the_app_and_cover_all_apps() {
        let e = err_of(
            "[[apps]]\nname = \"one\"\nbinary = \"/x\"\ninstances = 0\n[[apps]]\nname = \"ok\"\nbinary = \"/x\"\n[[apps]]\nname = \"../evil\"\nbinary = \"/x\"\n",
        );
        assert!(e.contains("app 'one'") && e.contains("instances"), "{e}");
        assert!(e.contains("app '../evil'") && e.contains("name"), "{e}");
        assert!(!e.contains("app 'ok'"), "{e}");
    }

    #[test]
    fn config_set_rules_duplicate_names_and_cycles() {
        let e = err_of(
            "[[apps]]\nname = \"a\"\nbinary = \"/x\"\n[[apps]]\nname = \"a\"\nbinary = \"/y\"\n",
        );
        assert!(e.contains("'a'") && e.contains("more than once"), "{e}");
        let e = err_of(
            "[[apps]]\nname = \"a\"\nbinary = \"/x\"\ndepends_on = [\"b\"]\n[[apps]]\nname = \"b\"\nbinary = \"/x\"\ndepends_on = [\"a\"]\n",
        );
        assert!(e.contains("cycle") && e.contains("a -> b -> a"), "{e}");
        // A dependency outside the file is fine: it may already be running.
        let s = one("[[apps]]\nname = \"a\"\nbinary = \"/x\"\ndepends_on = [\"postgres\"]\n");
        assert_eq!(s.depends_on, vec!["postgres"]);
    }

    #[test]
    fn config_relative_base_dir_becomes_absolute() {
        let s = specs_toml(
            "[[apps]]\nname = \"a\"\nbinary = \"/x\"\ncwd = \"sub\"\n",
            Path::new(""),
        )
        .unwrap();
        assert!(s[0].cwd.is_absolute(), "{:?}", s[0].cwd);
        assert!(s[0].cwd.ends_with("sub"));
        let s = specs_toml(
            "[[apps]]\nname = \"a\"\nbinary = \"/x\"\n",
            Path::new("rel/dir"),
        )
        .unwrap();
        assert_eq!(s[0].cwd, std::env::current_dir().unwrap().join("rel/dir"));
    }

    /// Every config shipped as an example or documented in the README must
    /// load with the strict parser, so a typo in the docs cannot ship. The
    /// files an example refers to (`env_file`) are created in a scratch tree.
    fn check_doc_config(label: &str, text: &str, format: ConfigFormat) {
        let dir = tempfile::tempdir().unwrap();
        let parsed = ConfigFile::parse(text, format).unwrap_or_else(|e| panic!("{label}: {e}"));
        for app in &parsed.apps {
            let cwd: PathBuf = match &app.cwd {
                Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
                Some(c) => dir.path().join(c),
                None => dir.path().to_path_buf(),
            };
            if let Some(env_file) = &app.env_file {
                let path = cwd.join(env_file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "EXAMPLE=1\n").unwrap();
            }
        }
        let specs = parsed
            .into_specs(dir.path())
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(!specs.is_empty(), "{label}: no apps");
    }

    #[test]
    fn examples_parse_with_strict_config() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut checked = 0;
        for dir in ["examples", "docs"] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let is_app_config = name.contains(".config.");
                let Some(format) = ConfigFormat::from_path(&path).filter(|_| is_app_config) else {
                    continue;
                };
                let text = std::fs::read_to_string(&path).unwrap();
                check_doc_config(&path.display().to_string(), &text, format);
                checked += 1;
            }
        }
        assert!(
            checked >= 4,
            "expected the shipped examples, checked {checked}"
        );

        // The README's config sample is documentation of the same format.
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        let mut blocks = 0;
        for block in readme.split("```toml").skip(1) {
            let body = block.split("```").next().unwrap();
            if body.contains("[[apps]]") {
                check_doc_config("README.md", body, ConfigFormat::Toml);
                blocks += 1;
            }
        }
        assert!(blocks >= 1, "README should show a config sample");
    }
}
