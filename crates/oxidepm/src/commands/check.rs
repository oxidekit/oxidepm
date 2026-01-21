//! Check command implementation - validates project readiness before running

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::CheckArgs;
use crate::output::is_json_mode;

/// Summary of preflight check results - used by start command
#[derive(Debug)]
pub struct PreflightSummary {
    pub results: Vec<CheckResult>,
    pub warnings: usize,
    pub errors: usize,
    pub has_blocking_issues: bool,
}

impl PreflightSummary {
    /// Check if there are issues that would prevent starting
    pub fn can_start(&self) -> bool {
        !self.has_blocking_issues
    }

    /// Get a user-friendly error message for blocking issues
    pub fn blocking_message(&self) -> String {
        let blocking: Vec<&CheckResult> = self.results.iter()
            .filter(|r| r.status == CheckStatus::Warn || r.status == CheckStatus::Error)
            .collect();

        let mut msg = String::from("Cannot start - missing dependencies:\n");
        for result in blocking {
            msg.push_str(&format!("  - {}\n", result.message));
        }
        msg
    }
}

/// Run preflight checks on a project directory (called by both check and start commands)
pub fn run_preflight_checks(project_dir: &Path, auto_fix: bool) -> PreflightSummary {
    let mut results: Vec<CheckResult> = Vec::new();

    // Detect project type
    let project_type = detect_project_type(project_dir);

    // Run checks based on project type
    match project_type {
        ProjectType::NodeJs => {
            check_nodejs_project(project_dir, auto_fix, &mut results);
        }
        ProjectType::Cargo => {
            check_cargo_project(project_dir, auto_fix, &mut results);
        }
        ProjectType::Generic => {
            // Still run generic checks
        }
    }

    // Run generic checks for all project types
    check_env_files(project_dir, auto_fix, &mut results);

    // Calculate statistics
    let warnings = results.iter().filter(|r| r.status == CheckStatus::Warn).count();
    let errors = results.iter().filter(|r| r.status == CheckStatus::Error).count();

    // Blocking issues are warnings about missing deps (node_modules) or errors
    let has_blocking_issues = results.iter().any(|r| {
        r.status == CheckStatus::Error ||
        (r.status == CheckStatus::Warn && r.message.contains("node_modules"))
    });

    PreflightSummary {
        results,
        warnings,
        errors,
        has_blocking_issues,
    }
}

/// Result of a single check
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

/// Status of a single check result
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
    Info,
    Fixed,
}

impl CheckStatus {
    fn prefix(&self) -> String {
        match self {
            CheckStatus::Ok => format!("[{}]", "OK".green()),
            CheckStatus::Warn => format!("[{}]", "WARN".yellow()),
            CheckStatus::Error => format!("[{}]", "ERROR".red()),
            CheckStatus::Info => format!("[{}]", "INFO".cyan()),
            CheckStatus::Fixed => format!("[{}]", "FIX".blue()),
        }
    }
}

/// JSON output for check results
#[derive(Serialize)]
struct CheckOutput {
    project: String,
    project_type: String,
    checks: Vec<CheckResult>,
    warnings: usize,
    errors: usize,
    fixed: usize,
}

/// Project type detected
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectType {
    NodeJs,
    Cargo,
    Generic,
}

impl ProjectType {
    fn as_str(&self) -> &'static str {
        match self {
            ProjectType::NodeJs => "nodejs",
            ProjectType::Cargo => "cargo",
            ProjectType::Generic => "generic",
        }
    }
}

