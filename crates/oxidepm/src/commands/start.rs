//! Start command implementation

use anyhow::{bail, Result};
use oxidepm_core::{
    constants, AppSpec, ConfigFile, ConfigFormat, HealthCheck, RestartPolicy, Runtime, Selector,
};
use oxidepm_ipc::{ItemOutcome, ItemResult, Request, Response};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cli::StartArgs;
use crate::commands::verb;
use crate::output::{is_json_mode, print_error, print_success};

pub async fn execute(mut args: StartArgs) -> Result<()> {
    // Clap places everything after the separator in the trailing process arguments.
    if args.target.is_empty() {
        if !args.check || args.args.is_empty() {
            bail!("No target specified. Use a file or directory path");
        }
        args.target = args.args.remove(0);
    }
    let target = &args.target;

    // Determine what to start
    let target_path = Path::new(target);

    // Check if it's a config file
    if target_path.is_file() && ConfigFormat::from_path(target_path).is_some() {
        // Load config file and start all apps
        let unsupported = config_incompatible_flags(&args);
        if !unsupported.is_empty() {
            let guidance = if args.check {
                "Run `oxidepm check <project>` separately before starting"
            } else {
                "Set these options in the config file instead"
            };
            bail!(
                "{} cannot be combined with a config file (the config file defines the apps). {guidance}",
                unsupported.join(", ")
            );
        }
        return start_from_config(target_path).await;
    }

    if args.check {
        let cwd = std::env::current_dir()?.display().to_string();
        if !super::is_dry_run() && verb::find_verb("check").is_some() {
            drop(super::get_client().connect_or_start().await?);
        }
        let Some(done) = verb::invoke_verb(
            "check",
            vec!["--".into(), target.clone()],
            cwd,
            false,
            is_json_mode(),
        )
        .await?
        else {
            bail!(
                "{}",
                verb::missing_hint("check").expect("check has an adapter")
            );
        };
        if done.exit_code != 0 {
            bail!("Preflight checks failed");
        }
    }

    // Single app start
    let spec = build_app_spec(&args)?;

    let response = start_spec(spec).await?;

    if let Some(done) = super::plan_shown(&response) {
        return done;
    }

    match response {
        Response::Started { id, name, .. } => {
            print_success(&format!("Started {} (id: {})", name, id));
            Ok(())
        }
        Response::Error { message, .. } => {
            print_error(&message);
            bail!(message)
        }
        _ => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
    }
}

pub(crate) async fn start_spec(spec: AppSpec) -> Result<Response> {
    super::send_autostart(&super::get_client(), &Request::Start { spec }).await
}

pub(crate) async fn start_batch(specs: Vec<AppSpec>) -> Result<Response> {
    if specs.is_empty() {
        bail!("No apps in config");
    }
    super::send_autostart(&super::get_client(), &Request::StartBatch { specs }).await
}

/// Flags of `start` that only make sense for a single app started from a
/// path. With a config file they would be silently ignored, so they are
/// reported and rejected instead.
fn config_incompatible_flags(args: &StartArgs) -> Vec<String> {
    let mut found: Vec<&str> = Vec::new();
    let mut check = |set: bool, flag: &'static str| {
        if set {
            found.push(flag);
        }
    };
    check(args.name.is_some(), "--name");
    check(args.cwd.is_some(), "--cwd");
    check(!args.envs.is_empty(), "--env");
    check(args.env_file.is_some(), "--env-file");
    check(args.restart_delay.is_some(), "--restart-delay");
    check(args.max_restarts.is_some(), "--max-restarts");
    check(args.kill_timeout.is_some(), "--kill-timeout");
    check(args.no_autorestart, "--no-autorestart");
    check(args.mode.is_some(), "--mode");
    check(args.script.is_some(), "--script");
    check(args.bin.is_some(), "--bin");
    check(!args.tag.is_empty(), "--tag");
    check(args.max_memory.is_some(), "--max-memory");
    check(args.max_uptime.is_some(), "--max-uptime");
    check(args.startup_delay.is_some(), "--delay");
    check(args.env_inherit, "--env-inherit");
    check(args.on_start.is_some(), "--on-start");
    check(args.on_stop.is_some(), "--on-stop");
    check(args.on_restart.is_some(), "--on-restart");
    check(args.on_crash.is_some(), "--on-crash");
    check(args.check, "--check");
    check(args.instances.is_some(), "--instances");
    check(args.port.is_some(), "--port");
    check(args.health_check.is_some(), "--health-check");
    check(args.protected, "--protected");
    check(args.ai_control, "--ai-control");
    check(!args.args.is_empty(), "-- <args>");
    found.into_iter().map(String::from).collect()
}

