//! Self-update command: downloads a release from GitHub, verifies it and installs it.
//!
//! The archive is trusted only through its sha256 in the `checksums.txt` of the same release,
//! and everything is checked before a single installed file is touched: the exact asset name,
//! the size, the hash, the safe extraction. The files are staged next to their destinations
//! first and renamed over them afterwards (a rename works over a running executable; a copy does
//! not), the new `oxidepm` is run with `--version`, and the old files are put back if anything
//! after the first rename fails.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::output::print_plan;
use oxidepm_core::release_bundle::DIRECTORY as BUNDLE_PATH;
use oxidepm_core::Error;
use oxidepm_fetch::{
    check_version, extract_tar_gz, fetch_bytes, fetch_verified, hex, Expect, ExtractLimits,
    ExtractedKind, FetchOpts, StagedFile,
};
use oxidepm_ipc::{CallOptions, DryRunPlan, IpcClient, PlannedItem, Request, Response};

const GITHUB_REPO: &str = "oxidekit/oxidepm";
const GITHUB_API: &str = "https://api.github.com";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The release listing (a JSON document).
const MAX_RELEASE_JSON: u64 = 4 * 1024 * 1024;
/// `checksums.txt`.
const MAX_CHECKSUMS: u64 = 64 * 1024;
/// The release archive.
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
/// How long a daemon gets to stop its apps and exit (the default shutdown budget is 120 s).
const DAEMON_STOP_WAIT: Duration = Duration::from_secs(150);
/// How long `oxidepm --version` of the installed file may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The executables of a release archive, in the order they are installed. The first is required;
/// the others are installed when the archive has them (`oxidepm-sandbox` arrives with the
/// adapter launcher).
const BINARIES: [&str; 3] = ["oxidepm", "oxidepmd", "oxidepm-sandbox"];

/// Everything `update` reads from its surroundings, so that a test can point it at a local
/// server, a temporary install directory and a made-up daemon.
struct Env {
    /// Base URL of the release API.
    api: String,
    opts: FetchOpts,
    /// The running `oxidepm`; its directory is where the files are installed.
    current_exe: PathBuf,
    current_version: String,
    target: String,
    client: IpcClient,
    daemon_wait: Duration,
}

impl Env {
    fn real() -> Result<Self> {
        Ok(Self {
            api: GITHUB_API.to_string(),
            opts: FetchOpts::default(),
            current_exe: std::env::current_exe()
                .context("Cannot determine the path of the running oxidepm")?,
            current_version: CURRENT_VERSION.to_string(),
            target: current_target(),
            client: super::get_client(),
            daemon_wait: DAEMON_STOP_WAIT,
        })
    }

    fn install_dir(&self) -> Result<&Path> {
        self.current_exe
            .parent()
            .context("Cannot determine the directory of the running oxidepm")
    }
}

pub async fn execute(version: Option<String>, force: bool, dry_run: bool) -> Result<()> {
    execute_at(&Env::real()?, version, force, dry_run).await
}

/// What the release listing says about the release to install.
struct Release {
    version: String,
    asset_name: String,
    asset_url: String,
    checksums_url: String,
}

/// `update`. With `dry_run` the release is looked up, the archive is downloaded, verified and
/// unpacked in a private temporary directory that is deleted again, and the plan printed;
/// nothing is installed and no daemon is stopped.
async fn execute_at(env: &Env, version: Option<String>, force: bool, dry_run: bool) -> Result<()> {
    // `--version` goes into a URL: check it first
    let wanted = match &version {
        Some(v) => {
            let v = v.strip_prefix('v').unwrap_or(v).to_string();
            check_version(&v, &env.current_version, force)?;
            Some(v)
        }
        None => None,
    };
    let dir = env.install_dir()?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_installation(dir).await?)
    };
    let release = resolve_release(env, wanted.as_deref()).await?;
    if wanted.is_none() {
        if release.version == env.current_version {
            println!(
                "{} Already up to date (v{})",
                "✓".green(),
                env.current_version
            );
            return Ok(());
        }
        check_version(&release.version, &env.current_version, force)?;
    }

    println!(
        "Updating oxidepm: v{} → {}",
        env.current_version,
        format!("v{}", release.version).cyan()
    );

    // a private working directory: the archive and its unpacked files live here and nowhere else
    let work = private_tempdir()?;
    let checksums = fetch_bytes(&release.checksums_url, MAX_CHECKSUMS, &env.opts)
        .await
        .context("Could not download checksums.txt of the release")?;
    let sha256 = checksum_for(&String::from_utf8_lossy(&checksums), &release.asset_name)?;

    println!("  Downloading: {}", release.asset_name);
    let archive = work.path().join("release.tar.gz");
    let verified = fetch_verified(
        &release.asset_url,
        &Expect {
            sha256,
            max_size: MAX_ARCHIVE,
        },
        &archive,
        &env.opts,
    )
    .await
    .with_context(|| format!("Could not download {}", release.asset_name))?;
    println!(
        "  Verified:    sha256 {} ({} bytes)",
        hex(&verified.sha256),
        verified.size
    );

    let unpacked = work.path().join("files");
    fs::create_dir(&unpacked)?;
    let found = extract_tar_gz(&archive, &unpacked, &ExtractLimits::default())
        .context("The release archive was refused")?;
    let mut new_files: Vec<(&'static str, PathBuf)> = Vec::new();
    for name in BINARIES {
        let mut matches = found
            .iter()
            .filter(|e| e.kind == ExtractedKind::File && is_binary_path(&e.path, name))
            .map(|e| unpacked.join(&e.path));
        match (matches.next(), matches.next()) {
            (Some(path), None) => new_files.push((name, path)),
            (Some(_), Some(_)) => bail!("The release archive holds more than one '{}'", name),
            (None, _) if name == "oxidepm" => {
                bail!("The release archive holds no 'oxidepm' binary")
            }
            (None, _) => {}
        }
    }

    let bundle_source = unpacked.join(BUNDLE_PATH);
    let bundle_destination = oxidepm_core::release_bundle::directory(&env.current_exe)?;
    let has_bundle = bundle_source.is_dir();
    if dry_run {
        let daemon_up = env.client.connect().await.is_ok();
        let mut plan = update_plan(env, &release, &verified, &new_files, dir, daemon_up);
        if has_bundle {
            plan.items.push(
                PlannedItem::new(None, bundle_destination.display().to_string(), "replace")
                    .detail("The bundled adapters are installed with the executables."),
            );
        }
        return print_plan(&plan);
    }

    // 1. stage every file next to its destination: a failure here changes nothing installed
    let mut staged = Vec::new();
    for (name, source) in &new_files {
        // the running CLI is replaced where it is, whatever it is called
        let destination = if *name == "oxidepm" {
            env.current_exe.clone()
        } else {
            dir.join(name)
        };
        staged.push((
            destination.clone(),
            StagedFile::stage(source, &destination, 0o755)
                .with_context(|| format!("Could not stage {}", destination.display()))?,
        ));
    }

    let mut staged_bundle = if has_bundle {
        Some(StagedBundle::stage(&bundle_source, &bundle_destination)?)
    } else {
        None
    };

    let mut backups = tempfile::Builder::new()
        .prefix(".oxidepm-update-")
        .tempdir_in(dir)?;
    if stop_running_daemon(env).await? {
        println!("  Stopped the running daemon (the update replaces its executable).");
    }

    let mut done = Vec::new();
    let outcome = async {
        for (destination, file) in staged {
            install_one(&destination, file, backups.path(), &mut done)
                .with_context(|| format!("Could not install {}", destination.display()))?;
            println!("  {} {}", "Updated".green(), destination.display());
        }
        if let Some(bundle) = &staged_bundle {
            bundle
                .install(&bundle_destination, &mut done)
                .with_context(|| format!("Could not install {}", bundle_destination.display()))?;
            println!("  {} {}", "Updated".green(), bundle_destination.display());
        }
        probe(&env.current_exe, &release.version)
            .await
            .context("The new oxidepm does not work")
    }
    .await;
    if let Err(error) = outcome {
        let restored = restore(&done, &mut backups, staged_bundle.as_mut());
        bail!("{error:#}. {restored}");
    }

    let mut saved = false;
    for installed in &done {
        if let Some(backup) = &installed.backup {
            let destination = backup_path(&installed.destination);
            let result = (|| {
                if installed.directory {
                    remove_bundle(&destination)?;
                }
                fs::rename(backup, &destination)?;
                sync_parent(&destination)
            })();
            match result {
                Ok(()) => saved = true,
                Err(error) => {
                    preserve_backups(&mut backups, staged_bundle.as_mut());
                    eprintln!(
                        "Could not publish backup {}: {error:#}",
                        destination.display()
                    );
                }
            }
        }
    }
    if saved {
        println!("  Backups are kept as <name>.pre-update next to the files.");
    }

    println!(
        "\n{} Updated to {} — integrity checked against checksums.txt of the release; authenticity is not checked (no release signing key is embedded yet).",
        "✓".green(),
        format!("v{}", release.version).cyan()
    );
    println!("  Start the daemon again with: oxidepmd &   (or restart its service)");
    Ok(())
}