pub async fn execute(args: CheckArgs) -> Result<()> {
    let target_path = PathBuf::from(&args.target).canonicalize().unwrap_or_else(|_| {
        PathBuf::from(&args.target)
    });

    // Determine the project directory
    let project_dir = if target_path.is_file() {
        target_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        target_path.clone()
    };

    if !project_dir.exists() {
        eprintln!("{} Directory does not exist: {}", "[ERROR]".red(), project_dir.display());
        std::process::exit(1);
    }

    let mut results: Vec<CheckResult> = Vec::new();

    // Detect project type
    let project_type = detect_project_type(&project_dir);

    if !is_json_mode() {
        println!("Checking project: {}\n", project_dir.display().to_string().cyan());
    }

    // Run checks based on project type
    match project_type {
        ProjectType::NodeJs => {
            check_nodejs_project(&project_dir, args.fix, &mut results);
        }
        ProjectType::Cargo => {
            check_cargo_project(&project_dir, args.fix, &mut results);
        }
        ProjectType::Generic => {
            // Still run generic checks
        }
    }

    // Run generic checks for all project types
    check_env_files(&project_dir, args.fix, &mut results);
    check_config_files(&project_dir, &mut results);

    // Handle --set-env
    if !args.set_envs.is_empty() {
        handle_set_env(&project_dir, &args.set_envs, &mut results);
    }

    // Calculate statistics
    let warnings = results.iter().filter(|r| r.status == CheckStatus::Warn).count();
    let errors = results.iter().filter(|r| r.status == CheckStatus::Error).count();
    let fixed = results.iter().filter(|r| r.status == CheckStatus::Fixed).count();

    // Output results
    if is_json_mode() {
        let output = CheckOutput {
            project: project_dir.display().to_string(),
            project_type: project_type.as_str().to_string(),
            checks: results.clone(),
            warnings,
            errors,
            fixed,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for result in &results {
            println!("{} {}", result.status.prefix(), result.message);
            if let Some(hint) = &result.fix_hint {
                if result.status == CheckStatus::Warn || result.status == CheckStatus::Error {
                    println!("       {}", hint.dimmed());
                }
            }
        }

        println!();

        if errors > 0 {
            println!("Issues: {} {}, {} {}",
                errors, "error(s)".red(),
                warnings, "warning(s)".yellow()
            );
            if !args.fix {
                println!("Run with {} to auto-resolve", "--fix".cyan());
            }
        } else if warnings > 0 {
            println!("Issues: {} {}", warnings, "warning(s)".yellow());
            if !args.fix {
                println!("Run with {} to auto-resolve", "--fix".cyan());
            }
        } else {
            println!("{}", "All checks passed!".green());
        }
    }

    Ok(())
}

fn detect_project_type(dir: &Path) -> ProjectType {
    if dir.join("package.json").exists() {
        ProjectType::NodeJs
    } else if dir.join("Cargo.toml").exists() {
        ProjectType::Cargo
    } else {
        ProjectType::Generic
    }
}

fn check_nodejs_project(dir: &Path, fix: bool, results: &mut Vec<CheckResult>) {
    let package_json = dir.join("package.json");

    // Check package.json exists
    if package_json.exists() {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "package.json found".to_string(),
            fix_hint: None,
        });
    } else {
        results.push(CheckResult {
            status: CheckStatus::Error,
            message: "package.json not found".to_string(),
            fix_hint: Some("Run `npm init` to create package.json".to_string()),
        });
        return;
    }

    // Check node_modules exists
    let node_modules = dir.join("node_modules");
    if node_modules.exists() && node_modules.is_dir() {
        // Count packages
        let package_count = fs::read_dir(&node_modules)
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: format!("node_modules/ found ({} packages)", package_count),
            fix_hint: None,
        });
    } else if fix {
        // Attempt to run npm install
        results.push(CheckResult {
            status: CheckStatus::Fixed,
            message: "Running npm install...".to_string(),
            fix_hint: None,
        });

        let install_result = run_npm_install(dir);
        match install_result {
            Ok(count) => {
                results.push(CheckResult {
                    status: CheckStatus::Ok,
                    message: format!("node_modules/ installed ({} packages)", count),
                    fix_hint: None,
                });
            }
            Err(e) => {
                results.push(CheckResult {
                    status: CheckStatus::Error,
                    message: format!("npm install failed: {}", e),
                    fix_hint: Some("Try running `npm install` manually".to_string()),
                });
            }
        }
    } else {
        results.push(CheckResult {
            status: CheckStatus::Warn,
            message: "node_modules/ missing - run `npm install`".to_string(),
            fix_hint: Some("Use --fix to auto-install dependencies".to_string()),
        });
    }

    // Check for lockfile
    let has_package_lock = dir.join("package-lock.json").exists();
    let has_yarn_lock = dir.join("yarn.lock").exists();
    let has_pnpm_lock = dir.join("pnpm-lock.yaml").exists();

    if has_package_lock {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "package-lock.json found".to_string(),
            fix_hint: None,
        });
    } else if has_yarn_lock {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "yarn.lock found".to_string(),
            fix_hint: None,
        });
    } else if has_pnpm_lock {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "pnpm-lock.yaml found".to_string(),
            fix_hint: None,
        });
    } else {
        results.push(CheckResult {
            status: CheckStatus::Warn,
            message: "No lockfile found (package-lock.json, yarn.lock, or pnpm-lock.yaml)".to_string(),
            fix_hint: Some("Run `npm install` to generate package-lock.json".to_string()),
        });
    }
}

