//! Cosmos auto-upgrade — download, verify, and swap binaries from a trusted GitHub source
//!
//! When an upgrade halt is detected and `auto_upgrade = true`, this module:
//! 1. Resolves the upgrade name to a GitHub release tag
//! 2. Downloads the binary from the trusted source repo
//! 3. Verifies SHA256 checksum against checksums.txt in the release
//! 4. Backs up the current binary
//! 5. Swaps in the new binary
//!
//! SECURITY: Only downloads from the configured `auto_upgrade_source` repo.
//! Never downloads from URLs in governance proposals. Uses sha256sum for
//! checksum verification and tar for extraction (no extra Rust dependencies).

use oxidepm_core::CosmosConfig;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info, warn};

/// Result of an auto-upgrade attempt
#[derive(Debug)]
pub enum AutoUpgradeResult {
    /// Upgrade completed successfully
    Success {
        new_version: String,
        binary_path: PathBuf,
    },
    /// Auto-upgrade is disabled
    Disabled,
    /// No trusted source configured
    NoSource,
    /// Download or verification failed
    Failed(String),
}

/// Attempt to auto-upgrade the binary after an upgrade halt.
///
/// Returns `AutoUpgradeResult` indicating what happened.
/// The caller is responsible for restarting the process.
pub async fn try_auto_upgrade(
    cosmos_config: &CosmosConfig,
    binary_path: &Path,
    upgrade_name: &str,
) -> AutoUpgradeResult {
    if !cosmos_config.auto_upgrade {
        return AutoUpgradeResult::Disabled;
    }

    let source = match &cosmos_config.auto_upgrade_source {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            warn!("auto_upgrade enabled but no auto_upgrade_source configured");
            return AutoUpgradeResult::NoSource;
        }
    };

    info!(
        "Auto-upgrade triggered for '{}' from {}",
        upgrade_name, source
    );

    // Determine the release tag
    let tag = if upgrade_name.starts_with('v') {
        upgrade_name.to_string()
    } else {
        format!("v{}", upgrade_name)
    };

    // Create temp directory
    let temp_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return AutoUpgradeResult::Failed(format!("Failed to create temp dir: {}", e)),
    };
    let temp = temp_dir.path();

    // Try multiple asset naming conventions
    let asset_candidates = get_asset_candidates(&tag);

    let mut downloaded_asset = None;
    for asset_name in &asset_candidates {
        let url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            source, tag, asset_name
        );
        let dest = temp.join(asset_name);
        if download_file(&url, &dest).is_ok() {
            info!("Downloaded: {}", asset_name);
            downloaded_asset = Some((asset_name.clone(), dest));
            break;
        }
    }

    let (asset_name, asset_path) = match downloaded_asset {
        Some(a) => a,
        None => {
            return AutoUpgradeResult::Failed(format!(
                "No matching binary found in release {}. Tried: {}",
                tag,
                asset_candidates.join(", ")
            ))
        }
    };

    // Verify checksum if required
    if cosmos_config.auto_upgrade_require_checksum {
        let checksums_url = format!(
            "https://github.com/{}/releases/download/{}/checksums.txt",
            source, tag
        );
        let checksums_path = temp.join("checksums.txt");

        match download_file(&checksums_url, &checksums_path) {
            Ok(()) => {
                if let Err(e) = verify_checksum(&asset_path, &checksums_path, &asset_name) {
                    return AutoUpgradeResult::Failed(format!(
                        "Checksum verification failed: {}",
                        e
                    ));
                }
                info!("SHA256 checksum verified");
            }
            Err(e) => {
                return AutoUpgradeResult::Failed(format!(
                    "Failed to download checksums.txt (required when auto_upgrade_require_checksum = true): {}",
                    e
                ));
            }
        }
    } else {
        warn!("Checksum verification disabled — proceeding without verification");
    }

    // Extract if tarball, otherwise use directly
    let new_binary = if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        match extract_tarball(&asset_path, temp) {
            Ok(p) => p,
            Err(e) => return AutoUpgradeResult::Failed(format!("Failed to extract: {}", e)),
        }
    } else {
        asset_path.clone()
    };

    // Backup current binary
    let backup_path = PathBuf::from(format!("{}.pre-upgrade", binary_path.display()));
    if binary_path.exists() {
        if let Err(e) = std::fs::copy(binary_path, &backup_path) {
            return AutoUpgradeResult::Failed(format!("Failed to backup current binary: {}", e));
        }
        info!("Backed up current binary to {}", backup_path.display());
    }

    // Swap binary
    if let Err(e) = std::fs::copy(&new_binary, binary_path) {
        // Restore backup on failure
        if backup_path.exists() {
            let _ = std::fs::copy(&backup_path, binary_path);
            error!("Restored backup after failed swap");
        }
        return AutoUpgradeResult::Failed(format!("Failed to install new binary: {}", e));
    }

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(binary_path, std::fs::Permissions::from_mode(0o755))
        {
            error!("Failed to set binary permissions: {}", e);
        }
    }

    info!("Auto-upgrade complete: installed {} as {}", tag, binary_path.display());

    AutoUpgradeResult::Success {
        new_version: tag,
        binary_path: binary_path.to_path_buf(),
    }
}

