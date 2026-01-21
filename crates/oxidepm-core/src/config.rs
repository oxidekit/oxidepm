//! Configuration file parsing for OxidePM
//!
//! Supports multiple configuration file formats:
//! - TOML (.toml)
//! - YAML (.yaml, .yml)
//! - JSON (.json)

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::constants::*;
use crate::error::{Error, Result};
use crate::types::{AppMode, AppSpec, HealthCheck, Hooks, RestartPolicy};

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
#[derive(Debug, Deserialize, Default)]
pub struct HealthCheckConfig {
    /// HTTP endpoint to check (e.g., "http://localhost:3000/health")
    pub http_url: Option<String>,
    /// Script to execute for health check
    pub script: Option<String>,
    /// Expected HTTP status codes (default: [200])
    #[serde(default)]
    pub expected_status: Vec<u16>,
    /// Interval between checks in seconds (default: 30)
    pub interval_secs: Option<u64>,
    /// Timeout for each check in seconds (default: 5)
    pub timeout_secs: Option<u64>,
    /// Number of consecutive failures before marking unhealthy (default: 3)
    pub retries: Option<u32>,
}

impl HealthCheckConfig {
    /// Convert to HealthCheck type
    pub fn into_health_check(self) -> HealthCheck {
        let default = HealthCheck::default();
        HealthCheck {
            http_url: self.http_url,
            script: self.script,
            expected_status: if self.expected_status.is_empty() { default.expected_status } else { self.expected_status },
            interval_secs: self.interval_secs.unwrap_or(default.interval_secs),
            timeout_secs: self.timeout_secs.unwrap_or(default.timeout_secs),
            retries: self.retries.unwrap_or(default.retries),
        }
    }
}

/// Event hooks configuration from config file
#[derive(Debug, Deserialize, Default)]
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
#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub apps: Vec<AppConfig>,
}

/// Single app configuration from config file
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub mode: Option<String>,
    pub script: Option<String>,
    pub bin: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub env_file: Option<String>,
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub ignore: Vec<String>,
    pub restart_delay: Option<u64>,
    pub max_restarts: Option<u32>,
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
}

fn default_instances() -> u32 {
    1
}

/// Port range configuration
#[derive(Debug, Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl ConfigFile {
    /// Load config from file, automatically detecting format from extension
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
        let config: ConfigFile = toml::from_str(content)?;
        Ok(config)
    }

    /// Parse YAML config content
    pub fn from_yaml(content: &str) -> Result<Self> {
        let config: ConfigFile = serde_yaml::from_str(content)?;
        Ok(config)
    }

    /// Parse JSON config content
    pub fn from_json(content: &str) -> Result<Self> {
        let config: ConfigFile = serde_json::from_str(content)?;
        Ok(config)
    }

    /// Find and load config file from current directory
    pub fn find_and_load(dir: &Path) -> Result<(Self, std::path::PathBuf)> {
        for name in CONFIG_FILES {
            let path = dir.join(name);
            if path.exists() {
                let config = Self::load(&path)?;
                return Ok((config, path));
            }
        }
        Err(Error::ConfigError(format!(
            "No config file found in {}. Expected one of: {:?}",
            dir.display(),
            CONFIG_FILES
        )))
    }

    /// Convert to AppSpec list
    pub fn into_specs(self, base_dir: &Path) -> Result<Vec<AppSpec>> {
        self.apps
            .into_iter()
            .map(|app| app.into_spec(base_dir))
            .collect()
    }
}