fn check_cargo_project(dir: &Path, _fix: bool, results: &mut Vec<CheckResult>) {
    let cargo_toml = dir.join("Cargo.toml");

    // Check Cargo.toml exists
    if cargo_toml.exists() {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "Cargo.toml found".to_string(),
            fix_hint: None,
        });
    } else {
        results.push(CheckResult {
            status: CheckStatus::Error,
            message: "Cargo.toml not found".to_string(),
            fix_hint: Some("Run `cargo init` to create a new Cargo project".to_string()),
        });
        return;
    }

    // Check Cargo.lock exists
    let cargo_lock = dir.join("Cargo.lock");
    if cargo_lock.exists() {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: "Cargo.lock found".to_string(),
            fix_hint: None,
        });
    } else {
        results.push(CheckResult {
            status: CheckStatus::Warn,
            message: "Cargo.lock missing - dependencies not locked".to_string(),
            fix_hint: Some("Run `cargo build` to generate Cargo.lock".to_string()),
        });
    }

    // Check target/ directory (build artifacts)
    let target_dir = dir.join("target");
    if target_dir.exists() && target_dir.is_dir() {
        // Check for debug or release builds
        let has_debug = target_dir.join("debug").exists();
        let has_release = target_dir.join("release").exists();

        if has_release {
            results.push(CheckResult {
                status: CheckStatus::Ok,
                message: "target/release/ found (release build available)".to_string(),
                fix_hint: None,
            });
        } else if has_debug {
            results.push(CheckResult {
                status: CheckStatus::Ok,
                message: "target/debug/ found (debug build available)".to_string(),
                fix_hint: None,
            });
        } else {
            results.push(CheckResult {
                status: CheckStatus::Info,
                message: "target/ exists but no builds found".to_string(),
                fix_hint: Some("Run `cargo build` or `cargo build --release`".to_string()),
            });
        }
    } else {
        results.push(CheckResult {
            status: CheckStatus::Info,
            message: "target/ not found - project not built yet".to_string(),
            fix_hint: Some("Run `cargo build` to build the project".to_string()),
        });
    }
}

fn check_env_files(dir: &Path, fix: bool, results: &mut Vec<CheckResult>) {
    let env_file = dir.join(".env");
    let env_example = dir.join(".env.example");
    let env_template = dir.join(".env.template");
    let env_local = dir.join(".env.local");

    // Check if .env exists
    if env_file.exists() {
        results.push(CheckResult {
            status: CheckStatus::Ok,
            message: ".env found".to_string(),
            fix_hint: None,
        });
    } else {
        // Check for templates
        let template_path = if env_example.exists() {
            Some(env_example.clone())
        } else if env_template.exists() {
            Some(env_template.clone())
        } else {
            None
        };

        if let Some(template) = template_path {
            let template_name = template.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("template");

            if fix {
                // Copy template to .env
                match fs::copy(&template, &env_file) {
                    Ok(_) => {
                        results.push(CheckResult {
                            status: CheckStatus::Fixed,
                            message: format!("Creating .env from {}", template_name),
                            fix_hint: None,
                        });
                        results.push(CheckResult {
                            status: CheckStatus::Ok,
                            message: ".env created".to_string(),
                            fix_hint: None,
                        });
                    }
                    Err(e) => {
                        results.push(CheckResult {
                            status: CheckStatus::Error,
                            message: format!("Failed to create .env: {}", e),
                            fix_hint: Some(format!("Manually copy {} to .env", template_name)),
                        });
                    }
                }
            } else {
                results.push(CheckResult {
                    status: CheckStatus::Warn,
                    message: format!(".env missing (template found: {})", template_name),
                    fix_hint: Some("Use --fix to create .env from template".to_string()),
                });
            }
        } else {
            // No .env and no template - just info
            results.push(CheckResult {
                status: CheckStatus::Info,
                message: ".env not found (no template available)".to_string(),
                fix_hint: None,
            });
        }
    }

    // Check .env.local
    if env_local.exists() {
        results.push(CheckResult {
            status: CheckStatus::Info,
            message: ".env.local found".to_string(),
            fix_hint: None,
        });
    }
}

