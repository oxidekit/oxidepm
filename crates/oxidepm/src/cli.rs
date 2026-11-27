//! CLI argument definitions

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "oxidepm")]
#[command(
    version,
    about = "Process manager written in Rust for any runtime, with adapter support"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Output in JSON format instead of tables
    #[arg(long, global = true)]
    pub json: bool,

    /// Show what the command would do and change nothing (with --json, the output is the plan)
    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,
}

/// What `--dry-run` means for a command.
pub enum DryRun {
    /// The daemon answers with a plan.
    Plan,
    /// The command rehearses itself (it runs in the CLI, not in the daemon).
    Own,
    /// The command only reads: there is nothing to rehearse.
    ReadOnly,
    /// The command changes something and has no rehearsal.
    None,
}

impl Commands {
    /// What `--dry-run` does for this command.
    pub fn dry_run(&self) -> DryRun {
        match self {
            // The preflight verb runs before the start request.
            Commands::Start(args) if args.check => DryRun::None,
            Commands::Start(_)
            | Commands::Stop { .. }
            | Commands::Restart { .. }
            | Commands::Reload { .. }
            | Commands::Delete { .. }
            | Commands::Flush { .. }
            | Commands::Save { .. }
            | Commands::Resurrect
            | Commands::Kill
            | Commands::Policy { .. } => DryRun::Plan,
            Commands::Startup { .. }
            | Commands::Update { .. }
            | Commands::Backup { .. }
            | Commands::Restore { .. } => DryRun::Own,
            Commands::Env(args) => match args.command {
                EnvCommand::List { .. } => DryRun::ReadOnly,
                _ => DryRun::None,
            },
            Commands::Status { .. }
            | Commands::Show { .. }
            | Commands::Logs(_)
            | Commands::Audit(_)
            | Commands::Ping
            | Commands::Describe { .. }
            | Commands::Top
            | Commands::Completions { .. }
            | Commands::ManPage => DryRun::ReadOnly,
            Commands::Adapter(args) => match args.command {
                // the rehearsal fetches, verifies and unpacks in the CLI and prints the preview
                AdapterCommand::Import(_) | AdapterCommand::Update(_) => DryRun::Own,
                // the daemon answers with a plan: the grant, the process, the state
                AdapterCommand::Enable(_)
                | AdapterCommand::Disable { .. }
                | AdapterCommand::Remove { .. } => DryRun::Plan,
                AdapterCommand::List { .. }
                | AdapterCommand::Search { .. }
                | AdapterCommand::Info { .. } => DryRun::ReadOnly,
            },
        }
    }

    /// The command's name as the user typed it, for messages.
    pub fn name(&self) -> String {
        let name = match self {
            Commands::Start(args) if args.check => "start --check",
            Commands::Start(_) => "start",
            Commands::Stop { .. } => "stop",
            Commands::Restart { .. } => "restart",
            Commands::Delete { .. } => "delete",
            Commands::Status { .. } => "status",
            Commands::Policy { .. } => "policy",
            Commands::Show { .. } => "show",
            Commands::Logs(_) => "logs",
            Commands::Audit(_) => "audit",
            Commands::Ping => "ping",
            Commands::Save { .. } => "save",
            Commands::Resurrect => "resurrect",
            Commands::Kill => "kill",
            Commands::Startup { .. } => "startup",
            Commands::Reload { .. } => "reload",
            Commands::Flush { .. } => "flush",
            Commands::Describe { .. } => "describe",
            Commands::Update { .. } => "update",
            Commands::Env(_) => "env",
            Commands::Top => "top",
            Commands::Completions { .. } => "completions",
            Commands::ManPage => "man",
            Commands::Backup { .. } => "backup",
            Commands::Restore { .. } => "restore",
            Commands::Adapter(_) => "adapter",
        };
        name.to_string()
    }
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // Parsed once per invocation.
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
    Status {
        /// Show extended info (cwd, port, policy markers)
        #[arg(long)]
        more: bool,
        /// Show how the daemon itself is (audit log, agent socket, warnings) instead of the apps
        #[arg(long, conflicts_with = "more")]
        daemon: bool,
    },