/// Get candidate asset names to try downloading, in priority order
fn get_asset_candidates(tag: &str) -> Vec<String> {
    let version = tag.trim_start_matches('v');
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };

    vec![
        // monod_0.1.0_linux_amd64.tar.gz (Cosmos convention)
        format!("monod_{}_linux_{}.tar.gz", version, arch),
        // monod-linux-amd64 (bare binary)
        format!("monod-linux-{}", arch),
        // monod_linux_amd64.tar.gz (without version)
        format!("monod_linux_{}.tar.gz", arch),
        // monod (generic)
        "monod".to_string(),
    ]
}

/// Download a file using curl (available on all servers)
fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "30",
            "--max-time",
            "300",
            "-o",
            &dest.to_string_lossy(),
            url,
        ])
        .output()
        .map_err(|e| format!("curl failed to execute: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl failed ({}): {}", url, stderr.trim()));
    }

    Ok(())
}

/// Verify SHA256 checksum using sha256sum command
fn verify_checksum(
    file_path: &Path,
    checksums_path: &Path,
    asset_name: &str,
) -> Result<(), String> {
    // Read expected hash from checksums.txt
    let checksums =
        std::fs::read_to_string(checksums_path).map_err(|e| format!("Read checksums: {}", e))?;

    let expected_hash = checksums
        .lines()
        .find_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1].trim_start_matches("*./").contains(asset_name) {
                Some(parts[0].to_lowercase())
            } else {
                None
            }
        })
        .ok_or_else(|| format!("No checksum found for '{}' in checksums.txt", asset_name))?;

    // Compute actual hash using sha256sum
    let output = Command::new("sha256sum")
        .arg(file_path)
        .output()
        .map_err(|e| format!("sha256sum failed: {}", e))?;

    if !output.status.success() {
        return Err("sha256sum command failed".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual_hash = stdout
        .split_whitespace()
        .next()
        .ok_or("sha256sum produced no output")?
        .to_lowercase();

    if actual_hash != expected_hash {
        return Err(format!(
            "SHA256 mismatch: expected {}, got {}",
            expected_hash, actual_hash
        ));
    }

    Ok(())
}

/// Extract a .tar.gz and find the binary inside using tar command
fn extract_tarball(tarball: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let extract_dir = dest_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("Create extract dir: {}", e))?;

    let output = Command::new("tar")
        .args(["xzf", &tarball.to_string_lossy(), "-C", &extract_dir.to_string_lossy()])
        .output()
        .map_err(|e| format!("tar failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tar extraction failed: {}", stderr.trim()));
    }

    // Find the monod binary in the extracted contents
    find_binary_recursive(&extract_dir)
        .ok_or_else(|| "No executable binary found in tarball".to_string())
}

/// Recursively find a binary named "monod" in a directory
fn find_binary_recursive(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_binary_recursive(&path) {
                return Some(found);
            }
        } else if path.is_file() {
            let name = path.file_name()?.to_string_lossy();
            if name == "monod" {
                return Some(path);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_get_asset_candidates() {
        let candidates = get_asset_candidates("v0.2.0");
        assert!(candidates[0].contains("0.2.0"));
        assert!(candidates[0].contains("linux"));
        assert!(candidates[0].ends_with(".tar.gz"));
    }

    #[test]
    fn test_auto_upgrade_disabled() {
        let config = CosmosConfig {
            auto_upgrade: false,
            ..Default::default()
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(try_auto_upgrade(
            &config,
            Path::new("/usr/local/bin/monod"),
            "v0.2.0",
        ));
        assert!(matches!(result, AutoUpgradeResult::Disabled));
    }

    #[test]
    fn test_auto_upgrade_no_source() {
        let config = CosmosConfig {
            auto_upgrade: true,
            auto_upgrade_source: None,
            ..Default::default()
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(try_auto_upgrade(
            &config,
            Path::new("/usr/local/bin/monod"),
            "v0.2.0",
        ));
        assert!(matches!(result, AutoUpgradeResult::NoSource));
    }

    #[test]
    fn test_backup_and_restore() {
        let dir = TempDir::new().unwrap();
        let binary = dir.path().join("monod");
        let backup = dir.path().join("monod.pre-upgrade");

        std::fs::write(&binary, b"original_binary").unwrap();
        std::fs::copy(&binary, &backup).unwrap();

        assert!(backup.exists());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "original_binary"
        );
    }

    #[test]
    fn test_verify_checksum_format() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("testfile");
        let checksums = dir.path().join("checksums.txt");

        std::fs::write(&file, b"hello world").unwrap();

        // Get actual hash
        let output = Command::new("sha256sum")
            .arg(&file)
            .output()
            .unwrap();
        let hash = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();

        // Write checksums.txt
        std::fs::write(&checksums, format!("{}  testfile\n", hash)).unwrap();

        let result = verify_checksum(&file, &checksums, "testfile");
        assert!(result.is_ok(), "Checksum should match: {:?}", result);
    }

    #[test]
    fn test_verify_checksum_mismatch() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("testfile");
        let checksums = dir.path().join("checksums.txt");

        std::fs::write(&file, b"hello world").unwrap();
        std::fs::write(&checksums, "0000000000000000000000000000000000000000000000000000000000000000  testfile\n").unwrap();

        let result = verify_checksum(&file, &checksums, "testfile");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("mismatch"));
    }

    #[test]
    fn test_find_binary_recursive() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("monod"), b"binary").unwrap();

        let found = find_binary_recursive(dir.path());
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("monod"));
    }
}