pub(crate) fn load_config_specs(config_path: &Path) -> Result<Vec<AppSpec>> {
    let config = ConfigFile::load(config_path)?;
    let base_dir = absolute_dir(containing_dir(config_path))?;
    Ok(config.into_specs(&base_dir)?)
}

fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn absolute_dir(dir: &Path) -> std::io::Result<PathBuf> {
    if let Ok(canonical) = dir.canonicalize() {
        return Ok(canonical);
    }
    // Keep the path absolute even if the directory does not exist yet.
    if dir.is_absolute() {
        Ok(dir.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(dir))
    }
}

async fn start_from_config(config_path: &Path) -> Result<()> {
    let specs = load_config_specs(config_path)?;
    if specs.is_empty() {
        print_error("No apps defined in config file");
        bail!("No apps in config");
    }
    let total = specs.len();

    // One request for the whole file: the daemon checks the dependencies as a graph (an app may
    // be listed before the app it depends on) and every app is attempted, even after a failure,
    // so the summary is complete.
    let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    let response = start_batch(specs).await?;
    if let Some(done) = super::plan_shown(&response) {
        return done;
    }
    let (started, failed) = match response {
        Response::StartedBatch { items } => report_batch(&items, true),
        Response::Error { message, .. } => {
            print_error(&format!("Failed to start the apps: {}", message));
            (Vec::new(), names)
        }
        _ => {
            print_error("Unexpected response from daemon");
            (Vec::new(), names)
        }
    };

    if failed.is_empty() {
        println!("\nStarted {} apps", started.len());
        Ok(())
    } else {
        println!("\nStarted: {}, Failed: {}", started.len(), failed.len());
        bail!(
            "{} of {} apps failed to start (started: {}; failed: {})",
            failed.len(),
            total,
            if started.is_empty() {
                "none".to_string()
            } else {
                started.join(", ")
            },
            failed.join(", ")
        )
    }
}

pub(crate) fn report_batch(items: &[ItemResult], print: bool) -> (Vec<String>, Vec<String>) {
    let mut started = Vec::new();
    let mut failed = Vec::new();
    for item in items {
        match &item.result {
            ItemOutcome::Ok { detail, .. } => {
                if print {
                    let note = match detail.as_deref() {
                        Some("waiting") => ", waiting for its dependencies",
                        _ => "",
                    };
                    print_success(&format!("Started {} (id: {}{note})", item.name, item.id));
                }
                started.push(item.name.clone());
            }
            ItemOutcome::Err { message, .. } => {
                if print {
                    print_error(&format!("Failed to start {}: {message}", item.name));
                }
                failed.push(item.name.clone());
            }
        }
    }
    (started, failed)
}

/// `--health-check` takes an HTTP(S) URL, or otherwise a script to run.
fn health_check_from_arg(value: &str) -> HealthCheck {
    if value.starts_with("http://") || value.starts_with("https://") {
        HealthCheck::http(value)
    } else {
        HealthCheck::script(value)
    }
}