    /// Change an app's policy markers without restarting it (see docs/AI-SAFETY.md)
    Policy {
        #[command(subcommand)]
        command: PolicyCommands,
    },

    /// Show detailed info for a process
    Show {
        /// Process name or id
        selector: String,

        /// Show environment values and hook scripts instead of markers
        #[arg(long)]
        reveal: bool,
    },

    /// View process logs
    Logs(LogsArgs),

    /// Read the audit log: what AI agents and adapters did, and every caution or dangerous request
    Audit(AuditArgs),

    /// Check daemon health
    Ping,

    /// Save current process list
    Save {
        /// Replace a non-empty saved file even when nothing is running
        #[arg(long)]
        force: bool,
    },

    /// Restore saved processes
    Resurrect,

    /// Stop daemon and all processes
    Kill,

    /// Configure oxidepmd to start at boot with systemd
    Startup {
        /// Service manager
        #[arg(value_enum)]
        target: Option<StartupTarget>,
    },

    /// Restart process(es), same as restart (there is no zero-downtime reload)
    Reload {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Clear/truncate log files for process(es).
    /// Log rotation is automatic (default: 10MB max, 5 files).
    /// Configure per-process in config file: log_max_size, log_max_files, log_compress.
    Flush {
        /// Process name, id, or "all"
        selector: String,
    },

    /// Show what command would run without starting
    Describe {
        /// Process name or id
        target: String,
    },

    /// Update oxidepm to the latest release (or a specific version)
    Update {
        /// Target version (e.g., "0.4.0"). Defaults to latest.
        #[arg(long)]
        version: Option<String>,
        /// Install a version older than the one running
        #[arg(long)]
        force: bool,
    },

    /// Manage process environment variables
    Env(EnvArgs),

    /// Real-time process resource monitor (like htop for oxidepm)
    Top,

    /// Generate shell completions (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Shell type
        shell: String,
    },

    /// Generate man page
    #[command(name = "man")]
    ManPage,

    /// Back up the saved process list, state database, and adapter registry to a tarball
    Backup {
        /// Output file (default: oxidepm-backup.tar.gz)
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Restore from a backup tarball (the daemon must not be running)
    Restore {
        /// Backup file to restore from
        file: String,

        /// Overwrite existing files without asking
        #[arg(short, long)]
        yes: bool,
    },

    /// Find, inspect and import adapters (separate programs that add features)
    Adapter(AdapterArgs),
}

#[derive(Args)]
pub struct AdapterArgs {
    #[command(subcommand)]
    pub command: AdapterCommand,
}

#[derive(Subcommand)]
pub enum AdapterCommand {
    /// Fetch an adapter, verify it and show what it needs (nothing runs)
    Import(ImportArgs),

    /// List the installed adapters (or, with --available, what the index offers)
    List {
        /// List what the signed index offers instead of what is installed
        #[arg(long)]
        available: bool,
    },

    /// Update installed adapters to a newer release, or go back to the previous one
    Update(UpdateArgs),

    /// Enable an installed adapter: give it the rights you accept and start it
    Enable(EnableArgs),

    /// Disable an adapter: stop it and take its rights away
    Disable {
        /// Adapter name
        name: String,
    },

    /// Remove an adapter's installed files (its state directory is kept unless --purge)
    Remove {
        /// Adapter name
        name: String,

        /// Also delete the adapter's state directory (it may hold tokens and keys)
        #[arg(long)]
        purge: bool,
    },

    /// Search the index by name, summary, tag or description
    Search {
        /// Text to look for
        query: String,
    },

    /// Show an installed adapter (or, if it is not installed, what the index says about it)
    Info {
        /// Adapter name
        name: String,
    },
}

#[derive(Args)]
pub struct ImportArgs {
    /// An adapter name (from the signed index), or a file or https URL of an archive you pin
    /// with --sha256. With --bundled, the names to take from the bundle (default: all).
    pub names: Vec<String>,

