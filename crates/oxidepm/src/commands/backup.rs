//! Backup and restore commands

use crate::output::print_plan;
use anyhow::{bail, Context, Result};
use colored::Colorize;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use oxidepm_core::{constants, Paths};
use oxidepm_ipc::{DryRunPlan, PlannedItem};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The only files a backup contains and a restore will accept.
/// `state.db` is the database of this release; `oxidepm.db` is the one of older
/// releases (kept so old archives still restore, the daemon refuses to use it).
const BACKUP_MEMBERS: &[&str] = &["saved.json", "state.db", "oxidepm.db", "adapters.db"];

/// The database members and the sidecar files SQLite keeps next to each. A
/// stale sidecar next to a freshly restored database could be replayed into
/// it, so they are removed when the database is restored.
const DB_SIDECARS: &[(&str, &[&str])] = &[
    (
        "state.db",
        &["state.db-journal", "state.db-wal", "state.db-shm"],
    ),
    (
        "oxidepm.db",
        &["oxidepm.db-journal", "oxidepm.db-wal", "oxidepm.db-shm"],
    ),
    (
        "adapters.db",
        &["adapters.db-journal", "adapters.db-wal", "adapters.db-shm"],
    ),
];

pub async fn execute_backup(output: Option<String>, dry_run: bool) -> Result<()> {
    let home = constants::oxidepm_home();
    if !home.exists() {
        bail!("OxidePM home not found: {}", home.display());
    }

    let output_file = PathBuf::from(output.unwrap_or_else(|| "oxidepm-backup.tar.gz".to_string()));

    if !dry_run {
        println!("{}", "Creating OxidePM backup...".cyan().bold());
    }

    // The database is copied as a plain file; while the daemon writes to it the
    // copy can be inconsistent.
    if !dry_run && super::get_client().connect().await.is_ok() {
        eprintln!(
            "{} The daemon is running; the database copy may be inconsistent. \
             For a reliable backup run `oxidepm kill` first.",
            "[WARN]".yellow()
        );
    }

    // Collect files to backup
    let mut files: Vec<&str> = Vec::new();
    for (name, what) in [
        ("saved.json", "process list"),
        ("state.db", "database"),
        (
            "oxidepm.db",
            "database of an older OxidePM, not used by this version",
        ),
        // the installed adapters; their executables and state directories are not backed up (the
        // state holds tokens and keys, and the executables are imported and verified again)
        ("adapters.db", "installed adapters and grants"),
    ] {
        if home.join(name).exists() {
            files.push(name);
            if !dry_run {
                println!("  {} {} ({})", "✓".green(), name, what);
            }
        }
    }

    if files.is_empty() {
        println!("  {} Nothing to backup", "!".yellow());
        return Ok(());
    }

    if dry_run {
        return rehearse_backup(&home, &files, &output_file).await;
    }

    if let Err(e) = write_backup(&home, &files, &output_file) {
        let _ = fs::remove_file(&output_file);
        return Err(e);
    }

    let final_path = if output_file.is_absolute() {
        output_file
    } else {
        std::env::current_dir()?.join(&output_file)
    };

    println!(
        "\n{} Backup saved to {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan()
    );

    Ok(())
}

/// `backup --dry-run`: the files that would go into the archive with their sizes, and where it
/// would be written. Nothing is created.
async fn rehearse_backup(home: &Path, files: &[&str], output_file: &Path) -> Result<()> {
    let mut items: Vec<PlannedItem> = files
        .iter()
        .map(|name| {
            let size = fs::metadata(home.join(name)).map(|m| m.len()).unwrap_or(0);
            PlannedItem::new(None, *name, "add").detail(format!("{size} B"))
        })
        .collect();
    let dest = if output_file.is_absolute() {
        output_file.to_path_buf()
    } else {
        std::env::current_dir()?.join(output_file)
    };
    let detail = if dest.exists() {
        "replaces the file there now (mode 0600)"
    } else {
        "a new file (mode 0600)"
    };
    items.push(PlannedItem::new(None, dest.display().to_string(), "write").detail(detail));
    let mut warnings = Vec::new();
    if super::get_client().connect().await.is_ok() {
        warnings.push("the daemon is running; the database copy may be inconsistent (run `oxidepm kill` first)".to_string());
    }
    print_plan(&DryRunPlan::allowed("backup", items, warnings))
}

