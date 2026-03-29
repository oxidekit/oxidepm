//! Self-update command — downloads latest oxidepm release from GitHub

use anyhow::{bail, Result};
use colored::Colorize;
use std::fs;
use std::os::unix::fs::PermissionsExt;

const GITHUB_REPO: &str = "oxidekit/oxidepm";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn execute(version: Option<String>) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("oxidepm-updater")
        .build()?;

    // Resolve target version
    let (tag, download_url, asset_name) = if let Some(ref ver) = version {
        let tag = if ver.starts_with('v') {
            ver.clone()
        } else {
            format!("v{}", ver)
        };
        let (url, name) = resolve_asset_url(&client, &tag).await?;
        (tag, url, name)
    } else {
        // Get latest release
        let (tag, url, name) = resolve_latest(&client).await?;
        (tag, url, name)
    };

    let target_version = tag.trim_start_matches('v');

    // Check if already up to date
    if target_version == CURRENT_VERSION && version.is_none() {
        println!("{} Already up to date (v{})", "✓".green(), CURRENT_VERSION);
        return Ok(());
    }

    println!(
        "Updating oxidepm: v{} → {}",
        CURRENT_VERSION,
        tag.cyan()
    );
    println!("  Downloading: {}", asset_name);

    // Download to temp file
    let response = client.get(&download_url).send().await?;
    if !response.status().is_success() {
        bail!("Download failed: HTTP {}", response.status());
    }
    let bytes = response.bytes().await?;

    // Extract tarball
    let temp_dir = tempfile::tempdir()?;
    let tarball_path = temp_dir.path().join(&asset_name);
    fs::write(&tarball_path, &bytes)?;

    let output = std::process::Command::new("tar")
        .args(["xzf", &tarball_path.to_string_lossy(), "-C", &temp_dir.path().to_string_lossy()])
        .output()?;

    if !output.status.success() {
        bail!("Failed to extract tarball: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Find the oxidepm binary in extracted files
    let new_binary = find_binary(temp_dir.path(), "oxidepm")?;
    let new_daemon = find_binary(temp_dir.path(), "oxidepmd").ok();

    // Get current binary path
    let current_exe = std::env::current_exe()?;
    let current_dir = current_exe.parent().unwrap();

    // Backup current binaries
    let backup_path = current_exe.with_extension("pre-update");
    fs::copy(&current_exe, &backup_path)?;
    println!("  Backup: {}", backup_path.display());

    // Replace binaries
    fs::copy(&new_binary, &current_exe)?;
    fs::set_permissions(&current_exe, fs::Permissions::from_mode(0o755))?;
    println!("  {} oxidepm", "Updated".green());

    if let Some(new_daemon_path) = new_daemon {
        let daemon_path = current_dir.join("oxidepmd");
        if daemon_path.exists() {
            let daemon_backup = daemon_path.with_extension("pre-update");
            fs::copy(&daemon_path, &daemon_backup)?;
        }
        fs::copy(&new_daemon_path, &daemon_path)?;
        fs::set_permissions(&daemon_path, fs::Permissions::from_mode(0o755))?;
        println!("  {} oxidepmd", "Updated".green());
    }

    println!(
        "\n{} Updated to {} — restart the daemon with: oxidepm kill && oxidepmd &",
        "✓".green(),
        tag.cyan()
    );

    Ok(())
}

/// Find a binary by name in a directory (recursive)
fn find_binary(dir: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    // Check top-level first
    let direct = dir.join(name);
    if direct.exists() {
        return Ok(direct);
    }

    // Search subdirectories
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Ok(found) = find_binary(&path, name) {
                return Ok(found);
            }
        } else if path.file_name().map(|n| n == name).unwrap_or(false) {
            return Ok(path);
        }
    }

    bail!("Binary '{}' not found in extracted archive", name)
}

/// Resolve the download URL for a specific tag
async fn resolve_asset_url(
    client: &reqwest::Client,
    tag: &str,
) -> Result<(String, String)> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/tags/{}",
        GITHUB_REPO, tag
    );

    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

    let target = current_target();
    let assets = resp["assets"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No assets found for {}", tag))?;

    for asset in assets {
        let name = asset["name"].as_str().unwrap_or("");
        if name.contains(&target) && name.ends_with(".tar.gz") {
            let download_url = asset["browser_download_url"]
                .as_str()
                .unwrap_or("")
                .to_string();
            return Ok((download_url, name.to_string()));
        }
    }

    bail!(
        "No binary found for target '{}' in release {}. Available: {}",
        target,
        tag,
        assets
            .iter()
            .filter_map(|a| a["name"].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Get the latest release info
async fn resolve_latest(
    client: &reqwest::Client,
) -> Result<(String, String, String)> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

    let tag = resp["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No tag_name in latest release"))?
        .to_string();

    let (download_url, asset_name) = resolve_asset_url(client, &tag).await?;

    Ok((tag, download_url, asset_name))
}

/// Get the current platform target triple
fn current_target() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    match (arch, os) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        _ => format!("{}-{}", arch, os),
    }
}
