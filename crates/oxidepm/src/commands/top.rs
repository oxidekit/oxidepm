//! Top command — real-time process resource monitor

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_core::AppStatus;
use oxidepm_ipc::{Request, Response};
use std::io::Write;

use crate::output::{is_json_mode, truncate_chars};

pub async fn execute() -> Result<()> {
    // top redraws a live screen; there is no meaningful single JSON document.
    if is_json_mode() {
        bail!("`top` is an interactive view and does not support --json (use `status --json`)");
    }

    let client = super::get_client();

    println!("{}", "oxidepm top — press Ctrl+C to exit".dimmed());
    println!();

    loop {
        let response = super::send(&client, &Request::Status).await?;

        match response {
            Response::Status { mut apps } => {
                // Sort by CPU descending
                apps.sort_by(|a, b| {
                    b.state
                        .cpu_percent
                        .partial_cmp(&a.state.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                // Clear screen and move cursor to top
                print!("\x1B[2J\x1B[H");

                // Header
                let running = apps.iter().filter(|a| a.state.status.has_child()).count();
                let total = apps.len();
                println!(
                    "{} — {} processes ({} running)",
                    "oxidepm top".cyan().bold(),
                    total,
                    running
                );
                println!();

                // Table header
                println!(
                    "  {:>3} {:<16} {:<8} {:>7} {:>8} {:>8} {:>10}",
                    "ID".dimmed(),
                    "NAME".dimmed(),
                    "STATUS".dimmed(),
                    "CPU".dimmed(),
                    "MEM".dimmed(),
                    "PID".dimmed(),
                    "UPTIME".dimmed(),
                );
                println!("{}", "─".repeat(70).dimmed());

                for app in &apps {
                    let status_str = match app.state.status {
                        AppStatus::Running => "online".green().to_string(),
                        AppStatus::Stopped => "stopped".red().to_string(),
                        AppStatus::Errored => "errored".red().bold().to_string(),
                        AppStatus::Stopping => "stopping".yellow().to_string(),
                        _ => format!("{:?}", app.state.status).dimmed().to_string(),
                    };

                    let cpu = format!("{:.1}%", app.state.cpu_percent);
                    let mem = format_bytes(app.state.memory_bytes);
                    let pid = app
                        .state
                        .pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let uptime = format_uptime(app.state.uptime_secs);

                    println!(
                        "  {:>3} {:<16} {:<8} {:>7} {:>8} {:>8} {:>10}",
                        app.spec.id,
                        truncate_chars(&app.spec.name, 16),
                        status_str,
                        if app.state.status.has_child() {
                            cpu.to_string()
                        } else {
                            "-".dimmed().to_string()
                        },
                        if app.state.status.has_child() {
                            mem
                        } else {
                            "-".dimmed().to_string()
                        },
                        pid,
                        uptime,
                    );
                }

                println!("\n{}", "Refreshing every 2s — Ctrl+C to exit".dimmed());

                std::io::stdout().flush()?;
            }
            Response::Error { message, .. } => bail!(message),
            _ => bail!("Unexpected response from daemon"),
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0B".to_string();
    }
    let units = ["B", "K", "M", "G"];
    let mut val = bytes as f64;
    let mut unit_idx = 0;
    while val >= 1024.0 && unit_idx < units.len() - 1 {
        val /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{}B", bytes)
    } else {
        format!("{:.1}{}", val, units[unit_idx])
    }
}

fn format_uptime(secs: u64) -> String {
    if secs == 0 {
        return "-".dimmed().to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}