struct InstallationLock(fs::File);

impl Drop for InstallationLock {
    fn drop(&mut self) {
        // A concurrent fork can briefly hold another reference to the open file description.
        // SAFETY: unlock the directory while this guard still owns its descriptor.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Lock the install directory itself so every updater locks the same inode.
async fn lock_installation(dir: &Path) -> Result<InstallationLock> {
    let directory =
        fs::File::open(dir).with_context(|| format!("Could not open {}", dir.display()))?;
    tokio::task::spawn_blocking(move || loop {
        // SAFETY: the descriptor stays open for the lifetime of the installation lock.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(InstallationLock(directory));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("Could not lock the installation directory");
        }
    })
    .await?
}

/// A private (0700) temporary directory, deleted when dropped. (`tempfile` leaves the mode to
/// the umask.)
fn private_tempdir() -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .prefix("oxidepm-update-")
        .tempdir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// `name` at the top of the archive, or one directory down (`oxidepm-0.4.0/oxidepm`).
fn is_binary_path(path: &Path, name: &str) -> bool {
    let parts: Vec<_> = path.components().collect();
    parts.last().map(|c| c.as_os_str() == name).unwrap_or(false) && parts.len() <= 2
}

/// Ask the release API for the release (the latest, or `v<wanted>`) and pick the assets.
async fn resolve_release(env: &Env, wanted: Option<&str>) -> Result<Release> {
    let url = match wanted {
        Some(v) => format!("{}/repos/{}/releases/tags/v{}", env.api, GITHUB_REPO, v),
        None => format!("{}/repos/{}/releases/latest", env.api, GITHUB_REPO),
    };
    let body = fetch_bytes(&url, MAX_RELEASE_JSON, &env.opts)
        .await
        .context("Could not look up the release")?;
    let json: serde_json::Value =
        serde_json::from_slice(&body).context("The release listing is not valid JSON")?;
    let tag = json["tag_name"]
        .as_str()
        .context("No tag_name in the release")?;
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    check_version(&version, &env.current_version, true)
        .context("The release has an unusable tag")?;
    if let Some(wanted) = wanted {
        if version != wanted {
            bail!("Asked for v{} but the server answered with {}", wanted, tag);
        }
    }
    let assets = json["assets"]
        .as_array()
        .with_context(|| format!("No assets found for {}", tag))?;
    // exactly this name; a name that merely contains the target is not the archive
    let asset_name = format!("oxidepm-{}.tar.gz", env.target);
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_string)
    };
    let asset_url = url_of(&asset_name).with_context(|| {
        format!(
            "Release {} has no asset named exactly '{}'. Available: {}",
            tag,
            asset_name,
            assets
                .iter()
                .filter_map(|a| a["name"].as_str())
                .map(printable)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let checksums_url = url_of("checksums.txt")
        .with_context(|| format!("Release {} has no checksums.txt; refusing to install an archive that cannot be checked", tag))?;
    Ok(Release {
        version,
        asset_name,
        asset_url,
        checksums_url,
    })
}

/// A name from the network, safe to print.
fn printable(s: &str) -> String {
    s.chars()
        .take(80)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The sha256 that `checksums.txt` (the output of `sha256sum`) gives for the file `name`.
fn checksum_for(text: &str, name: &str) -> Result<[u8; 32]> {
    let mut found: Option<[u8; 32]> = None;
    for line in text.lines() {
        let Some((digest, file)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let file = file.trim_start().trim_start_matches('*');
        if file != name {
            continue;
        }
        let digest = parse_sha256(digest)
            .with_context(|| format!("checksums.txt has a bad digest for {name}"))?;
        if found
            .replace(digest)
            .is_some_and(|earlier| earlier != digest)
        {
            bail!("checksums.txt lists {} twice with different digests", name);
        }
    }
    found.with_context(|| format!("checksums.txt does not list {}", name))
}

fn parse_sha256(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// What `update --dry-run` reports: the archive that was downloaded and checked, and the files
/// that would be replaced.
fn update_plan(
    env: &Env,
    release: &Release,
    verified: &oxidepm_fetch::Verified,
    new_files: &[(&'static str, PathBuf)],
    dir: &Path,
    daemon_up: bool,
) -> DryRunPlan {
    let mut items = vec![
        PlannedItem::new(None, release.asset_name.clone(), "verified").detail(format!(
            "{} ({} bytes, sha256 {} matches checksums.txt)",
            release.asset_url,
            verified.size,
            hex(&verified.sha256)
        )),
    ];
    if daemon_up {
        items.push(
            PlannedItem::new(None, "oxidepmd", "stop").detail(
                "the running daemon is stopped before its executable is replaced".to_string(),
            ),
        );
    }
    for (name, _) in new_files {
        items.push(
            PlannedItem::new(None, dir.join(name).display().to_string(), "replace").detail(
                format!(
                    "v{} -> v{} (the old file is kept as {}.pre-update)",
                    env.current_version, release.version, name
                ),
            ),
        );
    }
    DryRunPlan::allowed(
        "update",
        items,
        vec![
            "the archive was downloaded into a private temporary directory, checked and unpacked there, and that directory is deleted; nothing was installed".to_string(),
            "integrity is checked against checksums.txt of the same release; authenticity is not (no release signing key is embedded yet)".to_string(),
        ],
    )
}

/// Stop a running daemon and wait until it is gone. Returns whether one was running. Uses
/// `Kill`, which crosses protocol versions: a daemon of the first protocol is stopped too.
async fn stop_running_daemon(env: &Env) -> Result<bool> {
    let sent = env
        .client
        .send_with(&Request::Kill, &CallOptions::default())
        .await;
    match sent {
        Ok(Response::Error { message, .. }) => bail!(
            "The running daemon would not stop: {}. Nothing was replaced.",
            message
        ),
        Ok(_) => {}
        Err(Error::DaemonNotRunning) => return Ok(false),
        // the daemon may close the connection while it shuts down: what counts is that it is gone
        Err(_) => {}
    }
    let deadline = tokio::time::Instant::now() + env.daemon_wait;
    loop {
        if matches!(env.client.connect().await, Err(Error::DaemonNotRunning)) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "The running daemon did not stop within {} s. Nothing was replaced.",
                env.daemon_wait.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A file that was replaced, and the copy of what it was.
struct Installed {
    destination: PathBuf,
    /// The previous version in this update's private working directory.
    backup: Option<PathBuf>,
    directory: bool,
}

fn backup_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".pre-update");
    destination.with_file_name(name)
}

/// Save the old file in this update's private directory, then rename the staged file over it.
fn install_one(
    destination: &Path,
    staged: StagedFile,
    backups: &Path,
    done: &mut Vec<Installed>,
) -> Result<()> {
    let backup = match fs::symlink_metadata(destination) {
        Ok(meta) if meta.is_file() => {
            let backup = backups.join(destination.file_name().context("Missing executable name")?);
            if fs::hard_link(destination, &backup).is_err() {
                fs::copy(destination, &backup)
                    .with_context(|| format!("Could not back up {}", destination.display()))?;
            }
            Some(backup)
        }
        _ => None,
    };
    fs::rename(staged.staged_path(), destination)?;
    done.push(Installed {
        destination: destination.to_path_buf(),
        backup,
        directory: false,
    });
    sync_parent(destination)
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("Missing installation directory")?;
    fs::File::open(parent)?
        .sync_all()
        .with_context(|| format!("Could not sync {}", parent.display()))
}

#[derive(Default)]
struct CreatedDirs(Vec<PathBuf>);

impl Drop for CreatedDirs {
    fn drop(&mut self) {
        for dir in self.0.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
}

struct StagedBundle {
    work: tempfile::TempDir,
    _created: CreatedDirs,
}

impl StagedBundle {
    fn stage(source: &Path, destination: &Path) -> Result<Self> {
        let parent = destination.parent().context("Missing bundle parent")?;
        let share = parent.parent().context("Missing share directory")?;
        let mut created = CreatedDirs::default();
        for path in [share, parent] {
            match fs::create_dir(path) {
                Ok(()) => created.0.push(path.to_path_buf()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && fs::symlink_metadata(path)?.is_dir() => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Could not create {}", path.display()))
                }
            }
        }
        let work = tempfile::Builder::new()
            .prefix(".oxidepm-bundle-")
            .tempdir_in(parent)?;
        copy_bundle(source, &work.path().join("adapters"))
            .with_context(|| format!("Could not stage {}", destination.display()))?;
        Ok(Self {
            work,
            _created: created,
        })
    }

    fn install(&self, destination: &Path, done: &mut Vec<Installed>) -> Result<()> {
        let backup = match fs::symlink_metadata(destination) {
            Ok(meta) if meta.is_dir() => {
                let backup = self.work.path().join("previous");
                fs::rename(destination, &backup)?;
                Some(backup)
            }
            Ok(_) => bail!("{} is not a directory", destination.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // Record the backup even if the next rename or directory sync fails.
        done.push(Installed {
            destination: destination.to_path_buf(),
            backup,
            directory: true,
        });
        fs::rename(self.work.path().join("adapters"), destination)?;
        sync_parent(destination)
    }
}

fn copy_bundle(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_bundle(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            fs::File::open(target)?.sync_all()?;
        }
    }
    fs::File::open(destination)?.sync_all()
}

fn remove_bundle(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Put the old files back, newest first. Returns the sentence that says how it went.
fn restore(
    done: &[Installed],
    backups: &mut tempfile::TempDir,
    bundle: Option<&mut StagedBundle>,
) -> String {
    let mut failures = Vec::new();
    for installed in done.iter().rev() {
        let result = if installed.directory {
            remove_bundle(&installed.destination).and_then(|()| match &installed.backup {
                Some(backup) => fs::rename(backup, &installed.destination),
                None => Ok(()),
            })
        } else {
            match &installed.backup {
                Some(backup) => fs::rename(backup, &installed.destination),
                None => fs::remove_file(&installed.destination),
            }
        };
        if let Err(error) = result
            .context("restoring the previous installation")
            .and_then(|()| sync_parent(&installed.destination))
        {
            failures.push(format!("{}: {error:#}", installed.destination.display()));
        }
    }
    if failures.is_empty() {
        if done.is_empty() {
            "No file was replaced.".to_string()
        } else {
            "The old files were put back. A daemon that was running was stopped: start it again."
                .to_string()
        }
    } else {
        preserve_backups(backups, bundle);
        format!(
            "Putting the old files back failed for: {}.",
            failures.join("; ")
        )
    }
}

fn preserve_backups(backups: &mut tempfile::TempDir, bundle: Option<&mut StagedBundle>) {
    backups.disable_cleanup(true);
    eprintln!("Recovery files are in {}", backups.path().display());
    if let Some(bundle) = bundle {
        bundle.work.disable_cleanup(true);
        eprintln!(
            "Bundle recovery files are in {}",
            bundle.work.path().display()
        );
    }
}

/// Run `<exe> --version` and check that it reports `version`.
async fn probe(exe: &Path, version: &str) -> Result<()> {
    let exe = exe.to_path_buf();
    let version = version.to_string();
    tokio::task::spawn_blocking(move || probe_blocking(&exe, &version)).await?
}

fn probe_blocking(exe: &Path, version: &str) -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let started = std::time::Instant::now();
    // a file that was just written can be "busy" for a moment while another process still holds it
    let mut child = loop {
        match Command::new(exe)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(26) && started.elapsed() < PROBE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("could not run {} --version", exe.display()))
            }
        }
    };
    let mut stdout = child.stdout.take().context("no output pipe")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stdout).take(64 * 1024).read_to_string(&mut text);
        text
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} --version did not finish in {} s",
                exe.display(),
                PROBE_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = reader.join().unwrap_or_default();
    if !status.success() {
        bail!("{} --version failed ({})", exe.display(), status);
    }
    if !text
        .split(|c: char| c.is_whitespace())
        .any(|word| word.trim_start_matches('v') == version)
    {
        bail!(
            "{} --version printed {:?}, not version {}",
            exe.display(),
            printable(text.trim()),
            version
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OLD_VERSION: &str = "0.3.2";
    const NEW_VERSION: &str = "9.9.9";

    // ---- a release server on the loopback interface ----

    /// The checksums.txt a release carries.
    enum Checksums {
        /// The sha256sum lines of its assets.
        Honest,
        Text(String),
        /// The release has no such file.
        Absent,
    }

    struct Server {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
        routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl Server {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
            let (log, table) = (seen.clone(), routes.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (log, table) = (log.clone(), table.clone());
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => request.extend_from_slice(&buf[..n]),
                            }
                        }
                        let text = String::from_utf8_lossy(&request).to_string();
                        let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                        log.lock().unwrap().push(path.clone());
                        let found = table.lock().unwrap().get(&path).cloned();
                        let reply = match found {
                            Some(body) => {
                                let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                                r.extend_from_slice(&body);
                                r
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        };
                        let _ = stream.write_all(&reply).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Server { base, seen, routes }
        }

        fn requests(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn route(&self, path: &str, body: Vec<u8>) {
            self.routes.lock().unwrap().insert(path.to_string(), body);
        }

        /// Publish a release `tag` with these assets (name, bytes) and its checksums.txt.
        fn publish(&self, tag: &str, assets: &[(String, Vec<u8>)], checksums: Checksums) {
            let mut listed: Vec<serde_json::Value> = Vec::new();
            for (name, bytes) in assets {
                self.route(&format!("/dl/{name}"), bytes.clone());
                listed.push(serde_json::json!({"name": name, "browser_download_url": format!("{}/dl/{name}", self.base)}));
            }
            let text = match checksums {
                Checksums::Text(text) => Some(text),
                Checksums::Absent => None,
                Checksums::Honest => Some(
                    assets
                        .iter()
                        .map(|(n, b)| format!("{}  {n}\n", hex(&Sha256::digest(b))))
                        .collect::<String>(),
                ),
            };
            if let Some(text) = text {
                self.route("/dl/checksums.txt", text.into_bytes());
                listed.push(serde_json::json!({"name": "checksums.txt", "browser_download_url": format!("{}/dl/checksums.txt", self.base)}));
            }
            let json = serde_json::json!({"tag_name": tag, "assets": listed})
                .to_string()
                .into_bytes();
            self.route("/repos/oxidekit/oxidepm/releases/latest", json.clone());
            self.route(
                &format!("/repos/oxidekit/oxidepm/releases/tags/{tag}"),
                json,
            );
        }
    }

    /// A tar.gz holding these files, all executable.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &raw).unwrap();
        gz.finish().unwrap()
    }

    fn asset_name() -> String {
        format!("oxidepm-{}.tar.gz", current_target())
    }

    /// A stand-in for `oxidepm`: reports a version and says where it was run from.
    fn script(version: &str, marker: Option<&Path>) -> Vec<u8> {
        let mark = marker
            .map(|m| format!("echo \"$0\" >> '{}'\n", m.display()))
            .unwrap_or_default();
        format!("#!/bin/sh\n{mark}echo \"oxidepm {version}\"\n").into_bytes()
    }

    // ---- an install directory and the environment around it ----

    struct Fixture {
        dir: tempfile::TempDir,
        bin: PathBuf,
        socket: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = private_tempdir().unwrap();
            let bin = dir.path().join("bin");
            fs::create_dir(&bin).unwrap();
            for name in ["oxidepm", "oxidepmd"] {
                fs::write(bin.join(name), format!("old {name}")).unwrap();
                fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
            let socket = dir.path().join("daemon.sock");
            Fixture { dir, bin, socket }
        }

        fn env(&self, server: &Server) -> Env {
            Env {
                api: server.base.clone(),
                opts: FetchOpts {
                    allow_loopback_http: true,
                    ..FetchOpts::default()
                },
                current_exe: self.bin.join("oxidepm"),
                current_version: OLD_VERSION.to_string(),
                target: current_target(),
                client: IpcClient::new(self.socket.clone()),
                daemon_wait: Duration::from_secs(3),
            }
        }

        fn read(&self, name: &str) -> String {
            String::from_utf8_lossy(&fs::read(self.bin.join(name)).unwrap()).to_string()
        }

        /// Names in the install directory, sorted.
        fn listing(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.bin)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            names
        }
    }

    /// A release with `oxidepm` and `oxidepmd` of the new version in the right archive.
    fn standard_release(server: &Server, marker: Option<&Path>) {
        let cli = script(NEW_VERSION, marker);
        let tarball = archive(&[("oxidepm", &cli), ("oxidepmd", b"new oxidepmd")]);
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), tarball)],
            Checksums::Honest,
        );
    }

    /// A daemon of the first protocol on `path`: it refuses any other protocol number (and closes the
    /// connection, as that release did) and answers a protocol-1 `kill` with ok, then exits.
    /// `on_kill` runs when the kill arrives. Returns the (number, request) pairs it saw.
    type Seen = Arc<Mutex<Vec<(u64, String)>>>;
    fn old_daemon(
        path: PathBuf,
        on_kill: impl FnOnce() + Send + 'static,
    ) -> (std::thread::JoinHandle<()>, Seen) {
        use std::io::{BufRead, Write};
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let handle = std::thread::spawn(move || {
            let mut on_kill = Some(on_kill);
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut line = String::new();
                // a connection that only looks (a plan asks whether a daemon is there) sends nothing
                if std::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .unwrap_or(0)
                    == 0
                {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let number = value["protocol"].as_u64().unwrap();
                let kind = value["request"]["type"].as_str().unwrap().to_string();
                log.lock().unwrap().push((number, kind.clone()));
                let response = if number == 1 {
                    serde_json::json!({"type": "ok", "message": "old daemon stopping"})
                } else {
                    serde_json::json!({"type": "error", "code": "E_PROTOCOL_VERSION", "message": "protocol 1 only"})
                };
                let reply = serde_json::json!({"protocol": 1, "request_id": value["request_id"], "response": response});
                let mut stream = stream;
                let _ = writeln!(stream, "{reply}");
                if number == 1 && kind == "kill" {
                    if let Some(f) = on_kill.take() {
                        f();
                    }
                    return; // the listener goes away with the thread
                }
            }
        });
        (handle, seen)
    }

    /// A process this test started, killed and reaped when the guard goes away.
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // ---- the tests ----

    #[test]
    fn checksums_are_read_from_sha256sum_lines() {
        let a = hex(&Sha256::digest(b"a"));
        let b = hex(&Sha256::digest(b"b"));
        let text =
            format!("{a}  one.tar.gz\n{b} *two.tar.gz\n\nnot a line\n{a}  ./path/three.tar.gz\n");
        assert_eq!(hex(&checksum_for(&text, "one.tar.gz").unwrap()), a);
        assert_eq!(
            hex(&checksum_for(&text, "two.tar.gz").unwrap()),
            b,
            "binary-mode marker"
        );
        assert!(
            checksum_for(&text, "three.tar.gz").is_err(),
            "the name must match exactly"
        );
        assert!(checksum_for(&text, "one.tar").is_err());
        assert!(
            checksum_for(&format!("{}  one.tar.gz\n", "zz".repeat(32)), "one.tar.gz").is_err(),
            "not hex"
        );
        assert!(
            checksum_for(&format!("{a}  x\n{b}  x\n"), "x").is_err(),
            "two different digests for one name"
        );
    }

    #[tokio::test]
    async fn update_picks_the_exact_asset_name() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let target = current_target();
        let good = archive(&[
            ("oxidepm", &script(NEW_VERSION, None)),
            ("oxidepmd", b"good oxidepmd"),
        ]);
        let decoy = |tag: &str| {
            archive(&[
                ("oxidepm", &script(NEW_VERSION, None)),
                ("oxidepmd", format!("decoy {tag}").as_bytes()),
            ])
        };
        // names that contain the target and end in .tar.gz, listed before the real one
        let decoys: Vec<(String, Vec<u8>)> = vec![
            (
                format!("oxidepm-v{NEW_VERSION}-{target}.tar.gz"),
                decoy("versioned"),
            ),
            (format!("evil-oxidepm-{target}.tar.gz"), decoy("prefixed")),
            (format!("oxidepm-{target}-debug.tar.gz"), decoy("debug")),
        ];
        let mut assets = decoys.clone();
        assets.push((asset_name(), good));
        server.publish(&format!("v{NEW_VERSION}"), &assets, Checksums::Honest);

        execute_at(&fx.env(&server), None, false, false)
            .await
            .expect("the update succeeds");
        assert_eq!(fx.read("oxidepmd"), "good oxidepmd");
        let requests = server.requests();
        for (name, _) in &decoys {
            assert!(
                !requests.iter().any(|r| r.ends_with(name.as_str())),
                "{name} was downloaded: {requests:?}"
            );
        }

        // a release with only look-alikes has nothing to install
        let fx = Fixture::new();
        let server = Server::start().await;
        server.publish(&format!("v{NEW_VERSION}"), &decoys, Checksums::Honest);
        let err = execute_at(&fx.env(&server), None, false, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no asset named exactly"), "{err}");
        assert_eq!(fx.read("oxidepmd"), "old oxidepmd");
        assert!(
            !server.requests().iter().any(|r| r.starts_with("/dl/")),
            "{:?}",
            server.requests()
        );
    }

    #[tokio::test]
    async fn update_refuses_what_checksums_txt_does_not_vouch_for() {
        // 1. the archive is not what checksums.txt says
        let fx = Fixture::new();
        let server = Server::start().await;
        let hit = Arc::new(Mutex::new(false));
        let (daemon, kills) = old_daemon(fx.socket.clone(), {
            let hit = hit.clone();
            move || *hit.lock().unwrap() = true
        });
        let real = archive(&[("oxidepm", &script(NEW_VERSION, None))]);
        let lie = format!(
            "{}  {}\n",
            hex(&Sha256::digest(b"something else")),
            asset_name()
        );
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), real.clone())],
            Checksums::Text(lie),
        );
        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, false)
                .await
                .unwrap_err()
        );
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
        assert_eq!(
            fx.listing(),
            ["oxidepm", "oxidepmd"],
            "nothing was left behind"
        );
        assert!(
            kills.lock().unwrap().is_empty(),
            "the daemon was not touched: {:?}",
            kills.lock().unwrap()
        );
        assert!(!*hit.lock().unwrap());

        // 2. checksums.txt does not list the archive
        let server = Server::start().await;
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), real.clone())],
            Checksums::Text(format!("{}  other.tar.gz\n", hex(&Sha256::digest(&real)))),
        );
        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, false)
                .await
                .unwrap_err()
        );
        assert!(err.contains("does not list"), "{err}");

        // 3. the release has no checksums.txt at all
        let server = Server::start().await;
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), real)],
            Checksums::Absent,
        );
        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, false)
                .await
                .unwrap_err()
        );
        assert!(
            err.contains("no checksums.txt") || err.contains("checksums.txt"),
            "{err}"
        );
        assert_eq!(fx.listing(), ["oxidepm", "oxidepmd"]);
        // let the fake daemon end
        let _ = fx.env(&server).client.send(&Request::Kill).await;
        daemon.join().unwrap();
    }

    #[tokio::test]
    async fn update_refuses_unsafe_versions_and_downgrades() {
        let fx = Fixture::new();
        let server = Server::start().await;
        standard_release(&server, None);
        for bad in ["1/../../x", "1.0.0?x=1", "v", "1.0", "../../etc", "1.0.0\n"] {
            let err = execute_at(&fx.env(&server), Some(bad.to_string()), false, false)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not acceptable"), "{bad:?}: {err}");
        }
        // a lower version needs --force, and that is decided before anything is asked of the network
        let err = execute_at(&fx.env(&server), Some("0.1.0".to_string()), false, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(server.requests().is_empty(), "{:?}", server.requests());

        // the latest release being older than this build is a downgrade as well
        server.publish(
            "v0.1.0",
            &[(
                asset_name(),
                archive(&[("oxidepm", &script("0.1.0", None))]),
            )],
            Checksums::Honest,
        );
        let err = execute_at(&fx.env(&server), None, false, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(
            !server.requests().iter().any(|r| r.starts_with("/dl/")),
            "{:?}",
            server.requests()
        );
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
        // with --force it is installed
        execute_at(&fx.env(&server), None, true, false)
            .await
            .expect("forced");
        assert!(fx.read("oxidepm").contains("oxidepm 0.1.0"));
    }

    #[tokio::test]
    async fn update_can_install_an_explicit_release_candidate() {
        let candidate = "0.4.0-rc.1";
        let fx = Fixture::new();
        let server = Server::start().await;
        let cli = script(candidate, None);
        let tarball = archive(&[("oxidepm", &cli), ("oxidepmd", b"new oxidepmd")]);
        server.publish(
            &format!("v{candidate}"),
            &[(asset_name(), tarball)],
            Checksums::Honest,
        );
        let env = fx.env(&server);

        execute_at(&env, Some(format!("v{candidate}")), false, false)
            .await
            .unwrap();
        assert!(fx.read("oxidepm").contains(candidate));
        assert!(server
            .requests()
            .iter()
            .any(|path| path.ends_with("/releases/tags/v0.4.0-rc.1")));
    }

    #[tokio::test]
    async fn update_with_nothing_newer_downloads_nothing() {
        let fx = Fixture::new();
        let server = Server::start().await;
        server.publish(
            &format!("v{OLD_VERSION}"),
            &[(asset_name(), archive(&[("oxidepm", b"x")]))],
            Checksums::Honest,
        );
        execute_at(&fx.env(&server), None, false, false)
            .await
            .unwrap();
        assert!(
            !server.requests().iter().any(|r| r.starts_with("/dl/")),
            "{:?}",
            server.requests()
        );
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
    }

    #[tokio::test]
    async fn update_stages_all_binaries_before_renaming() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let cli = script(NEW_VERSION, None);
        let tarball = archive(&[
            ("oxidepm", &cli),
            ("oxidepmd", b"new oxidepmd"),
            ("oxidepm-sandbox", b"new sandbox"),
        ]);
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), tarball)],
            Checksums::Honest,
        );
        // staging the third file cannot work: its staging name is a directory
        fs::create_dir(fx.bin.join("oxidepm-sandbox.new")).unwrap();
        let hit = Arc::new(Mutex::new(false));
        let (daemon, kills) = old_daemon(fx.socket.clone(), {
            let hit = hit.clone();
            move || *hit.lock().unwrap() = true
        });

        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, false)
                .await
                .unwrap_err()
        );
        assert!(err.contains("Could not stage"), "{err}");
        // nothing was replaced, nothing was backed up, no staged copy was left, the daemon was not stopped
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
        assert_eq!(fx.read("oxidepmd"), "old oxidepmd");
        assert_eq!(fx.listing(), ["oxidepm", "oxidepm-sandbox.new", "oxidepmd"]);
        assert!(kills.lock().unwrap().is_empty());

        // without the obstacle all three are installed
        fs::remove_dir(fx.bin.join("oxidepm-sandbox.new")).unwrap();
        execute_at(&fx.env(&server), None, false, false)
            .await
            .expect("update");
        assert_eq!(fx.read("oxidepmd"), "new oxidepmd");
        assert_eq!(fx.read("oxidepm-sandbox"), "new sandbox");
        assert!(fx
            .read("oxidepm")
            .contains(&format!("oxidepm {NEW_VERSION}")));
        daemon.join().unwrap();
    }

    #[tokio::test]
    async fn update_restores_the_files_when_a_later_rename_fails() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let tarball = archive(&[
            ("oxidepm", &script(NEW_VERSION, None)),
            ("oxidepmd", b"new oxidepmd"),
            ("oxidepm-sandbox", b"new sandbox"),
        ]);
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), tarball)],
            Checksums::Honest,
        );
        // the third rename cannot work: the destination is a directory that is not empty
        fs::create_dir(fx.bin.join("oxidepm-sandbox")).unwrap();
        fs::write(fx.bin.join("oxidepm-sandbox/keep"), b"x").unwrap();

        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, false)
                .await
                .unwrap_err()
        );
        assert!(
            err.contains("Could not install") && err.contains("put back"),
            "{err}"
        );
        assert_eq!(
            fx.read("oxidepm"),
            "old oxidepm",
            "the first file was restored"
        );
        assert_eq!(
            fx.read("oxidepmd"),
            "old oxidepmd",
            "the second file was restored"
        );
        assert!(fx.bin.join("oxidepm-sandbox/keep").exists());
        assert_eq!(
            fx.listing(),
            ["oxidepm", "oxidepm-sandbox", "oxidepmd"],
            "no .new and no .pre-update is left"
        );
        let mode = fs::metadata(fx.bin.join("oxidepm"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[tokio::test]
    async fn update_keeps_the_bundle_and_binaries_in_one_transaction() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let bundle = fx.dir.path().join("share/oxidepm/adapters");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("old.tar.gz"), b"old archive").unwrap();

        let marker = fx.dir.path().join("bundle-probed");
        let failing = format!(
            "#!/bin/sh\ncat '{}'/SHA256SUMS > '{}'\nexit 3\n",
            bundle.display(),
            marker.display()
        );
        let publish = |cli: &[u8]| {
            let tarball = archive(&[
                ("oxidepm", cli),
                ("oxidepmd", b"new oxidepmd"),
                ("share/oxidepm/adapters/SHA256SUMS", b"new checksums"),
                ("share/oxidepm/adapters/new.tar.gz", b"new archive"),
            ]);
            server.publish(
                &format!("v{NEW_VERSION}"),
                &[(asset_name(), tarball)],
                Checksums::Honest,
            );
        };
        publish(failing.as_bytes());
        assert!(execute_at(&fx.env(&server), None, false, false)
            .await
            .is_err());
        assert_eq!(
            fs::read(&marker).unwrap(),
            b"new checksums",
            "the probe sees the complete new installation"
        );
        assert_eq!(fs::read(bundle.join("old.tar.gz")).unwrap(), b"old archive");
        assert!(!bundle.join("new.tar.gz").exists());
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
        assert_eq!(fx.read("oxidepmd"), "old oxidepmd");

        publish(&script(NEW_VERSION, None));
        execute_at(&fx.env(&server), None, false, false)
            .await
            .unwrap();
        assert_eq!(
            fs::read(bundle.join("SHA256SUMS")).unwrap(),
            b"new checksums"
        );
        assert_eq!(fs::read(bundle.join("new.tar.gz")).unwrap(), b"new archive");
        assert!(!bundle.join("old.tar.gz").exists());
        assert_eq!(fx.read("oxidepmd"), "new oxidepmd");

        publish(failing.as_bytes());
        assert!(execute_at(&fx.env(&server), None, false, false)
            .await
            .is_err());
        assert_eq!(fx.read("oxidepm.pre-update"), "old oxidepm");
        assert_eq!(
            fs::read(backup_path(&bundle).join("old.tar.gz")).unwrap(),
            b"old archive"
        );
        assert_eq!(fs::read(bundle.join("new.tar.gz")).unwrap(), b"new archive");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn update_holds_the_installation_lock_through_the_version_probe() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let marker = fx.dir.path().join("probe.lock");
        let cli = format!(
            "#!/bin/sh\nflock -n '{}' -c 'echo acquired' > '{}'\necho 'oxidepm {NEW_VERSION}'\n",
            fx.bin.display(),
            marker.display()
        );
        let tarball = archive(&[("oxidepm", cli.as_bytes())]);
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), tarball)],
            Checksums::Honest,
        );
        execute_at(&fx.env(&server), None, false, false)
            .await
            .unwrap();
        assert_eq!(
            fs::read(&marker).unwrap(),
            b"",
            "another process cannot acquire the installation lock during the probe"
        );

        let dir = fs::File::open(&fx.bin).unwrap();
        use std::os::fd::AsRawFd;
        // The updater's lock must have been released.
        assert_eq!(
            unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[tokio::test]
    async fn update_runs_version_probe_after_install() {
        // the new file is the one that is run, and it is run with --version
        let fx = Fixture::new();
        let server = Server::start().await;
        let marker = fx.dir.path().join("probe.log");
        standard_release(&server, Some(&marker));
        execute_at(&fx.env(&server), None, false, false)
            .await
            .expect("update");
        let ran = fs::read_to_string(&marker).unwrap();
        assert_eq!(
            ran.lines().collect::<Vec<_>>(),
            [fx.bin.join("oxidepm").to_str().unwrap()],
            "{ran:?}"
        );
        assert!(
            fx.bin.join("oxidepm.pre-update").exists(),
            "the old file is kept"
        );
        assert_eq!(
            fs::read_to_string(fx.bin.join("oxidepm.pre-update")).unwrap(),
            "old oxidepm"
        );

        // a new oxidepm that fails, and one that reports another version, are both taken back
        for (label, body) in [
            ("fails", b"#!/bin/sh\nexit 3\n".to_vec()),
            ("lies", script("1.2.3", None)),
            ("not-a-program", b"not executable at all".to_vec()),
        ] {
            let fx = Fixture::new();
            let server = Server::start().await;
            let tarball = archive(&[("oxidepm", &body), ("oxidepmd", b"new oxidepmd")]);
            server.publish(
                &format!("v{NEW_VERSION}"),
                &[(asset_name(), tarball)],
                Checksums::Honest,
            );
            let err = format!(
                "{:#}",
                execute_at(&fx.env(&server), None, false, false)
                    .await
                    .unwrap_err()
            );
            assert!(
                err.contains("does not work") && err.contains("put back"),
                "{label}: {err}"
            );
            assert_eq!(fx.read("oxidepm"), "old oxidepm", "{label}");
            assert_eq!(
                fx.read("oxidepmd"),
                "old oxidepmd",
                "{label}: every file is restored, not only the one probed"
            );
            assert_eq!(fx.listing(), ["oxidepm", "oxidepmd"], "{label}");
        }
    }

    #[tokio::test]
    async fn update_stops_an_old_daemon_first() {
        let fx = Fixture::new();
        let server = Server::start().await;
        standard_release(&server, None);
        // what is installed at the moment the old daemon gets its kill
        let at_kill: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (daemon, seen) = old_daemon(fx.socket.clone(), {
            let (bin, at_kill) = (fx.bin.clone(), at_kill.clone());
            move || {
                for name in ["oxidepm", "oxidepmd"] {
                    at_kill.lock().unwrap().push(
                        String::from_utf8_lossy(&fs::read(bin.join(name)).unwrap()).to_string(),
                    );
                }
            }
        });

        execute_at(&fx.env(&server), None, false, false)
            .await
            .expect("update");
        daemon.join().unwrap();
        // this build's protocol first, refused, then the old one: the kill crossed versions
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(2, "kill".to_string()), (1, "kill".to_string())]
        );
        assert_eq!(
            *at_kill.lock().unwrap(),
            ["old oxidepm", "old oxidepmd"],
            "the daemon was stopped before any file was replaced"
        );
        assert_eq!(fx.read("oxidepmd"), "new oxidepmd");
        assert!(fx.read("oxidepm").contains(NEW_VERSION));
    }

    #[tokio::test]
    async fn update_does_not_replace_files_when_the_daemon_will_not_stop() {
        let fx = Fixture::new();
        let server = Server::start().await;
        standard_release(&server, None);
        // a daemon that answers the kill and stays: the listener keeps accepting
        let listener = std::os::unix::net::UnixListener::bind(&fx.socket).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        listener.set_nonblocking(true).unwrap();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                use std::io::{BufRead, Write};
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    if std::io::BufReader::new(&stream)
                        .read_line(&mut line)
                        .unwrap_or(0)
                        == 0
                    {
                        continue;
                    }
                    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                    let reply = serde_json::json!({"protocol": value["protocol"], "request_id": value["request_id"],
                        "response": {"type": "ok", "message": "stopping (not really)"}});
                    let mut stream = stream;
                    let _ = writeln!(stream, "{reply}");
                }
            })
        };
        let mut env = fx.env(&server);
        env.daemon_wait = Duration::from_millis(600);
        let err = execute_at(&env, None, false, false)
            .await
            .unwrap_err()
            .to_string();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
        assert!(
            err.contains("did not stop") && err.contains("Nothing was replaced"),
            "{err}"
        );
        assert_eq!(fx.read("oxidepm"), "old oxidepm");
        assert_eq!(fx.read("oxidepmd"), "old oxidepmd");
        assert_eq!(fx.listing(), ["oxidepm", "oxidepmd"]);
    }

    /// the running executable is replaced, and the process keeps running.
    #[tokio::test]
    async fn update_replaces_the_binary_it_runs() {
        use std::io::{BufRead, BufReader, Write};

        let fx = Fixture::new();
        fs::remove_file(fx.bin.join("oxidepm")).unwrap();
        fs::copy("/bin/bash", fx.bin.join("oxidepm")).unwrap();
        fs::set_permissions(fx.bin.join("oxidepm"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut running = Guard(
            Command::new(fx.bin.join("oxidepm"))
                .args(["-c", "printf 'ready\\n'; IFS= read -r line"])
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(running.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n", "the old executable started");

        let server = Server::start().await;
        standard_release(&server, None);
        execute_at(&fx.env(&server), None, false, false)
            .await
            .expect("the update of a running binary succeeds");
        assert!(fx
            .read("oxidepm")
            .contains(&format!("oxidepm {NEW_VERSION}")));
        assert!(
            running.0.try_wait().unwrap().is_none(),
            "the old process keeps running"
        );
        writeln!(running.0.stdin.take().unwrap(), "continue").unwrap();
        assert!(running.0.wait().unwrap().success());
    }

    /// `update --dry-run` downloads the archive and checks it, unpacks it in a private directory
    /// that is deleted again, and installs nothing.
    #[tokio::test]
    async fn update_dry_run_verifies_and_installs_nothing() {
        let fx = Fixture::new();
        let server = Server::start().await;
        standard_release(&server, None);
        let before = fx.listing();
        let stamp = |name: &str| fs::metadata(fx.bin.join(name)).unwrap().modified().unwrap();
        let (cli_time, daemon_time) = (stamp("oxidepm"), stamp("oxidepmd"));
        let (daemon, seen) = old_daemon(fx.socket.clone(), || {});

        for version in [None, Some(NEW_VERSION.to_string())] {
            execute_at(&fx.env(&server), version, false, true)
                .await
                .expect("the rehearsal succeeds");
            let requests = server.requests();
            assert!(
                requests.iter().any(|r| r.ends_with(&asset_name())),
                "the archive is downloaded to be checked: {requests:?}"
            );
            assert!(requests.iter().any(|r| r.ends_with("checksums.txt")));
            assert_eq!(fx.listing(), before, "nothing was added or left");
            assert_eq!(
                (stamp("oxidepm"), stamp("oxidepmd")),
                (cli_time, daemon_time),
                "nothing was replaced"
            );
            assert_eq!(fx.read("oxidepm"), "old oxidepm");
        }
        assert!(
            seen.lock().unwrap().is_empty()
                || seen.lock().unwrap().iter().all(|(_, k)| k != "kill"),
            "the daemon was not stopped"
        );
        let _ = fx.env(&server).client.send(&Request::Kill).await;
        daemon.join().unwrap();

        // the rehearsal verifies: an archive that does not match checksums.txt fails it
        let server = Server::start().await;
        let lie = format!("{}  {}\n", hex(&Sha256::digest(b"other")), asset_name());
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), archive(&[("oxidepm", b"x")]))],
            Checksums::Text(lie),
        );
        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, true)
                .await
                .unwrap_err()
        );
        assert!(err.contains("sha256 mismatch"), "{err}");
        // and so does one the extractor refuses
        let server = Server::start().await;
        let evil = {
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            tar.append_link(&mut header, "oxidepm", "/etc/passwd")
                .unwrap();
            let raw = tar.into_inner().unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut gz, &raw).unwrap();
            gz.finish().unwrap()
        };
        server.publish(
            &format!("v{NEW_VERSION}"),
            &[(asset_name(), evil)],
            Checksums::Honest,
        );
        let err = format!(
            "{:#}",
            execute_at(&fx.env(&server), None, false, true)
                .await
                .unwrap_err()
        );
        assert!(
            err.contains("refused") && err.contains("symbolic link"),
            "{err}"
        );
        assert_eq!(fx.listing(), before);
    }

    #[tokio::test]
    async fn the_plan_names_the_archive_and_the_files_it_would_replace() {
        let fx = Fixture::new();
        let server = Server::start().await;
        let env = fx.env(&server);
        let release = Release {
            version: NEW_VERSION.to_string(),
            asset_name: asset_name(),
            asset_url: "https://example.invalid/a.tar.gz".to_string(),
            checksums_url: String::new(),
        };
        let verified = oxidepm_fetch::Verified {
            sha256: [7; 32],
            size: 123,
        };
        let files = vec![
            ("oxidepm", PathBuf::from("/x/oxidepm")),
            ("oxidepmd", PathBuf::from("/x/oxidepmd")),
        ];
        let plan = update_plan(
            &env,
            &release,
            &verified,
            &files,
            Path::new("/opt/bin"),
            true,
        );
        assert_eq!(plan.op, "update");
        let actions: Vec<_> = plan.items.iter().map(|i| i.action.as_str()).collect();
        assert_eq!(actions, ["verified", "stop", "replace", "replace"]);
        assert!(plan.items[2].name.ends_with("/opt/bin/oxidepm"));
        assert!(plan.items[2]
            .detail
            .as_deref()
            .unwrap()
            .contains("-> v9.9.9"));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("nothing was installed")));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("authenticity is not")));
    }
}