impl AppConfig {
    /// Convert to AppSpec
    pub fn into_spec(self, base_dir: &Path) -> Result<AppSpec> {
        // Determine mode
        let mode = if let Some(mode_str) = &self.mode {
            mode_str.parse::<AppMode>()?
        } else if self.script.is_some() {
            AppMode::Node
        } else if self.bin.is_some() {
            AppMode::Cargo
        } else {
            AppMode::Cmd
        };

        // Determine command
        let command = self
            .script
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

        // Load env file if specified
        let mut env = self.env;
        if let Some(env_file) = &self.env_file {
            let env_path = if Path::new(env_file).is_absolute() {
                Path::new(env_file).to_path_buf()
            } else {
                cwd.join(env_file)
            };
            if env_path.exists() {
                load_env_file(&env_path, &mut env)?;
            }
        }

        // Build ignore patterns
        let mut ignore_patterns: Vec<String> = DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        ignore_patterns.extend(self.ignore);

        // Build restart policy
        let restart_policy = RestartPolicy {
            auto_restart: !self.no_autorestart,
            max_restarts: self.max_restarts.unwrap_or(DEFAULT_MAX_RESTARTS),
            restart_delay_ms: self.restart_delay.unwrap_or(DEFAULT_RESTART_DELAY_MS),
            crash_window_secs: DEFAULT_CRASH_WINDOW_SECS,
        };

        // Convert health check config
        let health_check = self.health_check.map(|hc| hc.into_health_check());

        // Convert port range
        let port_range = self.port_range.map(|pr| (pr.start, pr.end));

        // Convert hooks config
        let hooks = self.hooks.map(|h| h.into_hooks()).unwrap_or_default();

        Ok(AppSpec {
            id: 0, // Will be assigned by database
            name: self.name,
            mode,
            command,
            args: self.args,
            cwd,
            env,
            watch: self.watch,
            ignore_patterns,
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
        })
    }
}

