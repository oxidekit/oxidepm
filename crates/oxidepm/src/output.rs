//! PM2-style output formatting

use colored::Colorize;
use oxidepm_core::{AppInfo, AppStatus};
use oxidepm_ipc::{DryRunPlan, PolicyDecision};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tabled::{
    settings::{object::Columns, Alignment, Modify, Style},
    Table, Tabled,
};

/// Global flag for JSON output mode
static JSON_MODE: AtomicBool = AtomicBool::new(false);

/// Enable or disable JSON output mode
pub fn set_json_mode(enabled: bool) {
    JSON_MODE.store(enabled, Ordering::SeqCst);
}

/// `text` with every control character (C0 and C1, ESC and DEL included) replaced by U+FFFD. Any
/// string that did not originate in the CLI (an actor's label, a reason, a note) goes through
/// this before it reaches a terminal: cursor-movement or erase-line sequences in it could
/// otherwise overwrite the lines around it.
pub fn sanitize_terminal(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Check if JSON output mode is enabled
pub fn is_json_mode() -> bool {
    JSON_MODE.load(Ordering::SeqCst)
}

#[derive(Tabled, Serialize)]
pub struct StatusRow {
    #[tabled(rename = "id")]
    pub id: u32,
    #[tabled(rename = "name")]
    pub name: String,
    #[tabled(rename = "mode")]
    pub mode: String,
    #[tabled(rename = "pid")]
    pub pid: String,
    #[tabled(rename = "↺")]
    #[serde(rename = "restarts")]
    pub restarts: String,
    #[tabled(rename = "status")]
    pub status: String,
    #[tabled(rename = "cpu")]
    pub cpu: String,
    #[tabled(rename = "mem")]
    pub mem: String,
    #[tabled(rename = "uptime")]
    pub uptime: String,
}

/// JSON-friendly status representation
#[derive(Serialize)]
pub struct StatusJson {
    pub id: u32,
    pub name: String,
    pub mode: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub status: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub uptime_secs: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<&AppInfo> for StatusJson {
    fn from(info: &AppInfo) -> Self {
        StatusJson {
            id: info.spec.id,
            name: info.spec.name.clone(),
            mode: info.spec.mode.to_string(),
            pid: info.state.pid,
            restarts: info.state.restarts,
            status: info.state.status.as_str().to_string(),
            cpu_percent: info.state.cpu_percent,
            memory_bytes: info.state.memory_bytes,
            uptime_secs: info.state.uptime_secs,
            tags: info.spec.tags.clone(),
        }
    }
}

impl From<&AppInfo> for StatusRow {
    fn from(info: &AppInfo) -> Self {
        let status_colored = match info.state.status {
            AppStatus::Running => "online".green().to_string(),
            AppStatus::Stopped => "stopped".red().to_string(),
            AppStatus::Errored => "errored".red().bold().to_string(),
            AppStatus::Starting => "starting".yellow().to_string(),
            AppStatus::Stopping => "stopping".yellow().to_string(),
            AppStatus::Building => "building".cyan().to_string(),
            AppStatus::Waiting => "waiting".cyan().to_string(),
            AppStatus::Backoff => "backoff".yellow().to_string(),
            AppStatus::Unhealthy => "unhealthy".red().to_string(),
        };

        StatusRow {
            id: info.spec.id,
            name: info.spec.name.clone(),
            mode: info.spec.mode.to_string(),
            pid: info
                .state
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            restarts: info.state.restarts.to_string(),
            status: status_colored,
            cpu: format!("{:.1}%", info.state.cpu_percent),
            mem: format_bytes(info.state.memory_bytes),
            uptime: format_duration(info.state.uptime_secs),
        }
    }
}

pub fn print_status_table(apps: &[AppInfo]) {
    if is_json_mode() {
        let json_apps: Vec<StatusJson> = apps.iter().map(StatusJson::from).collect();
        match serde_json::to_string_pretty(&json_apps) {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Error serializing to JSON: {}", e),
        }
        return;
    }

    if apps.is_empty() {
        println!("No processes running");
        return;
    }

    let rows: Vec<StatusRow> = apps.iter().map(StatusRow::from).collect();

    let table = Table::new(rows)
        .with(Style::rounded())
        .with(Modify::new(Columns::single(0)).with(Alignment::right()))
        .to_string();

    println!("{}", table);
}

/// Extended status row with cwd and port
#[derive(Tabled, Serialize)]
pub struct StatusRowExtended {
    #[tabled(rename = "id")]
    pub id: u32,
    #[tabled(rename = "name")]
    pub name: String,
    #[tabled(rename = "mode")]
    pub mode: String,
    #[tabled(rename = "pid")]
    pub pid: String,
    #[tabled(rename = "↺")]
    #[serde(rename = "restarts")]
    pub restarts: String,
    #[tabled(rename = "status")]
    pub status: String,
    #[tabled(rename = "port")]
    pub port: String,
    #[tabled(rename = "cwd")]
    pub cwd: String,
    #[tabled(rename = "uptime")]
    pub uptime: String,
    #[tabled(rename = "policy")]
    pub policy: String,
}

/// The markers of an app as one word for a table: `-`, `protected`, `ai-control` or both.
pub fn policy_text(protected: bool, ai_control: bool) -> String {
    match (protected, ai_control) {
        (false, false) => "-".to_string(),
        (true, false) => "protected".to_string(),
        (false, true) => "ai-control".to_string(),
        (true, true) => "protected ai-control".to_string(),
    }
}

impl From<&AppInfo> for StatusRowExtended {
    fn from(info: &AppInfo) -> Self {
        let status_colored = match info.state.status {
            AppStatus::Running => "online".green().to_string(),
            AppStatus::Stopped => "stopped".red().to_string(),
            AppStatus::Errored => "errored".red().bold().to_string(),
            AppStatus::Starting => "starting".yellow().to_string(),
            AppStatus::Stopping => "stopping".yellow().to_string(),
            AppStatus::Building => "building".cyan().to_string(),
            AppStatus::Waiting => "waiting".cyan().to_string(),
            AppStatus::Backoff => "backoff".yellow().to_string(),
            AppStatus::Unhealthy => "unhealthy".red().to_string(),
        };

        // Get port from state first (actual running port), fall back to spec
        let port = info
            .state
            .port
            .or(info.spec.port)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());

        // Shorten cwd for display (show last 2 path components)
        let cwd = shorten_path(&info.spec.cwd.display().to_string(), 30);

        StatusRowExtended {
            id: info.spec.id,
            name: info.spec.name.clone(),
            mode: info.spec.mode.to_string(),
            pid: info
                .state
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            restarts: info.state.restarts.to_string(),
            status: status_colored,
            port,
            cwd,
            uptime: format_duration(info.state.uptime_secs),
            policy: policy_text(info.spec.protected, info.spec.ai_control),
        }
    }
}

/// Truncate to at most `max` characters, ending in "…" when shortened.
/// Works on characters, so multi-byte names never hit a byte boundary.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let kept: String = s.chars().take(max - 1).collect();
    format!("{}…", kept)
}

