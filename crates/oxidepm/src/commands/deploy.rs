//! Deploy command — SSH to remote, pull code, restart processes

use anyhow::{bail, Result};
use colored::Colorize;
use std::process::Command;

use crate::cli::DeployArgs;

/// Shell-quote a string to prevent injection when passed to SSH commands.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub async fn execute(args: DeployArgs) -> Result<()> {
    let host = &args.host;

    println!("{} {}", "Deploying to".cyan().bold(), host.bold());

    // Build SSH command components
    let cwd_cmd = if let Some(ref cwd) = args.cwd {
        format!("cd {} && ", shell_quote(cwd))
    } else {
        String::new()
    };

    // Step 1: Git pull
    let branch = args.branch.as_deref().unwrap_or("prod");
    let pull_cmd = format!("{}git pull origin {}", cwd_cmd, shell_quote(branch));

    if args.dry_run {
        println!("  {} ssh {} '{}'", "[DRY-RUN]".yellow(), host, pull_cmd);
    } else {
        println!("  {} git pull origin {}", "→".dimmed(), branch);
        let output = ssh_exec(host, &pull_cmd)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Git pull failed: {}", stderr.trim());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().take(5) {
            println!("    {}", line.dimmed());
        }
    }

    // Step 2: Post-deploy command (e.g., build)
    if let Some(ref post_cmd) = args.post {
        let full_cmd = format!("{}{}", cwd_cmd, post_cmd);

        if args.dry_run {
            println!("  {} ssh {} '{}'", "[DRY-RUN]".yellow(), host, full_cmd);
        } else {
            println!("  {} {}", "→".dimmed(), post_cmd);
            let output = ssh_exec(host, &full_cmd)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Post-deploy command failed: {}", stderr.trim());
            }
        }
    }

    // Step 3: Restart processes
    let restart_target = args.restart.as_deref().unwrap_or("all");
    let restart_cmd = format!("oxidepm restart {}", shell_quote(restart_target));

    if args.dry_run {
        println!("  {} ssh {} '{}'", "[DRY-RUN]".yellow(), host, restart_cmd);
    } else {
        println!("  {} oxidepm restart {}", "→".dimmed(), restart_target);
        let output = ssh_exec(host, &restart_cmd)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("  {} restart failed: {}", "!".yellow(), stderr.trim());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                println!("    {}", line);
            }
        }
    }

    println!("\n{} Deploy complete", "✓".green().bold());

    Ok(())
}

fn ssh_exec(host: &str, cmd: &str) -> Result<std::process::Output> {
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, cmd])
        .output()?;
    Ok(output)
}