fn check_config_files(dir: &Path, results: &mut Vec<CheckResult>) {
    let config_files = [
        "config.json",
        "config.toml",
        "config.yaml",
        "config.yml",
        ".config.json",
        ".config.toml",
        "settings.json",
        "settings.toml",
    ];

    let found_configs: Vec<&str> = config_files
        .iter()
        .filter(|f| dir.join(f).exists())
        .cloned()
        .collect();

    if !found_configs.is_empty() {
        results.push(CheckResult {
            status: CheckStatus::Info,
            message: format!("Found config files: {}", found_configs.join(", ")),
            fix_hint: None,
        });
    }
}

fn handle_set_env(dir: &Path, envs: &[(String, String)], results: &mut Vec<CheckResult>) {
    let env_file = dir.join(".env");

    // Read existing .env content if it exists
    let existing_content = if env_file.exists() {
        fs::read_to_string(&env_file).unwrap_or_default()
    } else {
        String::new()
    };

    // Parse existing env vars to check for updates vs new additions
    let mut existing_vars: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (line_num, line) in existing_content.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Some(pos) = trimmed.find('=') {
                let key = trimmed[..pos].trim().to_string();
                existing_vars.insert(key, line_num);
            }
        }
    }

    let mut lines: Vec<String> = existing_content.lines().map(|s| s.to_string()).collect();
    let mut added_count = 0;
    let mut updated_count = 0;

    for (key, value) in envs {
        let new_line = format!("{}={}", key, value);

        if let Some(&line_num) = existing_vars.get(key) {
            // Update existing
            lines[line_num] = new_line;
            updated_count += 1;
        } else {
            // Add new
            lines.push(new_line);
            added_count += 1;
        }
    }

    // Write back
    let new_content = lines.join("\n");
    // Ensure trailing newline
    let new_content = if new_content.ends_with('\n') {
        new_content
    } else {
        format!("{}\n", new_content)
    };

    match fs::write(&env_file, new_content) {
        Ok(_) => {
            if added_count > 0 {
                results.push(CheckResult {
                    status: CheckStatus::Fixed,
                    message: format!("Added {} environment variable(s) to .env", added_count),
                    fix_hint: None,
                });
            }
            if updated_count > 0 {
                results.push(CheckResult {
                    status: CheckStatus::Fixed,
                    message: format!("Updated {} environment variable(s) in .env", updated_count),
                    fix_hint: None,
                });
            }
        }
        Err(e) => {
            results.push(CheckResult {
                status: CheckStatus::Error,
                message: format!("Failed to write .env: {}", e),
                fix_hint: None,
            });
        }
    }
}

fn run_npm_install(dir: &Path) -> Result<usize> {
    // Detect which package manager to use
    let (cmd, args) = if dir.join("pnpm-lock.yaml").exists() {
        ("pnpm", vec!["install"])
    } else if dir.join("yarn.lock").exists() {
        ("yarn", vec!["install"])
    } else {
        ("npm", vec!["install"])
    };

    let output = Command::new(cmd)
        .args(&args)
        .current_dir(dir)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }

    // Count installed packages
    let node_modules = dir.join("node_modules");
    let count = fs::read_dir(&node_modules)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0);

    Ok(count)
}

/// Result of port conflict check
#[derive(Debug)]
pub struct PortCheckResult {
    /// The port the app wants to use
    pub desired_port: u16,
    /// Whether the port is currently in use
    pub is_in_use: bool,
    /// The next available port (if desired port is in use)
    pub available_port: Option<u16>,
}