    /// Take the adapters from the bundle that this release carries
    #[arg(long)]
    pub bundled: bool,

    /// With --bundled: only the adapters that have this tag (for example `runtime`)
    #[arg(long)]
    pub tag: Option<String>,

    /// The sha256 of the archive you trust (for a file or URL); without it the hash is printed
    /// and nothing is installed
    #[arg(long)]
    pub sha256: Option<String>,

    /// Allow an adapter that is not signed to use a name the core reserves for official adapters
    #[arg(long = "allow-reserved-name")]
    pub allow_reserved_name: bool,

    /// Enable the adapter right after it is installed (the same consent as `adapter enable`)
    #[arg(long)]
    pub enable: bool,

    /// With --enable: accept without asking, only for an adapter that needs nothing broad
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// The adapter to update (or use --all)
    pub name: Option<String>,

    /// Update every installed adapter
    #[arg(long)]
    pub all: bool,

    /// Install this version instead of the newest (a lower one needs --allow-downgrade)
    #[arg(long)]
    pub version: Option<String>,

    /// Go back to the version before the last update, without the network
    #[arg(long)]
    pub rollback: bool,

    /// The fingerprint of the new release, when it asks for more than the installed one
    #[arg(long)]
    pub accept: Option<String>,

    /// Allow a version lower than the installed one
    #[arg(long = "allow-downgrade")]
    pub allow_downgrade: bool,

    /// Allow the new release to come from another source than the installed one (a pinned adapter
    /// from the index, or the other way round)
    #[arg(long = "switch-source")]
    pub switch_source: bool,

    /// For an adapter you pinned: the file or https URL of the new archive (with --sha256)
    #[arg(long)]
    pub from: Option<String>,

    /// The sha256 of the new archive, for an adapter you pinned
    #[arg(long)]
    pub sha256: Option<String>,
}

#[derive(Args)]
pub struct EnableArgs {
    /// Adapter name
    pub name: String,

    /// The fingerprint of the release you read the preview of (printed by `import` and `info`)
    #[arg(long)]
    pub accept: Option<String>,

    /// Accept without asking, only for an adapter that needs nothing broad
    #[arg(long)]
    pub yes: bool,

    /// Give the adapter a lower host level than it asks for (read or lifecycle)
    #[arg(long)]
    pub level: Option<String>,

    /// Give the adapter a narrower scope than it asks for (opted-in: only the apps that list it)
    #[arg(long)]
    pub scope: Option<String>,

    /// Accept that the OS cannot enforce something the adapter requires to be enforced
    #[arg(long = "allow-unenforced")]
    pub allow_unenforced: bool,
}

#[derive(Subcommand)]
pub enum PolicyCommands {
    /// Set the markers of process(es); an instance follows its cluster
    Set {
        /// Process name, id, or "all"
        selector: String,

        /// Read-only for AI actors, whatever --ai-control says (--protected or --protected=false)
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true",
              required_unless_present = "ai_control")]
        protected: Option<bool>,

        /// AI actors may stop, restart and reload it (--ai-control or --ai-control=false)
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        ai_control: Option<bool>,
    },
}