/// `restore --dry-run`: the members of the archive, the files of the home they would replace,
/// and what would stop the restore. Nothing is extracted and no directory is made.
async fn rehearse_restore(archive: &Path, home: &Path, yes: bool) -> Result<()> {
    let members = list_backup(archive)
        .with_context(|| format!("Refusing to restore {}", archive.display()))?;
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    let mut overwritten = Vec::new();
    for (name, size) in &members {
        let target = home.join(name);
        match fs::metadata(&target) {
            Ok(meta) => {
                overwritten.push(name.as_str());
                items.push(
                    PlannedItem::new(None, name.as_str(), "overwrite").detail(format!(
                        "{size} B from the archive replace {} B now in {}",
                        meta.len(),
                        target.display()
                    )),
                );
            }
            Err(_) => items.push(
                PlannedItem::new(None, name.as_str(), "restore")
                    .detail(format!("{size} B to {}", target.display())),
            ),
        }
        for (db, sidecars) in DB_SIDECARS {
            if name == db {
                for sidecar in *sidecars {
                    if home.join(sidecar).exists() {
                        items.push(
                            PlannedItem::new(None, *sidecar, "remove")
                                .detail("a stale sidecar of the database"),
                        );
                    }
                }
            }
        }
    }
    if super::get_client().connect().await.is_ok() {
        warnings.push(
            "the daemon is running: a real restore is refused until `oxidepm kill`".to_string(),
        );
    }
    if !overwritten.is_empty() && !yes {
        warnings.push(format!(
            "a real restore would overwrite {} and needs --yes",
            overwritten.join(", ")
        ));
    }
    print_plan(&DryRunPlan::allowed("restore", items, warnings))
}

/// Write the archive straight to its destination, readable by the owner only
/// (it contains tokens and process environments).
fn write_backup(home: &Path, files: &[&str], dest: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dest)
        .with_context(|| format!("Cannot create backup file {}", dest.display()))?;
    // `mode` only applies to newly created files; tighten an existing one too.
    fs::set_permissions(dest, fs::Permissions::from_mode(0o600))?;

    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    builder.mode(tar::HeaderMode::Deterministic);
    for name in files {
        builder
            .append_path_with_name(home.join(name), name)
            .with_context(|| format!("Failed to add {} to the backup", name))?;
    }
    let mut encoder = builder.into_inner()?.finish()?;
    encoder.flush()?;
    encoder.sync_all()?;
    Ok(())
}

pub async fn execute_restore(file: &str, yes: bool, dry_run: bool) -> Result<()> {
    let file_path = Path::new(file);
    if !file_path.is_file() {
        bail!("Backup file not found: {}", file);
    }

    let home = constants::oxidepm_home();
    if dry_run {
        return rehearse_restore(file_path, &home, yes).await;
    }
    Paths::new(&home).ensure_home()?;

    // 1. Unpack and validate in a scratch directory next to the real one
    //    (same filesystem, so the final moves are atomic renames).
    let scratch = tempfile::Builder::new()
        .prefix(".restore-")
        .tempdir_in(&home)
        .context("Cannot create a temporary directory in the OxidePM home")?;
    let members = unpack_backup(file_path, scratch.path())
        .with_context(|| format!("Refusing to restore {}", file))?;

    // 2. The daemon holds the database open; replacing files under it would
    //    be ineffective or corrupting.
    if super::get_client().connect().await.is_ok() {
        bail!("The daemon is running. Stop it first with `oxidepm kill`, then run restore again");
    }

    // 3. Never overwrite silently.
    let existing: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|m| home.join(m).exists())
        .collect();
    if !existing.is_empty() && !yes {
        bail!(
            "Restore would overwrite: {}. Re-run with --yes to confirm",
            existing.join(", ")
        );
    }

    println!("{}", "Restoring OxidePM backup...".cyan().bold());

    // 4. Move into place. Existing files are first moved aside so a failure
    //    part-way rolls back to the previous state instead of leaving a mix.
    let aside_dir = scratch.path().join("previous");
    fs::create_dir(&aside_dir)?;
    if let Err(e) = place_members(scratch.path(), &aside_dir, &home, &members) {
        bail!("{:#}. The previous files were left in place", e);
    }
    for (db, sidecars) in DB_SIDECARS {
        if members.iter().any(|m| m == db) {
            for sidecar in *sidecars {
                let _ = fs::remove_file(home.join(sidecar));
            }
        }
    }
    // an adapter that was enabled when the backup was made is not enabled now: the grant was
    // consent given on another day, and its files are not in the backup
    if members.iter().any(|m| m == "adapters.db") {
        if let Err(e) = reset_restored_adapters(&home, &aside_dir.join("adapters.db")).await {
            eprintln!("{} Could not reset the restored adapters: {:#}. Disable them with `oxidepm adapter disable <name>` before starting the daemon", "[WARN]".yellow(), e);
        }
    }
    for name in &members {
        println!("  {} {}", "✓".green(), name);
    }

    println!(
        "\n{} Backup restored. Run {} to start saved processes.",
        "✓".green().bold(),
        "oxidepm resurrect".cyan()
    );

    Ok(())
}

