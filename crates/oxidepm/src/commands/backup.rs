//! Backup and restore commands

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_core::constants;
use std::fs;
use std::process::Command;

pub async fn execute_backup(output: Option<String>) -> Result<()> {
    let home = constants::oxidepm_home();
    if !home.exists() {
        bail!("OxidePM home not found: {}", home.display());
    }

    let output_file = output.unwrap_or_else(|| "oxidepm-backup.tar.gz".to_string());

    println!("{}", "Creating OxidePM backup...".cyan().bold());

    // Collect files to backup
    let mut files = Vec::new();

    let saved = home.join("saved.json");
    if saved.exists() {
        files.push("saved.json");
        println!("  {} saved.json (process list)", "✓".green());
    }

    let notify = home.join("notify.toml");
    if notify.exists() {
        files.push("notify.toml");
        println!("  {} notify.toml (notification config)", "✓".green());
    }

    let db = home.join("oxidepm.db");
    if db.exists() {
        files.push("oxidepm.db");
        println!("  {} oxidepm.db (database)", "✓".green());
    }

    if files.is_empty() {
        println!("  {} Nothing to backup", "!".yellow());
        return Ok(());
    }

    // Create tarball
    let output_cmd = Command::new("tar")
        .args(["czf", &output_file])
        .args(files.iter().map(|f| f.to_string()).collect::<Vec<_>>())
        .current_dir(&home)
        .output()?;

    if !output_cmd.status.success() {
        bail!(
            "Failed to create backup: {}",
            String::from_utf8_lossy(&output_cmd.stderr)
        );
    }

    // Move to current directory if it was created in home
    let final_path = if std::path::Path::new(&output_file).is_absolute() {
        output_file.clone()
    } else {
        let src = home.join(&output_file);
        let dst = std::env::current_dir()?.join(&output_file);
        if src.exists() {
            fs::rename(&src, &dst)?;
        }
        dst.to_string_lossy().to_string()
    };

    println!(
        "\n{} Backup saved to {}",
        "✓".green().bold(),
        final_path.cyan()
    );

    Ok(())
}

pub async fn execute_restore(file: &str) -> Result<()> {
    let file_path = std::path::Path::new(file);
    if !file_path.exists() {
        bail!("Backup file not found: {}", file);
    }

    let home = constants::oxidepm_home();
    fs::create_dir_all(&home)?;

    println!("{}", "Restoring OxidePM backup...".cyan().bold());

    let output = Command::new("tar")
        .args(["xzf", file, "-C", &home.to_string_lossy()])
        .output()?;

    if !output.status.success() {
        bail!(
            "Failed to extract backup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // List restored files
    let entries = fs::read_dir(&home)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "saved.json" || name_str == "notify.toml" || name_str == "oxidepm.db" {
            println!("  {} {}", "✓".green(), name_str);
        }
    }

    println!(
        "\n{} Backup restored. Run {} to start saved processes.",
        "✓".green().bold(),
        "oxidepm resurrect".cyan()
    );

    Ok(())
}
