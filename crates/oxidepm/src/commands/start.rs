//! Start command implementation

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;
use oxidepm_core::{AppMode, AppSpec, ConfigFile, RestartPolicy, constants};
use oxidepm_ipc::{Request, Response};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::StartArgs;
use crate::commands::check::{run_preflight_checks, check_port_conflict, CheckStatus};
use crate::output::{print_error, print_success};

pub async fn execute(mut args: StartArgs) -> Result<()> {
    let client = super::get_client();

    // Handle --git flag: clone repo first
    if let Some(git_url) = &args.git {
        let cloned_dir = clone_git_repo(git_url, args.branch.as_deref(), args.clone_dir.as_ref())?;
        // Set target to the cloned directory
        args.target = Some(cloned_dir.display().to_string());
        // Imply --setup when using --git
        args.setup = true;
    }

    // Ensure we have a target
    let target = args.target.as_ref().ok_or_else(|| {
        anyhow::anyhow!("No target specified. Use a file/directory path or --git <url>")
    })?;

    // Determine what to start
    let target_path = Path::new(target);

    // Check if it's a config file
    if target_path.is_file()
        && (target.ends_with(".toml") || target.ends_with(".json"))
    {
        // Load config file and start all apps
        return start_from_config(&client, target_path, &args).await;
    }

    // Determine project directory for preflight checks
    let project_dir = if target_path.is_file() {
        target_path.parent().unwrap_or(Path::new("."))
    } else if target_path.is_dir() {
        target_path
    } else {
        Path::new(".")
    };

    // Run preflight checks unless --no-check is specified
    if !args.no_check {
        let summary = run_preflight_checks(project_dir, args.setup);

        // Print check results if there are issues
        if summary.warnings > 0 || summary.errors > 0 {
            if !args.setup {
                // Show what's wrong and suggest fix
                eprintln!("{}", "Cannot start - preflight checks failed:".red().bold());
                eprintln!();
                for result in &summary.results {
                    if result.status == CheckStatus::Warn || result.status == CheckStatus::Error {
                        let prefix = if result.status == CheckStatus::Error {
                            "[ERROR]".red()
                        } else {
                            "[WARN]".yellow()
                        };
                        eprintln!("  {} {}", prefix, result.message);
                    }
                }
                eprintln!();
                eprintln!("Fix with: {} {} {}",
                    "oxidepm start".cyan(),
                    target.cyan(),
                    "--setup".cyan()
                );
                eprintln!("Or run:   {} {} {}",
                    "oxidepm check".cyan(),
                    target.cyan(),
                    "--fix".cyan()
                );
                bail!("Preflight checks failed");
            } else {
                // --setup was provided, show what was fixed
                println!("{}", "Setting up project...".cyan());
                for result in &summary.results {
                    if result.status == CheckStatus::Fixed {
                        println!("  {} {}", "[FIX]".blue(), result.message);
                    }
                }
                // Re-check after fixes
                let recheck = run_preflight_checks(project_dir, false);
                if recheck.has_blocking_issues {
                    eprintln!();
                    eprintln!("{}", "Setup incomplete - some issues could not be fixed:".red());
                    for result in &recheck.results {
                        if result.status == CheckStatus::Error {
                            eprintln!("  {} {}", "[ERROR]".red(), result.message);
                        }
                    }
                    bail!("Setup failed");
                }
                println!("{}", "Setup complete!".green());
                println!();
            }
        }

        // Check for port conflicts (skip if user explicitly provided PORT)
        let user_provided_port = args.envs.iter().any(|(k, _)| k == "PORT");
        if !user_provided_port {
            if let Some(port_check) = check_port_conflict(project_dir) {
                if port_check.is_in_use {
                eprintln!("{} Port {} is already in use", "[WARN]".yellow(), port_check.desired_port);

                if let Some(available) = port_check.available_port {
                    // Try interactive prompt, fall back to suggesting command if not a terminal
                    let use_alternative = if atty::is(atty::Stream::Stdin) {
                        Confirm::new()
                            .with_prompt(format!("Would you like to run on port {} instead?", available))
                            .default(true)
                            .interact()
                            .unwrap_or(false)
                    } else {
                        // Non-interactive: suggest command and exit
                        eprintln!();
                        eprintln!("To use port {} instead, run:", available);
                        eprintln!("  {} {} {} {}",
                            "oxidepm start".cyan(),
                            target.cyan(),
                            "--env".cyan(),
                            format!("PORT={}", available).cyan()
                        );
                        bail!("Port {} is in use. Use --env PORT={} to use an alternative port",
                              port_check.desired_port, available);
                    };

                    if use_alternative {
                        // Add PORT env var to use the alternative port
                        args.envs.push(("PORT".to_string(), available.to_string()));
                        println!("{} Using port {}", "[OK]".green(), available);
                    } else {
                        bail!("Port {} is in use. Free the port or specify a different one with --env PORT=<port>", port_check.desired_port);
                    }
                } else {
                    bail!("Port {} is in use and no available ports found", port_check.desired_port);
                }
            }
        }
        }
    }

    // Single app start
    let spec = build_app_spec(&args)?;

    let response = client.send(&Request::Start { spec: spec.clone() }).await?;

    match response {
        Response::Started { id, name } => {
            print_success(&format!("Started {} (id: {})", name, id));
            Ok(())
        }
        Response::Error { message } => {
            print_error(&message);
            bail!(message)
        }
        _ => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
    }
}

