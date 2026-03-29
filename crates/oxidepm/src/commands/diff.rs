//! Diff command — show what changed on a remote server since last deploy

use anyhow::{bail, Result};
use colored::Colorize;
use std::process::Command;

use crate::cli::DiffArgs;

pub async fn execute(args: DiffArgs) -> Result<()> {
    let host = &args.host;

    let cwd_cmd = if let Some(ref cwd) = args.cwd {
        format!("cd {} && ", cwd)
    } else {
        String::new()
    };

    println!("{} {}", "Remote diff on".cyan().bold(), host.bold());
    println!();

    // Show recent commits
    let log_cmd = format!(
        "{}git log --oneline --no-decorate -{}",
        cwd_cmd, args.count
    );
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, &log_cmd])
        .output()?;

    if !output.status.success() {
        bail!(
            "Failed to get git log: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log_output = String::from_utf8_lossy(&output.stdout);
    println!("{}", "Recent commits:".dimmed());
    for line in log_output.lines() {
        println!("  {}", line);
    }
    println!();

    // Show uncommitted changes (working tree)
    let status_cmd = format!("{}git status --short", cwd_cmd);
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, &status_cmd])
        .output()?;

    let status_output = String::from_utf8_lossy(&output.stdout);
    if status_output.trim().is_empty() {
        println!("{}", "Working tree clean".green());
    } else {
        println!("{}", "Uncommitted changes:".yellow());
        for line in status_output.lines() {
            println!("  {}", line);
        }
    }

    // Show diff stat vs remote
    let diff_cmd = format!("{}git diff --stat HEAD", cwd_cmd);
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, &diff_cmd])
        .output()?;

    let diff_output = String::from_utf8_lossy(&output.stdout);
    if !diff_output.trim().is_empty() {
        println!();
        println!("{}", "Diff stat:".dimmed());
        for line in diff_output.lines() {
            println!("  {}", line);
        }
    }

    Ok(())
}