/// After `adapters.db` is restored: every adapter is disabled and holds no grant (enabling it
/// again needs fresh consent), and the index sequence kept for each signing key is the higher of
/// the restored one and the one of the database that was there before (`previous`, if any).
async fn reset_restored_adapters(home: &Path, previous: &Path) -> Result<()> {
    use oxidepm_db::Database;
    let restored = Database::open_adapters(&home.join("adapters.db"))
        .await
        .context("Cannot open the restored adapters.db")?;
    let repo = restored.adapters();
    for mut row in repo.list().await? {
        row.enabled = false;
        row.grant_json = None;
        row.grant_fingerprint = None;
        row.state_note =
            Some("restored from a backup: enable it again to give it rights".to_string());
        repo.update(&row).await?;
    }
    if previous.exists() {
        let old = Database::open_adapters(previous)
            .await
            .context("Cannot open the adapters.db that was there before")?;
        for (key, value) in old.adapters().meta_with_prefix("last_sequence:").await? {
            let theirs = value.parse::<u64>().ok();
            let ours = repo
                .get_meta(&key)
                .await?
                .and_then(|v| v.parse::<u64>().ok());
            if theirs > ours {
                repo.set_meta(&key, &value).await?;
            }
        }
        old.close().await;
    }
    restored.close().await;
    // the checkpoint left the file whole; no sidecar of the scratch copy belongs next to the real one
    for sidecar in ["adapters.db-wal", "adapters.db-shm", "adapters.db-journal"] {
        let _ = fs::remove_file(home.join(sidecar));
    }
    Ok(())
}

/// Move the unpacked `members` from `staged` into `home`, keeping replaced
/// files in `aside`. On any failure everything already moved is undone.
fn place_members(staged: &Path, aside: &Path, home: &Path, members: &[String]) -> Result<()> {
    let mut moved_aside: Vec<&String> = Vec::new();
    let mut placed: Vec<&String> = Vec::new();
    let mut run = || -> Result<()> {
        for name in members {
            let target = home.join(name);
            if target.exists() {
                fs::rename(&target, aside.join(name))
                    .with_context(|| format!("Failed to move the existing {} aside", name))?;
                moved_aside.push(name);
            }
            fs::rename(staged.join(name), &target)
                .with_context(|| format!("Failed to move {} into place", name))?;
            placed.push(name);
        }
        Ok(())
    };
    let result = run();
    if result.is_err() {
        for name in &placed {
            let _ = fs::remove_file(home.join(name));
        }
        for name in &moved_aside {
            let _ = fs::rename(aside.join(name), home.join(name));
        }
    }
    result
}

/// The whitelisted name of an archive member, or why the archive is refused. `seen` are the
/// names accepted before it.
fn check_member<R: std::io::Read>(
    entry: &tar::Entry<'_, R>,
    seen: &[String],
) -> Result<&'static str> {
    let shown = entry
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    if entry.header().entry_type() != tar::EntryType::Regular {
        bail!("Archive member '{}' is not a regular file (links, directories and special files are not allowed)", shown);
    }

    let name = expected_member(&entry.path()?).ok_or_else(|| {
        anyhow::anyhow!(
            "Unexpected archive member '{}' (only {} are allowed)",
            shown,
            BACKUP_MEMBERS.join(", ")
        )
    })?;
    if seen.iter().any(|m| m == name) {
        bail!("Archive contains '{}' more than once", name);
    }
    Ok(name)
}