/// Clone a git repository and return the path to the cloned directory
fn clone_git_repo(url: &str, branch: Option<&str>, clone_dir: Option<&PathBuf>) -> Result<PathBuf> {
    // Extract repo name from URL
    let repo_name = extract_repo_name(url)?;

    // Determine clone directory
    let target_dir = if let Some(dir) = clone_dir {
        dir.clone()
    } else {
        // Default: ~/.oxidepm/repos/<name>
        constants::repos_dir().join(&repo_name)
    };

    // Check if already cloned
    if target_dir.exists() {
        let git_dir = target_dir.join(".git");
        if git_dir.exists() {
            println!("{} {} (already cloned)", "[GIT]".blue(), repo_name);
            // Pull latest changes
            println!("  {} Pulling latest changes...", "→".dimmed());
            let pull_result = Command::new("git")
                .args(["pull", "--ff-only"])
                .current_dir(&target_dir)
                .output();

            match pull_result {
                Ok(output) if output.status.success() => {
                    println!("  {} Updated to latest", "✓".green());
                }
                _ => {
                    println!("  {} Could not pull (using existing)", "!".yellow());
                }
            }
            return Ok(target_dir);
        } else {
            // Directory exists but isn't a git repo - error
            bail!("Directory {} exists but is not a git repository", target_dir.display());
        }
    }

    // Create parent directory
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    println!("{} Cloning {}...", "[GIT]".blue(), url.cyan());

    // Build git clone command
    let mut git_args = vec!["clone", "--depth", "1"];

    if let Some(b) = branch {
        git_args.push("--branch");
        git_args.push(b);
    }

    git_args.push(url);
    git_args.push(target_dir.to_str().unwrap_or("."));

    let output = Command::new("git")
        .args(&git_args)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Git clone failed: {}", stderr.trim());
    }

    println!("  {} Cloned to {}", "✓".green(), target_dir.display());

    Ok(target_dir)
}

/// Extract repository name from git URL
fn extract_repo_name(url: &str) -> Result<String> {
    // Handle various URL formats:
    // https://github.com/user/repo.git
    // https://github.com/user/repo
    // git@github.com:user/repo.git
    // user/repo (shorthand)

    let url = url.trim_end_matches('/');
    let url = url.trim_end_matches(".git");

    // Get the last path component
    let name = url
        .rsplit('/')
        .next()
        .or_else(|| url.rsplit(':').next())
        .ok_or_else(|| anyhow::anyhow!("Could not extract repository name from URL: {}", url))?;

    // Validate name
    if name.is_empty() {
        bail!("Could not extract repository name from URL: {}", url);
    }

    Ok(name.to_string())
}