/// Load environment variables from a .env file
fn load_env_file(path: &Path, env: &mut HashMap<String, String>) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(pos) = line.find('=') {
            let key = line[..pos].trim().to_string();
            let value = line[pos + 1..].trim();
            // Remove surrounding quotes if present
            let value = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value)
                .to_string();
            env.insert(key, value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_config_format_detection() {
        assert_eq!(ConfigFormat::from_extension("toml"), Some(ConfigFormat::Toml));
        assert_eq!(ConfigFormat::from_extension("yaml"), Some(ConfigFormat::Yaml));
        assert_eq!(ConfigFormat::from_extension("yml"), Some(ConfigFormat::Yaml));
        assert_eq!(ConfigFormat::from_extension("json"), Some(ConfigFormat::Json));
        assert_eq!(ConfigFormat::from_extension("txt"), None);
    }

    #[test]
    fn test_config_parse_toml() {
        let config_content = r#"
[[apps]]
name = "api"
mode = "cargo"
cwd = "./"
watch = true
ignore = ["target", ".git"]

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
        assert!(config.apps[0].watch);
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
    watch: true
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
        assert!(config.apps[0].watch);
        assert!(config.apps[0].health_check.is_some());
        let hc = config.apps[0].health_check.as_ref().unwrap();
        assert_eq!(hc.http_url, Some("http://localhost:3000/health".to_string()));
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
            "watch": true,
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
        assert!(config.apps[0].watch);
        assert!(config.apps[0].health_check.is_some());
    }

    #[test]
    fn test_config_not_found() {
        let result = ConfigFile::load(Path::new("/nonexistent/config.toml"));
        assert!(matches!(result, Err(Error::ConfigNotFound(_))));
    }

    #[test]
    fn test_app_config_to_spec_with_new_fields() {
        let app_config = AppConfig {
            name: "test".to_string(),
            mode: Some("node".to_string()),
            script: Some("app.js".to_string()),
            bin: None,
            cwd: Some("./src".to_string()),
            args: vec!["--verbose".to_string()],
            env: HashMap::from([("NODE_ENV".to_string(), "production".to_string())]),
            env_file: None,
            watch: true,
            ignore: vec!["dist".to_string()],
            restart_delay: Some(1000),
            max_restarts: Some(5),
            kill_timeout: Some(5000),
            no_autorestart: false,
            instances: 4,
            port: Some(3000),
            port_range: Some(PortRange { start: 3000, end: 3003 }),
            health_check: Some(HealthCheckConfig {
                http_url: Some("http://localhost:3000/health".to_string()),
                script: None,
                expected_status: vec![200, 201],
                interval_secs: Some(30),
                timeout_secs: Some(10),
                retries: Some(5),
            }),
            max_memory_mb: Some(512),
            hooks: Some(HooksConfig {
                on_start: Some("echo started".to_string()),
                on_crash: Some("/scripts/notify.sh".to_string()),
                on_stop: None,
                on_restart: None,
                on_error: None,
            }),
            tags: vec!["web".to_string(), "production".to_string()],
            max_uptime_secs: Some(86400),
        };

        let base_dir = Path::new("/project");
        let spec = app_config.into_spec(base_dir).unwrap();

        assert_eq!(spec.name, "test");
        assert_eq!(spec.mode, AppMode::Node);
        assert_eq!(spec.command, "app.js");
        assert_eq!(spec.cwd, Path::new("/project/src"));
        assert!(spec.watch);
        assert_eq!(spec.restart_policy.max_restarts, 5);
        assert_eq!(spec.restart_policy.restart_delay_ms, 1000);
        assert_eq!(spec.instances, 4);
        assert_eq!(spec.port, Some(3000));
        assert_eq!(spec.port_range, Some((3000, 3003)));
        assert_eq!(spec.max_memory_mb, Some(512));

        let hc = spec.health_check.unwrap();
        assert_eq!(hc.http_url, Some("http://localhost:3000/health".to_string()));
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
        let app_config = AppConfig {
            name: "test".to_string(),
            mode: Some("node".to_string()),
            script: Some("app.js".to_string()),
            bin: None,
            cwd: Some("./src".to_string()),
            args: vec!["--verbose".to_string()],
            env: HashMap::from([("NODE_ENV".to_string(), "production".to_string())]),
            env_file: None,
            watch: true,
            ignore: vec!["dist".to_string()],
            restart_delay: Some(1000),
            max_restarts: Some(5),
            kill_timeout: Some(5000),
            no_autorestart: false,
            instances: 1,
            port: None,
            port_range: None,
            health_check: None,
            max_memory_mb: None,
            hooks: None,
            tags: vec![],
            max_uptime_secs: None,
        };

        let base_dir = Path::new("/project");
        let spec = app_config.into_spec(base_dir).unwrap();

        assert_eq!(spec.name, "test");
        assert_eq!(spec.mode, AppMode::Node);
        assert_eq!(spec.command, "app.js");
        assert_eq!(spec.cwd, Path::new("/project/src"));
        assert!(spec.watch);
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
        assert_eq!(hooks.on_restart, Some("echo 'Restarted!' >> /tmp/hooks.log".to_string()));
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
        assert_eq!(hooks.on_start, Some("echo 'starting' >> /tmp/hooks.log".to_string()));
        assert_eq!(hooks.on_crash, Some("/scripts/notify.sh".to_string()));
    }

    #[test]
    fn test_load_env_file() {
        let env_content = r#"
# Comment
DATABASE_URL=postgres://localhost/db
API_KEY="secret123"
DEBUG='true'
EMPTY=
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(env_content.as_bytes()).unwrap();

        let mut env = HashMap::new();
        load_env_file(file.path(), &mut env).unwrap();

        assert_eq!(env.get("DATABASE_URL"), Some(&"postgres://localhost/db".to_string()));
        assert_eq!(env.get("API_KEY"), Some(&"secret123".to_string()));
        assert_eq!(env.get("DEBUG"), Some(&"true".to_string()));
        assert_eq!(env.get("EMPTY"), Some(&"".to_string()));
        assert!(env.get("Comment").is_none());
    }

    #[test]
    fn test_health_check_config_defaults() {
        let hc_config = HealthCheckConfig {
            http_url: Some("http://localhost:3000/health".to_string()),
            script: None,
            expected_status: vec![],
            interval_secs: None,
            timeout_secs: None,
            retries: None,
        };

        let hc = hc_config.into_health_check();
        assert_eq!(hc.http_url, Some("http://localhost:3000/health".to_string()));
        // Should use defaults
        assert_eq!(hc.expected_status, vec![200]);
        assert_eq!(hc.interval_secs, 30);
        assert_eq!(hc.timeout_secs, 5);
        assert_eq!(hc.retries, 3);
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
}