/// Shorten a path for display by keeping last components
fn shorten_path(path: &str, max_len: usize) -> String {
    let len = path.chars().count();
    if len <= max_len {
        return path.to_string();
    }

    // Try to show just the last 2 components
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 {
        let shortened = format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
        if shortened.chars().count() <= max_len {
            return shortened;
        }
    }

    // Just keep the tail with an ellipsis
    let tail: String = path.chars().skip(len - max_len.saturating_sub(1)).collect();
    format!("…{}", tail)
}

pub fn print_status_table_extended(apps: &[AppInfo]) {
    if is_json_mode() {
        // Include cwd and port in JSON output
        let json_apps: Vec<AppDetailJson> = apps.iter().map(AppDetailJson::from).collect();
        match serde_json::to_string_pretty(&json_apps) {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Error serializing to JSON: {}", e),
        }
        return;
    }

    if apps.is_empty() {
        println!("No processes running");
        return;
    }

    let rows: Vec<StatusRowExtended> = apps.iter().map(StatusRowExtended::from).collect();

    let table = Table::new(rows)
        .with(Style::rounded())
        .with(Modify::new(Columns::single(0)).with(Alignment::right()))
        .to_string();

    println!("{}", table);
}

/// JSON representation of detailed app info
#[derive(Serialize)]
pub struct AppDetailJson {
    pub id: u32,
    pub name: String,
    pub mode: String,
    pub status: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub uptime_secs: u64,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub command: String,
    pub cwd: String,
    pub args: Vec<String>,
    pub last_exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub max_memory_mb: Option<u64>,
    pub max_uptime_secs: Option<u64>,
    pub healthy: bool,
    pub protected: bool,
    pub ai_control: bool,
    /// Environment and hook scripts: present only when the user asked to reveal them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<std::collections::BTreeMap<String, String>>,
}