#[derive(Args)]
pub struct StartArgs {
    /// Target: file, directory, or config file
    // The target in `start --check -- TARGET` arrives as a trailing argument.
    #[arg(
        required = false,
        required_unless_present = "args",
        default_value_if("args", clap::builder::ArgPredicate::IsPresent, Some(""))
    )]
    pub target: String,

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

    /// Restart delay in ms (default: 500)
    #[arg(long)]
    pub restart_delay: Option<u64>,

    /// Max restarts before errored (default: 15)
    #[arg(long)]
    pub max_restarts: Option<u32>,

    /// Kill timeout in ms (default: 3000)
    #[arg(long)]
    pub kill_timeout: Option<u64>,

    /// Disable auto-restart
    #[arg(long)]
    pub no_autorestart: bool,

    /// Force the runtime: cmd, node, npm, pnpm, yarn, cargo, rust, python, go, or any other
    /// runtime an installed adapter provides (`--runtime` is the same option)
    #[arg(long, alias = "runtime")]
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

    /// Maximum memory in MB before auto-restart (e.g., 512)
    #[arg(long)]
    pub max_memory: Option<u64>,

    /// Maximum uptime before auto-restart (e.g., "1h", "24h", "30m")
    #[arg(long, value_parser = parse_duration)]
    pub max_uptime: Option<u64>,

    /// Startup delay in milliseconds (wait before starting the process)
    #[arg(long = "delay")]
    pub startup_delay: Option<u64>,

    /// Give the app the daemon's whole environment (by default it gets PATH, HOME and a few
    /// other basics, plus --env and --env-file)
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

    /// Run the check adapter before starting
    #[arg(long)]
    pub check: bool,

    /// Number of instances (cluster mode, default: 1)
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..=256))]
    pub instances: Option<u32>,

    /// Base port for the process (instance N gets port + N)
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,

    /// Health check: an http(s):// URL, or a script/command to run
    #[arg(long)]
    pub health_check: Option<String>,

    /// Mark the app protected: read-only for AI actors, whatever --ai-control says
    #[arg(long)]
    pub protected: bool,

    /// Let AI actors stop, restart and reload this app (they can never start or delete it)
    #[arg(long)]
    pub ai_control: bool,

    /// Additional arguments passed to process
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct AuditArgs {
    /// How many of the newest matching lines to show
    #[arg(long, default_value_t = 50)]
    pub tail: usize,

    /// Only requests of this actor: `ai:example-agent`, or `ai` for a class, or `example-agent` for a label
    #[arg(long)]
    pub actor: Option<String>,

    /// Only this operation (`restart`, `stop`, `delete`, `show`, ...)
    #[arg(long)]
    pub op: Option<String>,

    /// Only lines since this long ago (`30m`, `2h`, `1d`) or since this RFC 3339 time
    #[arg(long)]
    pub since: Option<String>,

    /// Read the security file (denials, dangerous requests, what non-humans changed)
    #[arg(long)]
    pub security: bool,
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
}

#[derive(Args)]
pub struct EnvArgs {
    #[command(subcommand)]
    pub command: EnvCommand,
}

#[derive(Subcommand)]
pub enum EnvCommand {
    /// List environment variables for a process
    List {
        /// Process name or id
        selector: String,
    },
    /// Set an environment variable for a process
    Set {
        /// Process name or id
        selector: String,
        /// KEY=VALUE pair
        #[arg(value_parser = parse_env)]
        pair: (String, String),
    },
    /// Remove an environment variable
    Unset {
        /// Process name or id
        selector: String,
        /// Variable name to remove
        key: String,
    },
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    let pos = s.find('=').ok_or("Expected KEY=VALUE format")?;
    Ok((s[..pos].to_string(), s[pos + 1..].to_string()))
}

/// Parse duration strings like "1h", "30m", "2d", "24h30m" into seconds
pub(crate) fn parse_duration(s: &str) -> Result<u64, String> {
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

/// Add migration guidance to clap's error for the old start flags.
pub(crate) fn removed_start_flag_hint(error: &clap::Error) -> Option<&'static str> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    if error.kind() != ErrorKind::UnknownArgument {
        return None;
    }
    let ContextValue::String(flag) = error.get(ContextKind::InvalidArg)? else {
        return None;
    };
    let hint = match flag.split('=').next()? {
        "--git" | "--branch" | "--clone-dir" => {
            "Git checkout options moved to the git-start adapter. Use oxidepm git-start <URL> [--branch BRANCH]. Install it with oxidepm adapter import git-start"
        }
        "--setup" => "--setup was removed. Use the check adapter: oxidepm check <target> --fix",
        "--no-check" => "--no-check was removed. The check adapter is optional. Checks now run only with --check",
        _ => return None,
    };
    let command = std::env::args_os()
        .skip(1)
        .find(|arg| !arg.to_string_lossy().starts_with('-'));
    (command.as_deref() == Some(std::ffi::OsStr::new("start"))).then_some(hint)
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
