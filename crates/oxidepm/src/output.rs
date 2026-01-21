//! PM2-style output formatting

use colored::Colorize;
use oxidepm_core::{AppInfo, AppStatus};
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
    pub watch: bool,
    pub last_exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub max_memory_mb: Option<u64>,
    pub max_uptime_secs: Option<u64>,
    pub healthy: bool,
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
            watch: info.spec.watch,
            last_exit_code: info.state.last_exit_code,
            tags: info.spec.tags.clone(),
            max_memory_mb: info.spec.max_memory_mb,
            max_uptime_secs: info.spec.max_uptime_secs,
            healthy: info.state.healthy,
        }
    }
}

pub fn print_app_detail(info: &AppInfo) {
    if is_json_mode() {
        let json_detail = AppDetailJson::from(info);
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
    println!("  {} │ {}", "Status".bold(), format_status(info.state.status));
    println!(
        "  {} │ {}",
        "PID".bold(),
        info.state
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("  {} │ {}", "Restarts".bold(), info.state.restarts);
    println!("  {} │ {}", "Uptime".bold(), format_duration(info.state.uptime_secs));
    println!(
        "  {} │ {:.1}%",
        "CPU".bold(),
        info.state.cpu_percent
    );
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
    if info.spec.watch {
        println!("  {} │ enabled", "Watch".bold());
    }
    if !info.spec.tags.is_empty() {
        println!("  {} │ {:?}", "Tags".bold(), info.spec.tags);
    }
    if let Some(max_mem) = info.spec.max_memory_mb {
        println!("  {} │ {}MB", "Max Memory".bold(), max_mem);
    }
    if let Some(max_uptime) = info.spec.max_uptime_secs {
        println!("  {} │ {}", "Max Uptime".bold(), format_duration(max_uptime));
    }
    if let Some(code) = info.state.last_exit_code {
        println!("  {} │ {}", "Last Exit".bold(), code);
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

/// JSON wrapper for generic responses
#[derive(Serialize)]
pub struct ResponseJson<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

/// Print a success message in JSON format if enabled
#[allow(dead_code)]
pub fn print_success_json<T: Serialize>(message: &str, data: Option<T>) {
    if is_json_mode() {
        let response = ResponseJson {
            success: true,
            message: Some(message.to_string()),
            data,
        };
        if let Ok(json) = serde_json::to_string_pretty(&response) {
            println!("{}", json);
        }
    } else {
        print_success(message);
    }
}

/// Print an error message in JSON format if enabled
#[allow(dead_code)]
pub fn print_error_json(message: &str) {
    if is_json_mode() {
        let response: ResponseJson<()> = ResponseJson {
            success: false,
            message: Some(message.to_string()),
            data: None,
        };
        if let Ok(json) = serde_json::to_string_pretty(&response) {
            eprintln!("{}", json);
        }
    } else {
        print_error(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::{AppMode, AppSpec, RunState};
    use std::path::PathBuf;

    fn create_test_app_info() -> AppInfo {
        let mut spec = AppSpec::new(
            "test-app".to_string(),
            AppMode::Node,
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