/// The members of a backup and their sizes, checked like [`unpack_backup`] checks them, without
/// extracting anything.
fn list_backup(archive: &Path) -> Result<Vec<(String, u64)>> {
    let file =
        fs::File::open(archive).with_context(|| format!("Cannot open {}", archive.display()))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut members: Vec<(String, u64)> = Vec::new();
    for entry in tar.entries().context("Not a valid .tar.gz archive")? {
        let entry = entry.context("Corrupt archive")?;
        let seen: Vec<String> = members.iter().map(|(n, _)| n.clone()).collect();
        let name = check_member(&entry, &seen)?;
        members.push((name.to_string(), entry.header().size().unwrap_or(0)));
    }
    if members.is_empty() {
        bail!("Archive contains none of: {}", BACKUP_MEMBERS.join(", "));
    }
    Ok(members)
}

/// Extract a backup into `dest`, accepting only the expected plain files.
/// Returns the names that were unpacked. Nothing outside `dest` is touched,
/// and file names come from our own whitelist, never from the archive.
fn unpack_backup(archive: &Path, dest: &Path) -> Result<Vec<String>> {
    let file =
        fs::File::open(archive).with_context(|| format!("Cannot open {}", archive.display()))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut members: Vec<String> = Vec::new();

    for entry in tar.entries().context("Not a valid .tar.gz archive")? {
        let mut entry = entry.context("Corrupt archive")?;
        let name = check_member(&entry, &members)?;

        let target = dest.join(name);
        entry
            .unpack(&target)
            .with_context(|| format!("Failed to extract {}", name))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        members.push(name.to_string());
    }

    if members.is_empty() {
        bail!("Archive contains none of: {}", BACKUP_MEMBERS.join(", "));
    }
    Ok(members)
}

/// Map an archive path to a whitelisted file name: a single plain component
/// (an optional leading `./` is tolerated). Absolute paths, `..` and
/// sub-directories never match.
fn expected_member(path: &Path) -> Option<&'static str> {
    let mut normal = None;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(c) if normal.is_none() => normal = Some(c),
            _ => return None,
        }
    }
    let normal = normal?.to_str()?;
    BACKUP_MEMBERS.iter().copied().find(|m| *m == normal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_placement_rolls_back_to_the_previous_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (staged, aside, home) = (
            tmp.path().join("s"),
            tmp.path().join("a"),
            tmp.path().join("h"),
        );
        for d in [&staged, &aside, &home] {
            fs::create_dir(d).unwrap();
        }
        fs::write(home.join("saved.json"), b"OLD").unwrap();
        fs::write(staged.join("saved.json"), b"NEW").unwrap();
        // state.db was never staged, so its move fails after saved.json succeeded
        let members = vec!["saved.json".to_string(), "state.db".to_string()];
        assert!(place_members(&staged, &aside, &home, &members).is_err());
        assert_eq!(fs::read(home.join("saved.json")).unwrap(), b"OLD");
        assert!(!home.join("state.db").exists());

        // with everything staged the same call succeeds
        fs::write(staged.join("state.db"), b"n").unwrap();
        fs::write(staged.join("saved.json"), b"NEW").unwrap();
        place_members(&staged, &aside, &home, &members).unwrap();
        assert_eq!(fs::read(home.join("saved.json")).unwrap(), b"NEW");
        assert_eq!(fs::read(home.join("state.db")).unwrap(), b"n");
    }

    #[test]
    fn expected_member_accepts_only_plain_whitelisted_names() {
        assert_eq!(expected_member(Path::new("saved.json")), Some("saved.json"));
        assert_eq!(
            expected_member(Path::new("./oxidepm.db")),
            Some("oxidepm.db")
        );
        assert_eq!(expected_member(Path::new("state.db")), Some("state.db"));
        assert_eq!(expected_member(Path::new("state.db-wal")), None);
        // the notifier keeps its configuration in its own state directory, which is not backed up
        assert_eq!(expected_member(Path::new("notify.toml")), None);
        // the installed adapters travel; their state directories and executables do not
        assert_eq!(
            expected_member(Path::new("adapters.db")),
            Some("adapters.db")
        );
        assert_eq!(expected_member(Path::new("adapters.db-wal")), None);
        assert_eq!(expected_member(Path::new("adapters/state/x/token")), None);
        assert_eq!(expected_member(Path::new("adapter-pids.json")), None);

        assert_eq!(expected_member(Path::new("../saved.json")), None);
        assert_eq!(expected_member(Path::new("/saved.json")), None);
        assert_eq!(expected_member(Path::new("a/saved.json")), None);
        assert_eq!(expected_member(Path::new("saved.json/..")), None);
        assert_eq!(expected_member(Path::new("other.txt")), None);
        assert_eq!(expected_member(Path::new("")), None);
        assert_eq!(expected_member(Path::new(".")), None);
    }
}