/// Detect the port a project wants to use
pub fn detect_project_port(dir: &Path) -> Option<u16> {
    // 1. Check .env file for PORT
    let env_file = dir.join(".env");
    if env_file.exists() {
        if let Ok(content) = fs::read_to_string(&env_file) {
            if let Some(port) = parse_port_from_env(&content) {
                return Some(port);
            }
        }
    }

    // 2. Check .env.local
    let env_local = dir.join(".env.local");
    if env_local.exists() {
        if let Ok(content) = fs::read_to_string(&env_local) {
            if let Some(port) = parse_port_from_env(&content) {
                return Some(port);
            }
        }
    }

    // 3. Check package.json for start script with --port or PORT
    let package_json = dir.join("package.json");
    if package_json.exists() {
        if let Ok(content) = fs::read_to_string(&package_json) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                // Check scripts.start for port
                if let Some(start_script) = json.get("scripts").and_then(|s| s.get("start")).and_then(|s| s.as_str()) {
                    if let Some(port) = parse_port_from_script(start_script) {
                        return Some(port);
                    }
                }
                // Check scripts.dev for port
                if let Some(dev_script) = json.get("scripts").and_then(|s| s.get("dev")).and_then(|s| s.as_str()) {
                    if let Some(port) = parse_port_from_script(dev_script) {
                        return Some(port);
                    }
                }
            }
        }
        // Default port for Node.js projects
        return Some(3000);
    }

    // 4. Check Cargo.toml for Rocket, Actix, Axum (common Rust web frameworks)
    let cargo_toml = dir.join("Cargo.toml");
    if cargo_toml.exists() {
        // Rust web apps commonly use 8080
        // Check if it's a web project by looking for common web framework deps
        if let Ok(content) = fs::read_to_string(&cargo_toml) {
            if content.contains("actix-web") || content.contains("axum") ||
               content.contains("rocket") || content.contains("warp") {
                return Some(8080);
            }
        }
    }

    None
}

/// Parse PORT from env file content
fn parse_port_from_env(content: &str) -> Option<u16> {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("PORT=") {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if let Ok(port) = value.parse::<u16>() {
                return Some(port);
            }
        }
    }
    None
}

/// Parse port from npm script (e.g., "PORT=3001 react-scripts start" or "--port 3001")
fn parse_port_from_script(script: &str) -> Option<u16> {
    // Check for PORT=XXXX
    if let Some(idx) = script.find("PORT=") {
        let rest = &script[idx + 5..];
        let port_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(port) = port_str.parse::<u16>() {
            return Some(port);
        }
    }
    // Check for --port XXXX or -p XXXX
    let words: Vec<&str> = script.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        if (*word == "--port" || *word == "-p") && i + 1 < words.len() {
            if let Ok(port) = words[i + 1].parse::<u16>() {
                return Some(port);
            }
        }
    }
    None
}

/// Check if a port is currently in use
pub fn is_port_in_use(port: u16) -> bool {
    // Check both IPv4 and IPv6 to catch all cases
    // Try binding to 0.0.0.0 (all IPv4 interfaces) first
    if TcpListener::bind(("0.0.0.0", port)).is_err() {
        return true;
    }
    // Also check IPv6 on all interfaces
    if TcpListener::bind(("::", port)).is_err() {
        return true;
    }
    false
}

/// Find the next available port starting from the given port
pub fn find_available_port(start_port: u16) -> Option<u16> {
    for port in start_port..=65535 {
        if !is_port_in_use(port) {
            return Some(port);
        }
    }
    None
}