/// The configured hook scripts as (name, script) pairs.
fn hook_pairs(info: &AppInfo) -> Vec<(&'static str, &String)> {
    let h = &info.spec.hooks;
    [
        ("on_start", &h.on_start),
        ("on_stop", &h.on_stop),
        ("on_restart", &h.on_restart),
        ("on_crash", &h.on_crash),
        ("on_error", &h.on_error),
    ]
    .into_iter()
    .filter_map(|(name, script)| script.as_ref().map(|s| (name, s)))
    .collect()
}

impl From<&AppInfo> for AppDetailJson {
    fn from(info: &AppInfo) -> Self {
        AppDetailJson {
            id: info.spec.id,
            name: info.spec.name.clone(),
            mode: info.spec.mode.to_string(),
            status: info.state.status.as_str().to_string(),
            pid: info.state.pid,
            restarts: info.state.restarts,
            uptime_secs: info.state.uptime_secs,
            cpu_percent: info.state.cpu_percent,
            memory_bytes: info.state.memory_bytes,
            command: info.spec.command.clone(),
            cwd: info.spec.cwd.display().to_string(),
            args: info.spec.args.clone(),
            last_exit_code: info.state.last_exit_code,
            tags: info.spec.tags.clone(),
            max_memory_mb: info.spec.max_memory_mb,
            max_uptime_secs: info.spec.max_uptime_secs,
            healthy: info.state.healthy,
            protected: info.spec.protected,
            ai_control: info.spec.ai_control,
            env: None,
            hooks: None,
        }
    }
}