/// Names that `Selector::parse` would read as something other than a plain
/// name ("all", "@tag", "123") can never be targeted individually.
fn validate_cli_name(name: &str) -> Result<()> {
    if matches!(Selector::parse(name), Selector::ByName(_)) {
        Ok(())
    } else {
        bail!(
            "'{}' is a reserved name (\"all\", numbers and @tags are selectors); choose another with --name",
            name
        )
    }
}

fn build_app_spec(args: &StartArgs) -> Result<AppSpec> {
    let target = &args.target;
    let target_path = Path::new(target);
    let target_is_dir = target_path.is_dir();

    // Determine mode
    let mode = if let Some(mode_str) = &args.mode {
        mode_str.parse::<Runtime>()?
    } else if args.script.is_some() {
        // npm/pnpm/yarn mode
        Runtime::from_static("npm")
    } else {
        Runtime::detect(target_path).unwrap_or(Runtime::CMD)
    };

    // Determine working directory
    let dir = if let Some(cwd) = &args.cwd {
        cwd.as_path()
    } else if target_is_dir {
        target_path
    } else {
        containing_dir(target_path)
    };
    let cwd = absolute_dir(dir)?;

    // Determine command
    let command = match mode.as_str() {
        "npm" | "pnpm" | "yarn" => args.script.clone().unwrap_or_else(|| "start".to_string()),
        "cargo" => args.bin.clone().unwrap_or_else(|| ".".to_string()),
        _ if target_path.is_absolute() => target.clone(),
        _ if target_is_dir => ".".to_string(),
        _ => {
            let name = target_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(target);
            if mode.is_cmd() && (target.contains('/') || target_path.is_file()) {
                // A local executable is resolved against cwd instead of PATH.
                format!("./{name}")
            } else {
                name.to_string()
            }
        }
    };

    // Determine name
    let name = args.name.clone().unwrap_or_else(|| {
        if target_is_dir {
            target_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("app")
                .to_string()
        } else {
            target_path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("app")
                .to_string()
        }
    });

    validate_cli_name(&name)?;

    // Build environment
    let mut env: HashMap<String, String> = HashMap::new();

    // --env-inherit is not a copy of this shell's environment: it sets the spec's flag, and the
    // daemon gives the app its own environment (nothing from this shell is stored in the spec)

    // Overlay with env file if specified
    if let Some(env_file) = &args.env_file {
        if env_file.exists() {
            let content = std::fs::read_to_string(env_file)?;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(pos) = line.find('=') {
                    let key = line[..pos].trim().to_string();
                    let value = line[pos + 1..].trim().trim_matches('"').to_string();
                    env.insert(key, value);
                }
            }
        }
    }

    // Finally overlay with explicit --env arguments (highest priority)
    for (key, value) in args.envs.iter().cloned() {
        env.insert(key, value);
    }

    // --port reaches the child as PORT (cluster instances get PORT overwritten
    // per instance by the daemon); an explicit PORT in the env wins.
    if let Some(port) = args.port {
        env.entry("PORT".to_string())
            .or_insert_with(|| port.to_string());
    }

    // Build restart policy
    let restart_policy = RestartPolicy {
        auto_restart: !args.no_autorestart,
        max_restarts: args.max_restarts.unwrap_or(constants::DEFAULT_MAX_RESTARTS),
        restart_delay_ms: args
            .restart_delay
            .unwrap_or(constants::DEFAULT_RESTART_DELAY_MS),
        ..RestartPolicy::default()
    };

    Ok(AppSpec {
        id: 0, // Will be assigned by daemon
        name,
        mode,
        command,
        args: args.args.clone(),
        cwd,
        env,
        restart_policy,
        kill_timeout_ms: args
            .kill_timeout
            .unwrap_or(constants::DEFAULT_KILL_TIMEOUT_MS),
        created_at: chrono::Utc::now(),
        // Clustering
        instances: args.instances.unwrap_or(1),
        instance_id: None,
        // Port management
        port: args.port,
        port_range: None,
        // Health checks
        health_check: args.health_check.as_deref().map(health_check_from_arg),
        // Memory limit
        max_memory_mb: args.max_memory,
        // Startup delay
        startup_delay_ms: args.startup_delay,
        // The app gets the daemon's whole environment instead of a small base set
        env_inherit: args.env_inherit,
        // Event hooks
        hooks: oxidepm_core::Hooks {
            on_start: args.on_start.clone(),
            on_stop: args.on_stop.clone(),
            on_restart: args.on_restart.clone(),
            on_crash: args.on_crash.clone(),
            on_error: None,
        },
        // Process tags for grouping
        tags: args.tag.clone(),
        // Maximum uptime before auto-restart
        max_uptime_secs: args.max_uptime,
        // Restart mode
        restart_mode: oxidepm_core::RestartMode::default(),
        // Process dependencies
        depends_on: Vec::new(),
        depends_timeout_secs: oxidepm_core::DEFAULT_DEPENDS_TIMEOUT_SECS,
        depends_fail_open: false,
        // Cron restarts
        restart_cron: None,
        // Log rotation (use defaults unless set via config file)
        log_max_size: None,
        log_max_files: None,
        log_compress: false,
        adapters: Default::default(),
        protected: args.protected,
        ai_control: args.ai_control,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    fn parse_start(extra: &[&str]) -> StartArgs {
        let mut argv = vec!["oxidepm", "start"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).unwrap().command {
            Commands::Start(args) => args,
            _ => unreachable!(),
        }
    }

    #[test]
    fn start_flags_reach_the_spec() {
        let args = parse_start(&[
            "/bin/sleep",
            "-i",
            "3",
            "--port",
            "3000",
            "--health-check",
            "http://localhost:3000/health",
        ]);
        let spec = build_app_spec(&args).unwrap();
        assert_eq!(spec.instances, 3);
        assert_eq!(spec.port, Some(3000));
        assert_eq!(spec.env.get("PORT").map(String::as_str), Some("3000"));
        let hc = spec.health_check.expect("health check");
        assert_eq!(hc.http_url.as_deref(), Some("http://localhost:3000/health"));
    }

    #[test]
    fn cli_env_inherit_sets_flag_not_snapshot() {
        // this process certainly has PATH and HOME: neither may end up in the spec
        assert!(std::env::var_os("PATH").is_some());
        let spec = build_app_spec(&parse_start(&["/bin/sleep", "--env-inherit"])).unwrap();
        assert!(spec.env_inherit, "the flag is set");
        assert!(
            spec.env.is_empty(),
            "no snapshot of the CLI's environment: {:?}",
            spec.env.keys()
        );

        let spec = build_app_spec(&parse_start(&[
            "/bin/sleep",
            "--env-inherit",
            "--env",
            "A=1",
        ]))
        .unwrap();
        assert!(spec.env_inherit);
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env.get("A").map(String::as_str), Some("1"));

        let spec = build_app_spec(&parse_start(&["/bin/sleep"])).unwrap();
        assert!(!spec.env_inherit, "off by default");
    }

    #[test]
    fn explicit_env_port_wins_over_port_flag() {
        let spec = build_app_spec(&parse_start(&[
            "/bin/sleep",
            "--port",
            "3000",
            "--env",
            "PORT=4000",
        ]))
        .unwrap();
        assert_eq!(spec.env.get("PORT").map(String::as_str), Some("4000"));
        let spec = build_app_spec(&parse_start(&["/bin/sleep"])).unwrap();
        assert!(!spec.env.contains_key("PORT"));
    }

    #[test]
    fn health_check_accepts_a_script() {
        let args = parse_start(&["/bin/sleep", "--health-check", "./check-health.sh"]);
        let hc = build_app_spec(&args).unwrap().health_check.unwrap();
        assert_eq!(hc.script.as_deref(), Some("./check-health.sh"));
        assert!(hc.http_url.is_none());
    }

    #[test]
    fn start_defaults_are_unchanged() {
        let spec = build_app_spec(&parse_start(&["/bin/sleep"])).unwrap();
        assert_eq!(spec.instances, 1);
        assert_eq!(spec.port, None);
        assert!(spec.health_check.is_none());
        assert_eq!(
            spec.restart_policy.restart_delay_ms,
            constants::DEFAULT_RESTART_DELAY_MS
        );
        assert_eq!(
            spec.restart_policy.max_restarts,
            constants::DEFAULT_MAX_RESTARTS
        );
        assert_eq!(spec.kill_timeout_ms, constants::DEFAULT_KILL_TIMEOUT_MS);
    }

    #[test]
    fn explicit_restart_values_are_applied() {
        let spec = build_app_spec(&parse_start(&[
            "/bin/sleep",
            "--restart-delay",
            "10",
            "--max-restarts",
            "2",
            "--kill-timeout",
            "99",
        ]))
        .unwrap();
        assert_eq!(spec.restart_policy.restart_delay_ms, 10);
        assert_eq!(spec.restart_policy.max_restarts, 2);
        assert_eq!(spec.kill_timeout_ms, 99);
    }

    #[test]
    fn start_flags_are_not_global() {
        for argv in [
            vec!["oxidepm", "stop", "x", "--port", "1"],
            vec!["oxidepm", "status", "--health-check", "http://x/"],
            vec!["oxidepm", "restart", "x", "-i", "2"],
            vec!["oxidepm", "--port", "1", "status"],
        ] {
            assert!(
                Cli::try_parse_from(argv.clone()).is_err(),
                "{argv:?} must be rejected"
            );
        }
    }

    #[test]
    fn invalid_start_flag_values_are_rejected() {
        for extra in [
            vec!["-i", "0"],
            vec!["--port", "0"],
            vec!["--port", "70000"],
        ] {
            let mut argv = vec!["oxidepm", "start", "/bin/sleep"];
            argv.extend(extra.iter());
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{extra:?} must be rejected"
            );
        }
    }

    #[test]
    fn marker_flags_reach_the_spec() {
        let plain = build_app_spec(&parse_start(&["/bin/sleep"])).unwrap();
        assert!(!plain.protected && !plain.ai_control);
        let both =
            build_app_spec(&parse_start(&["/bin/sleep", "--protected", "--ai-control"])).unwrap();
        assert!(both.protected && both.ai_control);
        let one = build_app_spec(&parse_start(&["/bin/sleep", "--ai-control"])).unwrap();
        assert!(!one.protected && one.ai_control);
        // with a config file they are keys of the file, not flags
        let flags =
            config_incompatible_flags(&parse_start(&["cfg.toml", "--protected", "--ai-control"]));
        assert_eq!(flags, vec!["--protected", "--ai-control"]);
    }

    #[test]
    fn config_incompatible_flags_lists_everything_set() {
        assert!(config_incompatible_flags(&parse_start(&["cfg.toml"])).is_empty());
        assert_eq!(
            config_incompatible_flags(&parse_start(&["cfg.toml", "--check"])),
            ["--check"]
        );

        let flags = config_incompatible_flags(&parse_start(&[
            "cfg.toml",
            "--name",
            "x",
            "--port",
            "1",
            "--max-restarts",
            "15",
        ]));
        assert_eq!(flags, vec!["--name", "--max-restarts", "--port"]);
    }

    #[test]
    fn reserved_names_are_rejected() {
        for name in ["all", "ALL", "All", "@web", "42"] {
            let args = parse_start(&["/bin/sleep", "--name", name]);
            assert!(build_app_spec(&args).is_err(), "{name} must be rejected");
        }
        assert!(build_app_spec(&parse_start(&["/bin/sleep", "--name", "web-1"])).is_ok());
    }
}