async fn start_from_config(
    client: &oxidepm_ipc::IpcClient,
    config_path: &Path,
    _args: &StartArgs,
) -> Result<()> {
    let config = ConfigFile::load(config_path)?;
    let base_dir = config_path.parent().unwrap_or(Path::new("."));

    let specs = config.into_specs(base_dir)?;

    if specs.is_empty() {
        print_error("No apps defined in config file");
        bail!("No apps in config");
    }

    let mut started = 0;
    let mut failed = 0;

    for spec in specs {
        let name = spec.name.clone();
        let response = client.send(&Request::Start { spec }).await?;

        match response {
            Response::Started { id, name } => {
                print_success(&format!("Started {} (id: {})", name, id));
                started += 1;
            }
            Response::Error { message } => {
                print_error(&format!("Failed to start {}: {}", name, message));
                failed += 1;
            }
            _ => {
                print_error(&format!("Unexpected response for {}", name));
                failed += 1;
            }
        }
    }

    if failed > 0 {
        println!("\nStarted: {}, Failed: {}", started, failed);
    } else {
        println!("\nStarted {} apps", started);
    }

    Ok(())
}

fn build_app_spec(args: &StartArgs) -> Result<AppSpec> {
    let target = args.target.as_ref().ok_or_else(|| {
        anyhow::anyhow!("No target specified")
    })?;
    let target_path = Path::new(target);

    // Determine mode
    let mode = if let Some(mode_str) = &args.mode {
        mode_str.parse::<AppMode>()?
    } else if args.script.is_some() {
        // npm/pnpm/yarn mode
        AppMode::Npm
    } else if target_path.is_dir() {
        AppMode::detect(target_path).unwrap_or(AppMode::Cmd)
    } else if let Some(detected) = AppMode::detect(target_path) {
        detected
    } else {
        AppMode::Cmd
    };

    // Determine working directory
    let cwd = if let Some(cwd) = &args.cwd {
        cwd.canonicalize().unwrap_or_else(|_| cwd.clone())
    } else if target_path.is_dir() {
        target_path.canonicalize().unwrap_or_else(|_| target_path.to_path_buf())
    } else if let Some(parent) = target_path.parent() {
        parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf())
    } else {
        std::env::current_dir()?
    };

    // Determine command
    let command = match mode {
        AppMode::Npm | AppMode::Pnpm | AppMode::Yarn => {
            args.script.clone().unwrap_or_else(|| "start".to_string())
        }
        AppMode::Cargo => {
            args.bin.clone().unwrap_or_else(|| ".".to_string())
        }
        _ => {
            if target_path.is_absolute() {
                target.clone()
            } else if target_path.is_dir() {
                ".".to_string()
            } else {
                target_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(target)
                    .to_string()
            }
        }
    };

    // Determine name
    let name = args.name.clone().unwrap_or_else(|| {
        if target_path.is_dir() {
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

    // Build environment
    let mut env: HashMap<String, String> = HashMap::new();

    // If env_inherit is set, start with parent process environment
    if args.env_inherit {
        for (key, value) in std::env::vars() {
            env.insert(key, value);
        }
    }

    // Then overlay with env file if specified
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

    // Build ignore patterns
    let mut ignore_patterns: Vec<String> = oxidepm_core::DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    ignore_patterns.extend(args.ignore.clone());

    // Build restart policy
    let restart_policy = RestartPolicy {
        auto_restart: !args.no_autorestart,
        max_restarts: args.max_restarts,
        restart_delay_ms: args.restart_delay,
        crash_window_secs: 60,
    };

    Ok(AppSpec {
        id: 0, // Will be assigned by daemon
        name,
        mode,
        command,
        args: args.args.clone(),
        cwd,
        env,
        watch: args.watch,
        ignore_patterns,
        restart_policy,
        kill_timeout_ms: args.kill_timeout,
        created_at: chrono::Utc::now(),
        // Clustering
        instances: 1,
        instance_id: None,
        // Port management
        port: None,
        port_range: None,
        // Health checks
        health_check: None,
        // Memory limit
        max_memory_mb: None,
        // Startup delay
        startup_delay_ms: args.startup_delay,
        // Environment inheritance flag (for reference)
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
    })
}