/// Print one app. `reveal` also prints the environment and hook scripts (the daemon only sends
/// real values when asked, so without it these are markers and are left out).
pub fn print_app_detail(info: &AppInfo, reveal: bool) {
    if is_json_mode() {
        let mut json_detail = AppDetailJson::from(info);
        if reveal {
            json_detail.env = Some(info.spec.env.clone().into_iter().collect());
            json_detail.hooks = Some(
                hook_pairs(info)
                    .into_iter()
                    .map(|(name, script)| (name.to_string(), script.clone()))
                    .collect(),
            );
        }
        match serde_json::to_string_pretty(&json_detail) {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Error serializing to JSON: {}", e),
        }
        return;
    }

    println!("{}", "─".repeat(50));
    println!("  {} │ {}", "Name".bold(), info.spec.name);
    println!("  {} │ {}", "ID".bold(), info.spec.id);
    println!("  {} │ {}", "Mode".bold(), info.spec.mode);
    println!(
        "  {} │ {}",
        "Status".bold(),
        format_status(info.state.status)
    );
    println!(
        "  {} │ {}",
        "PID".bold(),
        info.state
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("  {} │ {}", "Restarts".bold(), info.state.restarts);
    println!(
        "  {} │ {}",
        "Uptime".bold(),
        format_duration(info.state.uptime_secs)
    );
    println!("  {} │ {:.1}%", "CPU".bold(), info.state.cpu_percent);
    println!(
        "  {} │ {}",
        "Memory".bold(),
        format_bytes(info.state.memory_bytes)
    );
    println!("{}", "─".repeat(50));
    println!("  {} │ {}", "Command".bold(), info.spec.command);
    println!("  {} │ {}", "CWD".bold(), info.spec.cwd.display());
    if !info.spec.args.is_empty() {
        println!("  {} │ {:?}", "Args".bold(), info.spec.args);
    }
    if !info.spec.tags.is_empty() {
        println!("  {} │ {:?}", "Tags".bold(), info.spec.tags);
    }
    if let Some(max_mem) = info.spec.max_memory_mb {
        println!("  {} │ {}MB", "Max Memory".bold(), max_mem);
    }
    if let Some(max_uptime) = info.spec.max_uptime_secs {
        println!(
            "  {} │ {}",
            "Max Uptime".bold(),
            format_duration(max_uptime)
        );
    }
    if let Some(code) = info.state.last_exit_code {
        println!("  {} │ {}", "Last Exit".bold(), code);
    }
    if info.spec.protected || info.spec.ai_control {
        println!(
            "  {} │ {}",
            "Policy".bold(),
            policy_text(info.spec.protected, info.spec.ai_control)
        );
    }
    if reveal {
        let mut env: Vec<_> = info.spec.env.iter().collect();
        env.sort();
        for (key, value) in env {
            println!("  {} │ {}={}", "Env".bold(), key, value);
        }
        for (name, script) in hook_pairs(info) {
            println!("  {} │ {}", format!("Hook {}", name).bold(), script);
        }
    }
    println!("{}", "─".repeat(50));
}

fn format_status(status: AppStatus) -> String {
    match status {
        AppStatus::Running => "online".green().to_string(),
        AppStatus::Stopped => "stopped".red().to_string(),
        AppStatus::Errored => "errored".red().bold().to_string(),
        AppStatus::Starting => "starting".yellow().to_string(),
        AppStatus::Stopping => "stopping".yellow().to_string(),
        AppStatus::Building => "building".cyan().to_string(),
        AppStatus::Waiting => "waiting".cyan().to_string(),
        AppStatus::Backoff => "backoff".yellow().to_string(),
        AppStatus::Unhealthy => "unhealthy".red().to_string(),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else if bytes > 0 {
        format!("{}B", bytes)
    } else {
        "0B".to_string()
    }
}

pub fn format_duration(secs: u64) -> String {
    if secs >= 86400 {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        format!("{}d {}h", days, hours)
    } else if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else if secs >= 60 {
        let mins = secs / 60;
        let s = secs % 60;
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", secs)
    }
}

pub fn print_success(message: &str) {
    println!("{} {}", "✓".green(), message);
}

pub fn print_error(message: &str) {
    eprintln!("{} {}", "✗".red(), message);
}

pub fn print_info(message: &str) {
    println!("{} {}", "ℹ".blue(), message);
}

/// Print logs in JSON format if enabled
pub fn print_logs(lines: &[String]) {
    if is_json_mode() {
        match serde_json::to_string_pretty(&lines) {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Error serializing to JSON: {}", e),
        }
        return;
    }

    for line in lines {
        println!("{}", line);
    }
}

/// Show the plan of a rehearsal. With `--json` the output is the plan itself. A plan the policy
/// refuses is shown and returned as an error (exit code 1), like the real request would be.
pub fn print_plan(plan: &DryRunPlan) -> anyhow::Result<()> {
    if is_json_mode() {
        println!("{}", serde_json::to_string_pretty(plan)?);
    } else {
        match &plan.decision {
            PolicyDecision::Allow => println!(
                "{} Dry run of '{}': this is what would happen. Nothing was changed.",
                "ℹ".blue(),
                sanitize_terminal(&plan.op)
            ),
            PolicyDecision::Deny { code, message } => println!(
                "{} Dry run of '{}': refused ({}): {}",
                "✗".red(),
                sanitize_terminal(&plan.op),
                sanitize_terminal(code.as_str()),
                sanitize_terminal(message)
            ),
        }
        if plan.items.is_empty() && matches!(plan.decision, PolicyDecision::Allow) {
            println!("  (nothing to do)");
        }
        let width = plan
            .items
            .iter()
            .map(|i| i.action.chars().count())
            .max()
            .unwrap_or(0);
        for item in &plan.items {
            let order = item
                .order
                .map(|o| format!(" [step {o}]"))
                .unwrap_or_default();
            let detail = item
                .detail
                .as_deref()
                .map(|d| format!("  {}", sanitize_terminal(d)))
                .unwrap_or_default();
            println!(
                "  {:<width$}  {}{}{}",
                sanitize_terminal(&item.action),
                sanitize_terminal(&item.name),
                order,
                detail,
                width = width
            );
        }
        for warning in &plan.warnings {
            println!("  {} {}", "!".yellow(), sanitize_terminal(warning));
        }
    }
    match &plan.decision {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny { message, .. } => anyhow::bail!("{}", message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::{AppSpec, RunState, Runtime};
    use std::path::PathBuf;

    #[test]
    fn truncate_chars_handles_multibyte_names() {
        let name = "é".repeat(19);
        let t = truncate_chars(&name, 16);
        assert_eq!(t.chars().count(), 16);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("short", 16), "short");
        assert_eq!(truncate_chars("exactly-sixteen!", 16), "exactly-sixteen!");
        assert_eq!(truncate_chars("日本語のプロセス名前", 4), "日本語…");
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn shorten_path_handles_multibyte_paths() {
        let long = format!("/{}", "é".repeat(40));
        let s = shorten_path(&long, 30);
        assert!(s.chars().count() <= 30);
        assert!(s.starts_with('…'));

        let nested = "/home/用户/项目/一个很长的目录名称/另一个很长的目录名称/应用";
        let s = shorten_path(nested, 20);
        assert!(s.chars().count() <= 20, "{s}");
    }

    #[test]
    fn shorten_path_keeps_ascii_behaviour() {
        assert_eq!(shorten_path("/a/b", 30), "/a/b");
        assert_eq!(
            shorten_path("/very/long/path/to/some/app", 14),
            "…/some/app"
        );
    }

    fn create_test_app_info() -> AppInfo {
        let mut spec = AppSpec::new(
            "test-app".to_string(),
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        )
        .with_tags(vec!["web".to_string(), "production".to_string()])
        .with_max_memory(512)
        .with_max_uptime(86400);

        // Simulate the ID being assigned
        spec.id = 1;

        let mut state = RunState::running(1, 1234);
        state.uptime_secs = 3600;
        state.cpu_percent = 15.5;
        state.memory_bytes = 128 * 1024 * 1024;

        AppInfo::new(spec, state)
    }

    #[test]
    fn test_json_mode_toggle() {
        // Initially off
        set_json_mode(false);
        assert!(!is_json_mode());

        // Turn on
        set_json_mode(true);
        assert!(is_json_mode());

        // Turn off again
        set_json_mode(false);
        assert!(!is_json_mode());
    }

    #[test]
    fn test_status_json_from_app_info() {
        let info = create_test_app_info();
        let json_status = StatusJson::from(&info);

        assert_eq!(json_status.id, 1);
        assert_eq!(json_status.name, "test-app");
        assert_eq!(json_status.mode, "node");
        assert_eq!(json_status.pid, Some(1234));
        assert_eq!(json_status.uptime_secs, 3600);
        assert_eq!(json_status.tags, vec!["web", "production"]);
    }

    #[test]
    fn test_app_detail_json_from_app_info() {
        let info = create_test_app_info();
        let json_detail = AppDetailJson::from(&info);

        assert_eq!(json_detail.id, 1);
        assert_eq!(json_detail.name, "test-app");
        assert_eq!(json_detail.mode, "node");
        assert_eq!(json_detail.command, "server.js");
        assert_eq!(json_detail.cwd, "/app");
        assert_eq!(json_detail.tags, vec!["web", "production"]);
        assert_eq!(json_detail.max_memory_mb, Some(512));
        assert_eq!(json_detail.max_uptime_secs, Some(86400));
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(3599), "59m 59s");
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(86400), "1d 0h");
        assert_eq!(format_duration(90061), "1d 1h");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(1024), "1K");
        assert_eq!(format_bytes(1024 * 1024), "1.0M");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_bytes(128 * 1024 * 1024), "128.0M");
    }
}
