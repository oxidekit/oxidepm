//! CLI argument definitions

use clap::{Parser, Subcommand, Args, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "oxidepm")]
#[command(version, about = "PM2-like process manager for Rust and Node.js")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Number of instances for clustering
    #[arg(short, long, default_value = "1", global = true)]
    pub instances: u32,

    /// Port assignment for the process
    #[arg(long, global = true)]
    pub port: Option<u16>,

    /// HTTP health check URL endpoint
    #[arg(long, global = true)]
    pub health_check: Option<String>,

    /// Output in JSON format instead of tables
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start a process
    Start(StartArgs),

    /// Stop process(es)
    Stop {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Restart process(es)
    Restart {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Remove process(es) from list
    Delete {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Show process status table
    Status,

    /// Show detailed info for a process
    Show {
        /// Process name or id
        selector: String,
    },

    /// View process logs
    Logs(LogsArgs),

    /// Check daemon health
    Ping,

    /// Save current process list
    Save,

    /// Restore saved processes
    Resurrect,

    /// Stop daemon and all processes
    Kill,

    /// Generate startup script
    Startup {
        /// Target system
        #[arg(value_enum)]
        target: Option<StartupTarget>,
    },

    /// Launch TUI dashboard for monitoring processes
    Monit,

    /// Start Web API server
    Web(WebArgs),

    /// Graceful restart of process(es)
    Reload {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Clear/truncate log files for process(es)
    Flush {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Show what command would run without starting
    Describe {
        /// Process name, id, or target file/directory
        target: String,
    },

    /// Configure notifications (Telegram, etc.)
    Notify(NotifyArgs),

    /// Check if a project is ready to run (dependencies, configs, env)
    Check(CheckArgs),
}

#[derive(Args)]
pub struct NotifyArgs {
    #[command(subcommand)]
    pub command: NotifyCommand,
}

#[derive(Subcommand)]
pub enum NotifyCommand {
    /// Configure Telegram notifications
    Telegram {
        /// Bot token from @BotFather
        #[arg(long)]
        token: String,

        /// Chat ID to send messages to
        #[arg(long)]
        chat: String,
    },

    /// Remove notification configuration
    Remove {
        /// Channel to remove (e.g., "telegram")
        channel: String,
    },

    /// Set which events to notify on
    Events {
        /// Events to notify (comma-separated: start,stop,crash,restart,memory_limit,health_check)
        #[arg(long)]
        set: String,
    },

    /// Show current notification configuration
    Status,

    /// Test notifications by sending a test message
    Test,
}

#[derive(Args)]
pub struct StartArgs {
    /// Target: file, directory, or config file (optional if --git is used)
    pub target: Option<String>,

    /// Clone and start from a git repository URL
    #[arg(long)]
    pub git: Option<String>,

    /// Git branch to clone (default: main/master)
    #[arg(long)]
    pub branch: Option<String>,

    /// Directory to clone into (default: ~/.oxidepm/repos/<name> or current dir)
    #[arg(long = "clone-dir")]
    pub clone_dir: Option<PathBuf>,

    /// Process name
    #[arg(short, long)]
    pub name: Option<String>,

    /// Working directory
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Environment variable (KEY=VALUE, repeatable)
    #[arg(long = "env", value_parser = parse_env)]
    pub envs: Vec<(String, String)>,

    /// Environment file path
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// Enable watch mode
    #[arg(long)]
    pub watch: bool,

    /// Ignore pattern for watch (repeatable)
    #[arg(long)]
    pub ignore: Vec<String>,

    /// Restart delay in ms
    #[arg(long, default_value = "500")]
    pub restart_delay: u64,

    /// Max restarts before errored
    #[arg(long, default_value = "15")]
    pub max_restarts: u32,

    /// Kill timeout in ms
    #[arg(long, default_value = "3000")]
    pub kill_timeout: u64,

    /// Disable auto-restart
    #[arg(long)]
    pub no_autorestart: bool,

    /// Force mode: rust, cargo, node, npm, pnpm, yarn, cmd
    #[arg(long)]
    pub mode: Option<String>,

    /// Script name for npm/pnpm/yarn mode
    #[arg(long)]
    pub script: Option<String>,

    /// Binary name for cargo mode
    #[arg(long)]
    pub bin: Option<String>,

    /// Tag for process grouping (repeatable, use @tag to select)
    #[arg(long)]
    pub tag: Vec<String>,

    /// Maximum uptime before auto-restart (e.g., "1h", "24h", "30m")
    #[arg(long, value_parser = parse_duration)]
    pub max_uptime: Option<u64>,

    /// Startup delay in milliseconds (wait before starting the process)
    #[arg(long = "delay")]
    pub startup_delay: Option<u64>,

    /// Inherit environment variables from parent process
    #[arg(long)]
    pub env_inherit: bool,

    /// Script to run after process starts
    #[arg(long)]
    pub on_start: Option<String>,

    /// Script to run after process stops
    #[arg(long)]
    pub on_stop: Option<String>,

    /// Script to run after restart
    #[arg(long)]
    pub on_restart: Option<String>,

    /// Script to run when process crashes
    #[arg(long)]
    pub on_crash: Option<String>,

    /// Auto-setup: install dependencies, create .env from template before starting
    #[arg(long)]
    pub setup: bool,

    /// Skip preflight checks (not recommended)
    #[arg(long)]
    pub no_check: bool,

    /// Additional arguments passed to process
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct LogsArgs {
    /// Process name or id
    pub selector: String,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Number of lines to show
    #[arg(long, default_value = "15")]
    pub lines: usize,

    /// Show only stdout
    #[arg(long)]
    pub out: bool,

    /// Show only stderr
    #[arg(long)]
    pub err: bool,

    /// Filter log lines by regex pattern
    #[arg(long)]
    pub grep: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum StartupTarget {
    Systemd,
    Launchd,
}

#[derive(Args)]
pub struct WebArgs {
    /// Port for the Web API server
    #[arg(short, long, default_value = "9615")]
    pub port: u16,

    /// API key for authentication (optional)
    #[arg(long)]
    pub api_key: Option<String>,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Target: file or directory to check
    pub target: String,

    /// Auto-fix issues (run npm install, create .env from template)
    #[arg(long)]
    pub fix: bool,

    /// Set environment variable (can be used multiple times, KEY=VALUE format)
    #[arg(long = "set-env", value_parser = parse_env)]
    pub set_envs: Vec<(String, String)>,
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    let pos = s.find('=').ok_or("Expected KEY=VALUE format")?;
    Ok((s[..pos].to_string(), s[pos + 1..].to_string()))
}

/// Parse duration strings like "1h", "30m", "2d", "24h30m" into seconds
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err("Empty duration string".to_string());
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            if current_num.is_empty() {
                return Err(format!("Invalid duration format: {}", s));
            }
            let num: u64 = current_num
                .parse()
                .map_err(|_| format!("Invalid number in duration: {}", current_num))?;
            current_num.clear();

            let multiplier = match c {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                _ => return Err(format!("Unknown duration unit: {}", c)),
            };
            total_secs += num * multiplier;
        }
    }

    // Handle plain numbers (assume seconds)
    if !current_num.is_empty() {
        let num: u64 = current_num
            .parse()
            .map_err(|_| format!("Invalid number in duration: {}", current_num))?;
        total_secs += num;
    }

    if total_secs == 0 {
        return Err("Duration must be greater than 0".to_string());
    }

    Ok(total_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration("24h").unwrap(), 86400);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("1m").unwrap(), 60);
        assert_eq!(parse_duration("60m").unwrap(), 3600);
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("120s").unwrap(), 120);
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert_eq!(parse_duration("7d").unwrap(), 604800);
    }

    #[test]
    fn test_parse_duration_combined() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("2h30m30s").unwrap(), 9030);
        assert_eq!(parse_duration("1d12h").unwrap(), 129600);
    }

    #[test]
    fn test_parse_duration_plain_number() {
        // Plain numbers are treated as seconds
        assert_eq!(parse_duration("3600").unwrap(), 3600);
        assert_eq!(parse_duration("60").unwrap(), 60);
    }

    #[test]
    fn test_parse_duration_case_insensitive() {
        assert_eq!(parse_duration("1H").unwrap(), 3600);
        assert_eq!(parse_duration("30M").unwrap(), 1800);
        assert_eq!(parse_duration("1D").unwrap(), 86400);
    }

    #[test]
    fn test_parse_duration_errors() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1x").is_err());
    }

    #[test]
    fn test_parse_env() {
        let (key, value) = parse_env("FOO=bar").unwrap();
        assert_eq!(key, "FOO");
        assert_eq!(value, "bar");

        let (key, value) = parse_env("COMPLEX_VAR=value=with=equals").unwrap();
        assert_eq!(key, "COMPLEX_VAR");
        assert_eq!(value, "value=with=equals");

        assert!(parse_env("NO_EQUALS").is_err());
    }
}