/// Check for port conflicts and return information
pub fn check_port_conflict(dir: &Path) -> Option<PortCheckResult> {
    let desired_port = detect_project_port(dir)?;
    let is_in_use = is_port_in_use(desired_port);

    let available_port = if is_in_use {
        find_available_port(desired_port + 1)
    } else {
        None
    };

    Some(PortCheckResult {
        desired_port,
        is_in_use,
        available_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_detect_project_type_nodejs() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("package.json")).unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::NodeJs);
    }

    #[test]
    fn test_detect_project_type_cargo() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("Cargo.toml")).unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::Cargo);
    }

    #[test]
    fn test_detect_project_type_generic() {
        let dir = TempDir::new().unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::Generic);
    }

    #[test]
    fn test_check_env_files_with_template() {
        let dir = TempDir::new().unwrap();
        let mut template = File::create(dir.path().join(".env.example")).unwrap();
        writeln!(template, "DATABASE_URL=postgres://localhost/mydb").unwrap();

        let mut results = Vec::new();
        check_env_files(dir.path(), false, &mut results);

        // Should have a warning about missing .env
        assert!(results.iter().any(|r| r.status == CheckStatus::Warn));
        assert!(results.iter().any(|r| r.message.contains(".env missing")));
    }

    #[test]
    fn test_check_env_files_with_fix() {
        let dir = TempDir::new().unwrap();
        let mut template = File::create(dir.path().join(".env.example")).unwrap();
        writeln!(template, "DATABASE_URL=postgres://localhost/mydb").unwrap();

        let mut results = Vec::new();
        check_env_files(dir.path(), true, &mut results);

        // Should have created .env
        assert!(dir.path().join(".env").exists());
        assert!(results.iter().any(|r| r.status == CheckStatus::Fixed));
    }

    #[test]
    fn test_handle_set_env_new_file() {
        let dir = TempDir::new().unwrap();
        let mut results = Vec::new();

        handle_set_env(dir.path(), &[
            ("DATABASE_URL".to_string(), "postgres://localhost/mydb".to_string()),
            ("PORT".to_string(), "3000".to_string()),
        ], &mut results);

        assert!(dir.path().join(".env").exists());
        let content = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(content.contains("DATABASE_URL=postgres://localhost/mydb"));
        assert!(content.contains("PORT=3000"));
    }

    #[test]
    fn test_handle_set_env_update_existing() {
        let dir = TempDir::new().unwrap();
        let mut env_file = File::create(dir.path().join(".env")).unwrap();
        writeln!(env_file, "DATABASE_URL=old_value").unwrap();
        writeln!(env_file, "OTHER_VAR=keep_me").unwrap();
        drop(env_file);

        let mut results = Vec::new();
        handle_set_env(dir.path(), &[
            ("DATABASE_URL".to_string(), "new_value".to_string()),
        ], &mut results);

        let content = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(content.contains("DATABASE_URL=new_value"));
        assert!(content.contains("OTHER_VAR=keep_me"));
        assert!(!content.contains("old_value"));
    }

    #[test]
    fn test_check_nodejs_project_missing_node_modules() {
        let dir = TempDir::new().unwrap();
        let mut package = File::create(dir.path().join("package.json")).unwrap();
        writeln!(package, r#"{{"name": "test"}}"#).unwrap();

        let mut results = Vec::new();
        check_nodejs_project(dir.path(), false, &mut results);

        assert!(results.iter().any(|r| r.message.contains("package.json found")));
        assert!(results.iter().any(|r| r.message.contains("node_modules/ missing")));
    }

    #[test]
    fn test_check_cargo_project() {
        let dir = TempDir::new().unwrap();
        let mut cargo_toml = File::create(dir.path().join("Cargo.toml")).unwrap();
        writeln!(cargo_toml, r#"[package]"#).unwrap();
        writeln!(cargo_toml, r#"name = "test""#).unwrap();

        let mut results = Vec::new();
        check_cargo_project(dir.path(), false, &mut results);

        assert!(results.iter().any(|r| r.message.contains("Cargo.toml found")));
        assert!(results.iter().any(|r| r.message.contains("Cargo.lock missing")));
    }

    #[test]
    fn test_check_config_files() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("config.json")).unwrap();
        File::create(dir.path().join("config.toml")).unwrap();

        let mut results = Vec::new();
        check_config_files(dir.path(), &mut results);

        assert!(results.iter().any(|r| r.message.contains("config.json")));
        assert!(results.iter().any(|r| r.message.contains("config.toml")));
    }

    #[test]
    fn test_check_status_prefix() {
        assert!(CheckStatus::Ok.prefix().contains("OK"));
        assert!(CheckStatus::Warn.prefix().contains("WARN"));
        assert!(CheckStatus::Error.prefix().contains("ERROR"));
        assert!(CheckStatus::Info.prefix().contains("INFO"));
        assert!(CheckStatus::Fixed.prefix().contains("FIX"));
    }
}
