//! Administering adapters in the daemon: install what the CLI staged, enable
//! with a grant, disable, remove, list and describe, and keep the index sequence, the cached index
//! and the revocations.
//!
//! The network, the archive and the download run in the CLI; the daemon is handed a staged
//! directory and the bytes of the index the CLI used, and checks again what it can: the index
//! signature with the embedded keys, the executable's sha256 against the signed one, the manifest.
//! One operation at a time (`AdapterHost::admin`). Nothing here starts a process except `enable`
//! (the adapter) and the self-test (the launcher).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{SecondsFormat, Utc};
use nix::errno::Errno;
use nix::fcntl::{openat, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{geteuid, unlinkat, UnlinkatFlags};
use oxidepm_adapter_proto::manifest::Rule;
use oxidepm_adapter_proto::messages::method;
use oxidepm_adapter_proto::permissions::{HostLevel, HostScope};
use oxidepm_adapter_proto::signing::is_official_release_trust;
use oxidepm_adapter_proto::{
    host_target, Index, Invalid, Kind, Manifest, Permissions, ValidateCtx, VerifiedIndex,
    INDEX_MAX_BYTES,
};
use oxidepm_core::constants::ADAPTERS_DIR;
use oxidepm_core::release_bundle::{self, ReleaseBundle};
use oxidepm_core::{runtime_hints, Error, Paths, Result};
use oxidepm_db::AdapterRow;
use oxidepm_ipc::{
    AdapterInfo, AdapterReply, AdapterRequest, EnableArgs, InstallArgs, PlannedItem,
};
use oxidepm_sandbox::{enforce_name, EnforcedRecord, Profile, ProfileInput};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::warn;

use super::entry::{AdapterEntry, EntryState};
use super::host::{AdapterHost, HostCtx};
use super::sandbox::{selftest, SelftestEnv};
use super::store::AdapterStore;
use crate::audit::AdapterLine;

/// A staging directory older than this was abandoned (the CLI deletes its own at the end).
pub const STAGING_MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// Services enabled at once.
pub const MAX_SERVICES: usize = 8;
/// Memory the enabled adapters may ask for together, in MiB.
pub const MAX_TOTAL_MEMORY_MB: u32 = 1024;
/// The most entries a staged tree may hold (the extractor allows 1000).
const MAX_TREE_ENTRIES: usize = 1000;
const MAX_STAGED_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STAGED_BYTES: u64 = 256 * 1024 * 1024;
/// The path searched for the programs a `spawn = named` adapter may run.
const ADAPTER_PATH: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

// ---- housekeeping ----------------------------------------------------------------------------

fn open_relative(parent: Option<&File>, path: &Path, flags: OFlag) -> nix::Result<File> {
    let fd = openat(
        parent.map(AsRawFd::as_raw_fd),
        path,
        flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    // SAFETY: openat returned a new descriptor owned by this caller.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_cleanup_dir(parent: Option<&File>, path: &Path) -> io::Result<File> {
    let dir = open_relative(parent, path, OFlag::O_DIRECTORY)?;
    let metadata = dir.metadata()?;
    if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be private to the current user", path.display()),
        ));
    }
    Ok(dir)
}

fn open_adapter_cleanup_root(paths: &Paths) -> io::Result<File> {
    // Ignore a trailing slash so O_NOFOLLOW also applies to the last home component.
    let home_path: PathBuf = paths.home().components().collect();
    let home = open_cleanup_dir(None, &home_path)?;
    open_cleanup_dir(Some(&home), Path::new(ADAPTERS_DIR))
}

fn directory_fd_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn cleanup_entries(parent: &File, name: &Path) -> io::Result<(File, std::fs::ReadDir)> {
    let dir = open_cleanup_dir(Some(parent), name)?;
    let entries = std::fs::read_dir(directory_fd_path(&dir))?;
    Ok((dir, entries))
}

/// Delete what an import left behind in `adapters/staging` more than [`STAGING_MAX_AGE`] ago. A
/// fresh entry is another import in progress and stays.
pub fn clean_staging(paths: &Paths) {
    let Ok(root) = open_adapter_cleanup_root(paths) else {
        return;
    };
    let Ok((dir, entries)) = cleanup_entries(&root, Path::new("staging")) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if age >= STAGING_MAX_AGE {
            let name = entry.file_name();
            if let Err(e) = remove_any_at(&dir, Path::new(&name)) {
                warn!(
                    "Could not delete the old staging entry {}: {}",
                    paths.adapter_staging().join(name).display(),
                    e
                );
            }
        }
    }
}

/// Delete the directories under `installed/` that no row refers to (a crash between two steps of
/// an install or an update leaves them) and everything in `trash/`. A version that a row names
/// as current or as its rollback step stays.
pub fn sweep_installed(paths: &Paths, rows: &[AdapterRow]) {
    let Ok(adapters) = open_adapter_cleanup_root(paths) else {
        return;
    };
    if let Ok((root, names)) = cleanup_entries(&adapters, Path::new("installed")) {
        for entry in names.flatten() {
            let name = entry.file_name();
            let Some(text) = name.to_str() else {
                continue;
            };
            let path = paths.adapter_installed_root().join(&name);
            let row = rows.iter().find(|r| r.name == text);
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let Some(row) = row.filter(|_| is_dir) else {
                if let Err(e) = remove_any_at(&root, Path::new(&name)) {
                    warn!(
                        "Could not delete the unreferenced {}: {}",
                        path.display(),
                        e
                    );
                }
                continue;
            };
            let Ok((dir, versions)) = cleanup_entries(&root, Path::new(&name)) else {
                continue;
            };
            for entry in versions.flatten() {
                let version = entry.file_name();
                let keep = version
                    .to_str()
                    .is_some_and(|v| v == row.version || row.prev_version.as_deref() == Some(v));
                if !keep {
                    if let Err(e) = remove_any_at(&dir, Path::new(&version)) {
                        warn!(
                            "Could not delete the unreferenced {}: {}",
                            path.join(version).display(),
                            e
                        );
                    }
                }
            }
        }
    }
    if let Ok((dir, entries)) = cleanup_entries(&adapters, Path::new("trash")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Err(e) = remove_any_at(&dir, Path::new(&name)) {
                warn!(
                    "Could not empty the trash entry {}: {}",
                    paths.adapter_trash().join(name).display(),
                    e
                );
            }
        }
    }
}

/// Delete a file, a link or a directory tree, and check that it is gone.
pub(super) fn remove_any(path: &Path) -> std::io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing name for adapter cleanup",
        )
    })?;
    let parent_path = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = match open_cleanup_dir(None, parent_path) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    remove_any_at(&parent, Path::new(name))
}

fn remove_any_at(parent: &File, name: &Path) -> io::Result<()> {
    let entry = match open_relative(Some(parent), name, OFlag::O_PATH) {
        Ok(entry) => entry,
        Err(Errno::ENOENT) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let path = directory_fd_path(parent).join(name);
    if entry.metadata()?.is_dir() {
        // Change permissions on the opened directory. On Linux remove_dir_all walks from the
        // parent descriptor and does not follow links in the tree.
        let _ = std::fs::set_permissions(
            directory_fd_path(&entry),
            std::fs::Permissions::from_mode(0o700),
        );
        std::fs::remove_dir_all(&path)?;
    } else {
        match unlinkat(Some(parent.as_raw_fd()), name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is still there after it was deleted",
            name.display()
        ))),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod housekeeping_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn boot_cleanup_leaves_link_targets_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(temp.path().join("home"));
        paths.ensure_dirs().unwrap();
        fs::create_dir(paths.adapters_dir()).unwrap();

        let sentinels = [
            paths.adapter_staging(),
            paths.adapter_installed_root(),
            paths.adapter_trash(),
        ]
        .map(|link| {
            let target = temp.path().join(link.file_name().unwrap());
            fs::create_dir(&target).unwrap();
            let sentinel = target.join("sentinel");
            fs::write(&sentinel, "keep").unwrap();
            File::open(&sentinel)
                .unwrap()
                .set_modified(SystemTime::now() - STAGING_MAX_AGE - Duration::from_secs(1))
                .unwrap();
            symlink(&target, link).unwrap();
            sentinel
        });

        clean_staging(&paths);
        sweep_installed(&paths, &[]);

        for sentinel in sentinels {
            assert_eq!(
                fs::read_to_string(&sentinel).as_deref().ok(),
                Some("keep"),
                "{}",
                sentinel.display()
            );
        }
    }
}

// ---- small helpers ---------------------------------------------------------------------------

pub(super) fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = open_regular(path)?.take(MAX_STAGED_FILE_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    if file.limit() == 0 {
        return Err(std::io::Error::other(
            "a staged file exceeds its size limit",
        ));
    }
    Ok(hex(&hasher.finalize()))
}

/// `value` as one plain path component: not empty, not `.` or `..`, no separator or NUL.
fn plain_component(value: &str) -> Result<&str> {
    let ok = !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && !value.contains(['/', '\\', '\0'])
        && !value.chars().any(char::is_control);
    if ok {
        Ok(value)
    } else {
        Err(Error::AdapterInvalid(format!(
            "{value:?} is not a staging directory name"
        )))
    }
}

/// An index rule that was broken, as the error it is.
pub(super) fn index_error(e: Invalid) -> Error {
    match e.rule {
        Rule::IndexStale => Error::IndexStale(e.detail),
        Rule::Signature => Error::SignatureInvalid(e.detail),
        _ => Error::AdapterInvalid(e.detail),
    }
}

fn manifest_error(e: Invalid) -> Error {
    Error::AdapterInvalid(e.detail)
}

pub(super) fn io_error(what: &str, e: std::io::Error) -> Error {
    Error::Internal(format!("{what}: {e}"))
}

fn daemon_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

/// Write `bytes` to `path` through a temporary file, mode 0600, so a reader sees all or nothing.
pub(super) fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

pub(super) fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut file = open_regular(path).ok()?.take(max + 1);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    (file.limit() > 0).then_some(bytes)
}

fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    use nix::fcntl::OFlag;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("expected a regular file"));
    }
    Ok(file)
}

fn installed_release_bundle() -> Result<(ReleaseBundle, Index)> {
    let exe = std::env::current_exe().map_err(|e| io_error("finding the daemon executable", e))?;
    let dir =
        release_bundle::directory(&exe).map_err(|e| io_error("finding the release bundle", e))?;
    #[cfg(feature = "test-hooks")]
    let dir = std::env::var_os("OXIDEPM_TEST_BUNDLE_DIR").map_or(dir, PathBuf::from);
    let bundle = ReleaseBundle::read(dir).map_err(|e| {
        Error::AdapterInvalid(format!("could not read the release adapter bundle: {e}"))
    })?;
    let mut bytes = Vec::new();
    bundle
        .copy_verified("index.json", INDEX_MAX_BYTES as u64, &mut bytes)
        .map_err(|e| Error::ChecksumMismatch(e.to_string()))?;
    let index = Index::parse(&bytes).map_err(index_error)?;
    Ok((bundle, index))
}

fn verify_auxiliary_files(
    root: &Path,
    files: &[PathBuf],
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    if files.len() != expected.len() {
        return Err(Error::ChecksumMismatch(
            "the staged files do not match the published artifact".into(),
        ));
    }
    for path in files {
        let relative = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .unwrap_or_default();
        let Some(digest) = expected.get(relative) else {
            return Err(Error::ChecksumMismatch(format!(
                "no published digest for staged file {}",
                path.display()
            )));
        };
        let actual = sha256_file(path).map_err(|e| io_error("hashing a staged file", e))?;
        if !bool::from(
            actual
                .as_bytes()
                .ct_eq(digest.to_ascii_lowercase().as_bytes()),
        ) {
            return Err(Error::ChecksumMismatch(format!(
                "the staged file {relative:?} does not match its published digest"
            )));
        }
    }
    Ok(())
}

/// What a staged tree holds.
struct Inspected {
    manifest: Manifest,
    manifest_bytes: Vec<u8>,
    exe_sha256: String,
    auxiliary_files: Vec<PathBuf>,
}

/// Walk a staged tree: only directories and regular files (no link, no device), a bounded number
/// of entries, nothing writable by anyone else. Then make the modes what the install wants:
/// directories 0700, the executable 0500, every other file 0400.
fn inspect_tree(dir: &Path, exec: &str) -> Result<Vec<PathBuf>> {
    let exec_path = dir.join(exec);
    let manifest_path = dir.join("manifest.json");
    let mut count = 0usize;
    let mut total = 0u64;
    let mut auxiliary_files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| io_error(&format!("reading {}", current.display()), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_error("reading the staged directory", e))?;
            count += 1;
            if count > MAX_TREE_ENTRIES {
                return Err(Error::AdapterInvalid(format!(
                    "the staged tree has more than {MAX_TREE_ENTRIES} entries"
                )));
            }
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| io_error("reading the staged tree", e))?;
            if meta.file_type().is_symlink() || !(meta.is_dir() || meta.is_file()) {
                return Err(Error::AdapterInvalid(format!(
                    "{} is not a regular file or a directory",
                    path.display()
                )));
            }
            if meta.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
                if meta.nlink() != 1
                    || meta.len() > MAX_STAGED_FILE_BYTES
                    || total > MAX_STAGED_BYTES
                {
                    return Err(Error::AdapterInvalid(format!(
                        "{} is linked or exceeds the staged size limits",
                        path.display()
                    )));
                }
                if path != exec_path && path != manifest_path {
                    auxiliary_files.push(path);
                }
            }
        }
    }
    // modes, deepest first so a directory is changed after what is in it
    let mut all = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        all.push(current.clone());
        if let Ok(entries) = std::fs::read_dir(&current) {
            for entry in entries.flatten() {
                let path = entry.path();
                if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                    stack.push(path);
                } else {
                    let mode = if path == exec_path { 0o500 } else { 0o400 };
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                        .map_err(|e| io_error("setting the modes of the staged tree", e))?;
                }
            }
        }
    }
    for path in all.iter().rev() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_error("setting the modes of the staged tree", e))?;
    }
    Ok(auxiliary_files)
}

/// Read what is in a staged tree: the manifest, the executable and its digest.
fn inspect(dir: &Path) -> Result<Inspected> {
    let manifest_path = dir.join("manifest.json");
    let meta = std::fs::symlink_metadata(&manifest_path).map_err(|_| {
        Error::AdapterInvalid("the staged adapter has no manifest.json".to_string())
    })?;
    if !meta.is_file() {
        return Err(Error::AdapterInvalid(
            "manifest.json is not a regular file".to_string(),
        ));
    }
    let manifest_bytes = read_capped(
        &manifest_path,
        oxidepm_adapter_proto::MANIFEST_MAX_BYTES as u64,
    )
    .ok_or_else(|| {
        Error::AdapterInvalid(format!(
            "manifest.json is larger than {} bytes",
            oxidepm_adapter_proto::MANIFEST_MAX_BYTES
        ))
    })?;
    let manifest = Manifest::parse(&manifest_bytes).map_err(manifest_error)?;
    manifest.check_exec_file(dir).map_err(manifest_error)?;
    let auxiliary_files = inspect_tree(dir, &manifest.exec)?;
    let exe_sha256 = sha256_file(&dir.join(&manifest.exec))
        .map_err(|e| io_error("reading the executable", e))?;
    Ok(Inspected {
        manifest,
        manifest_bytes,
        exe_sha256,
        auxiliary_files,
    })
}

/// A directory that is deleted again unless it was committed.
struct Pending {
    path: PathBuf,
    keep: bool,
}

impl Pending {
    fn new(path: PathBuf) -> Self {
        Pending { path, keep: false }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.keep {
            if let Err(e) = remove_any(&self.path) {
                warn!("Could not delete {}: {}", self.path.display(), e);
            }
        }
    }
}

/// How a trust string reads in a message.
fn describe_trust(trust: &str) -> String {
    match trust.strip_prefix("signed:") {
        Some(key) => format!("the index signed by key {key}"),
        None if trust == "bundled" => "the installed release bundle".to_string(),
        None => "a source you pinned by hand".to_string(),
    }
}

fn level_of(text: &str) -> Result<HostLevel> {
    match text {
        "read" => Ok(HostLevel::Read),
        "lifecycle" => Ok(HostLevel::Lifecycle),
        "admin" => Ok(HostLevel::Admin),
        other => Err(Error::AdapterInvalid(format!(
            "--level takes read, lifecycle or admin, not '{other}'"
        ))),
    }
}

fn level_text(level: HostLevel) -> &'static str {
    crate::policy::level_name(level)
}

fn scope_text(scope: HostScope) -> &'static str {
    match scope {
        HostScope::All => "all",
        HostScope::OptedIn => "opted-in",
    }
}

fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::Service => "service",
        Kind::Tool => "tool",
    }
}

fn check_enabled_set(rows: &[AdapterRow], manifest: &Manifest, grant: &Permissions) -> Result<()> {
    let others = || rows.iter().filter(|r| r.enabled && r.name != manifest.name);
    if manifest.kind == Kind::Service
        && others().filter(|r| r.kind == "service").count() >= MAX_SERVICES
    {
        return Err(Error::AdapterBusy(format!(
            "{MAX_SERVICES} services are enabled already; disable one first"
        )));
    }
    let mut memory = u64::from(grant.memory_mb);
    for row in others() {
        let permissions: Permissions =
            serde_json::from_str(row.grant_json.as_deref().unwrap_or("")).map_err(|e| {
                Error::AdapterInvalid(format!(
                    "the grant of enabled adapter '{}' is invalid: {e}",
                    row.name
                ))
            })?;
        memory += u64::from(permissions.memory_mb);
        if manifest.provides.runtimes.is_empty() {
            continue;
        }
        let other = Manifest::parse(row.manifest_json.as_bytes()).map_err(manifest_error)?;
        if let Some(runtime) = manifest
            .provides
            .runtimes
            .iter()
            .find(|r| other.provides.runtimes.contains(r))
        {
            return Err(Error::AdapterInvalid(format!(
                "adapter '{}' already provides the runtime '{runtime}'; disable it first: oxidepm adapter disable {}",
                row.name, row.name
            )));
        }
    }
    if memory > u64::from(MAX_TOTAL_MEMORY_MB) {
        return Err(Error::AdapterBusy(format!(
            "the enabled adapters would ask for {memory} MiB of memory together; the limit is {MAX_TOTAL_MEMORY_MB} MiB"
        )));
    }
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PreviousRelease {
    source: String,
    trust: String,
    grant: Option<String>,
    enforcement: Option<String>,
}

fn remember_previous(row: &mut AdapterRow, previous: &AdapterRow) -> Result<()> {
    let metadata = PreviousRelease {
        source: previous.source.clone(),
        trust: previous.trust.clone(),
        grant: previous.grant_json.clone(),
        enforcement: previous.enforced_json.clone(),
    };
    row.prev_version = Some(previous.version.clone());
    row.prev_exe_sha256 = Some(previous.exe_sha256.clone());
    row.prev_manifest_json = Some(previous.manifest_json.clone());
    row.prev_grant_json =
        Some(serde_json::to_string(&metadata).map_err(|e| Error::Internal(e.to_string()))?);
    Ok(())
}

// ---- the host's administrative side ----------------------------------------------------------

impl AdapterHost {
    pub(super) fn ctx(&self) -> Result<&HostCtx> {
        self.ctx
            .get()
            .ok_or_else(|| Error::Internal("the adapter host is not loaded".to_string()))
    }

    /// The open `adapters.db`, made (with the private directories) if this is the first adapter.
    pub(super) async fn store_or_create(&self) -> Result<Arc<AdapterStore>> {
        if let Some(store) = self.store.lock().clone() {
            return Ok(store);
        }
        let paths = Arc::clone(&self.ctx()?.paths);
        let store = Arc::new(AdapterStore::open_or_create(&paths).await?);
        *self.store.lock() = Some(Arc::clone(&store));
        Ok(store)
    }

    pub(super) fn audit_event(
        &self,
        name: &str,
        event: &str,
        version: Option<&str>,
        exe_sha256: Option<&str>,
    ) {
        if let Some(audit) = self.ctx.get().and_then(|c| c.audit.as_ref()) {
            audit.adapter_line(AdapterLine {
                name: name.to_string(),
                event: event.to_string(),
                version: version.map(str::to_string),
                sha12: exe_sha256.map(|s| s.chars().take(12).collect()),
            });
        }
    }

    /// Make the entry of `row` and put it in the registry, replacing the one there. The entry of a
    /// row that is not enabled is `Disabled`; the caller starts an enabled one.
    pub(super) fn register(
        &self,
        row: &AdapterRow,
        store: &Arc<AdapterStore>,
    ) -> Result<Arc<AdapterEntry>> {
        let ctx = self.ctx()?;
        let pids = Arc::clone(
            self.pids
                .get_or_init(|| crate::supervisor::PidTable::new(ctx.paths.adapter_pids())),
        );
        let vctx = ValidateCtx::new(daemon_version());
        let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
        let (entry, problem) = self.build(row, &ctx.paths, &pids, ctx.audit.clone(), &vctx, slot);
        if let Some((reason, note)) = problem {
            entry.mark_failed(reason, note);
        }
        self.watch_note(&entry, store);
        self.entries
            .write()
            .insert(row.name.clone(), Arc::clone(&entry));
        Ok(entry)
    }

    /// Stop the entry of `name` (not a crash), kill what is left of it and take it out of the
    /// registry and the event hub.
    pub(super) async fn retire(&self, name: &str) {
        let entry = self.entries.write().remove(name);
        if let Some(entry) = entry {
            // what the stop does to its state is not news about the row
            entry.detach();
            entry.stop(true).await;
            entry.kill_now().await;
        }
        self.events.register(name, &[], HostScope::All);
    }

    /// Replace the entry of `name` by one made from `row`.
    pub(super) async fn replace(
        &self,
        row: &AdapterRow,
        store: &Arc<AdapterStore>,
    ) -> Result<Arc<AdapterEntry>> {
        self.retire(&row.name).await;
        self.register(row, store)
    }

    /// One `adapter` request. Whatever changes an adapter, the table of verbs the CLI reads is
    /// written again afterwards.
    pub async fn handle(&self, request: AdapterRequest) -> Result<AdapterReply> {
        let changes = !request.is_read();
        let reply = self.handle_request(request).await;
        if changes {
            self.write_verb_table();
        }
        reply
    }

    async fn handle_request(&self, request: AdapterRequest) -> Result<AdapterReply> {
        match request {
            AdapterRequest::Install(args) => self.install(*args).await,
            AdapterRequest::Enable(args) => self.enable(*args).await,
            AdapterRequest::Disable { name } => self.disable(&name).await,
            AdapterRequest::Remove { name, purge } => self.remove(&name, purge).await,
            AdapterRequest::Rollback { name } => self.rollback(&name).await,
            AdapterRequest::List => self.list().await,
            AdapterRequest::Info { name } => self.info(&name).await,
            // a streamed request: the connection task runs it (`AdapterHost::invoke`)
            AdapterRequest::Invoke(_) => Err(Error::Internal(
                "a relayed verb is answered as a stream".to_string(),
            )),
            // a session is opened by the connection task, which knows who is asking
            AdapterRequest::SessionOpen(_) => Err(Error::Internal(
                "a session is opened by the connection task".to_string(),
            )),
            AdapterRequest::IndexUpdate {
                index_bytes,
                index_sig,
            } => {
                let _admin = self.admin.lock().await;
                let (verified, disabled) = self
                    .accept_index(index_bytes.as_bytes(), &index_sig)
                    .await?;
                Ok(AdapterReply::IndexAccepted {
                    sequence: verified.index.sequence,
                    key_id: verified.signed_by.key_id,
                    disabled,
                })
            }
        }
    }

    // ---- install ---------------------------------------------------------------------------

    /// The context the manifest of an adapter is validated in.
    fn with_validate_ctx<T>(
        &self,
        rows: &[AdapterRow],
        own_name: &str,
        signed: bool,
        args: &InstallArgs,
        f: impl FnOnce(&ValidateCtx) -> T,
    ) -> T {
        let reserved_adapters = runtime_hints::reserved_adapter_names();
        let reserved_runtimes = runtime_hints::reserved_runtime_names();
        let reserved_verbs = runtime_hints::reserved_verb_names();
        let builtin: Vec<&str> = args.builtin_verbs.iter().map(String::as_str).collect();
        // the verbs of the other installed adapters
        let others: Vec<(String, String)> = rows
            .iter()
            .filter(|r| r.name != own_name)
            .filter_map(|r| {
                Manifest::parse(r.manifest_json.as_bytes())
                    .ok()
                    .map(|m| (r.name.clone(), m))
            })
            .flat_map(|(name, m)| {
                m.provides
                    .verbs
                    .into_iter()
                    .map(move |v| (name.clone(), v.name))
            })
            .collect();
        let installed_verbs: Vec<(&str, &str)> = others
            .iter()
            .map(|(a, v)| (a.as_str(), v.as_str()))
            .collect();
        let mut ctx = ValidateCtx::new(daemon_version());
        ctx.signed_by_embedded_key = signed;
        ctx.allow_reserved_name = args.allow_reserved_name || args.source == "bundled";
        ctx.reserved_adapters = &reserved_adapters;
        ctx.reserved_runtimes = &reserved_runtimes;
        ctx.reserved_verbs = &reserved_verbs;
        ctx.builtin_verbs = &builtin;
        ctx.installed_verbs = &installed_verbs;
        f(&ctx)
    }

    async fn install(&self, args: InstallArgs) -> Result<AdapterReply> {
        let ctx = self.ctx()?;
        let paths = Arc::clone(&ctx.paths);
        let _admin = self.admin.lock().await;
        let store = self.store_or_create().await?;

        // 1. move the staged directory out of reach of the CLI before anything is checked
        let staged = paths
            .adapter_staging()
            .join(plain_component(&args.staging)?);
        let meta = std::fs::symlink_metadata(&staged).map_err(|_| {
            Error::AdapterInvalid(format!("there is no staged adapter '{}'", args.staging))
        })?;
        if !meta.is_dir() {
            return Err(Error::AdapterInvalid(format!(
                "the staged adapter '{}' is not a directory",
                args.staging
            )));
        }
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let incoming_path = paths.adapter_installed_root().join(format!(
            ".incoming-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::rename(&staged, &incoming_path)
            .map_err(|e| io_error("moving the staged adapter into place", e))?;
        let incoming = Pending::new(incoming_path.clone());

        // 2. what is in it
        let dir = incoming_path.clone();
        let inspected = tokio::task::spawn_blocking(move || inspect(&dir))
            .await
            .map_err(|e| {
                Error::Internal(format!("the inspection of the staged adapter failed: {e}"))
            })??;
        // 3. where it comes from
        let (trust, source, signed) = self.check_source(&args, &inspected, &incoming_path).await?;
        let manifest = inspected.manifest;
        self.check_revocation(&store, &manifest.name, &inspected.exe_sha256)
            .await?;

        // 4. the manifest, with the reserved names and the verbs of the others
        let rows = store.repo().list().await?;
        self.with_validate_ctx(&rows, &manifest.name, signed, &args, |vctx| {
            manifest.validate(vctx)
        })
        .map_err(manifest_error)?;

        if let Some(old) = rows.iter().find(|r| r.name == manifest.name).cloned() {
            // an installed name is an update: the same rules apply whichever command asked
            return self
                .update_installed(
                    &store,
                    &paths,
                    old,
                    incoming,
                    Inspected {
                        manifest,
                        ..inspected
                    },
                    trust,
                    source,
                    &args,
                )
                .await;
        }

        // 5. into its place: installed/<name>/<version>
        let target = paths.adapter_installed(&manifest.name, &manifest.version);
        let parent = target
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Error::Internal("an install path has a parent".to_string()))?;
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&parent) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(io_error("making the install directory", e));
            }
        }
        // an unreferenced directory of that name can only be left over from an install that failed
        remove_any(&target).map_err(|e| io_error("clearing the install directory", e))?;
        std::fs::rename(&incoming_path, &target)
            .map_err(|e| io_error("installing the adapter", e))?;
        let mut incoming = incoming;
        incoming.keep = true;
        let mut placed = Pending::new(target);

        // 6. the row, disabled
        let row = AdapterRow {
            name: manifest.name.clone(),
            kind: kind_text(manifest.kind).to_string(),
            version: manifest.version.clone(),
            exe_sha256: inspected.exe_sha256.clone(),
            manifest_json: String::from_utf8_lossy(&inspected.manifest_bytes).to_string(),
            source,
            trust,
            enabled: false,
            grant_json: None,
            grant_fingerprint: None,
            enforced_json: None,
            prev_version: None,
            prev_exe_sha256: None,
            prev_manifest_json: None,
            prev_grant_json: None,
            state_note: None,
            installed_at: now_text(),
            updated_at: now_text(),
        };
        store.repo().insert(&row).await?;
        placed.keep = true;
        self.register(&row, &store)?;
        self.audit_event(
            &row.name,
            "imported",
            Some(&row.version),
            Some(&row.exe_sha256),
        );

        let fingerprint = manifest.fingerprint(&row.exe_sha256);
        Ok(AdapterReply::Installed {
            message: format!(
                "Installed {} {} (disabled, nothing runs). Enable it with: oxidepm adapter enable {} --accept {}",
                row.name, row.version, row.name, fingerprint
            ),
            name: row.name,
            version: row.version,
            fingerprint,
            replaced: None,
            warnings: Vec::new(),
        })
    }

    /// Check where an adapter comes from: `(trust, source, signed by an embedded key)`.
    async fn check_source(
        &self,
        args: &InstallArgs,
        inspected: &Inspected,
        root: &Path,
    ) -> Result<(String, String, bool)> {
        let manifest = &inspected.manifest;
        let exe_sha256 = &inspected.exe_sha256;
        match args.source.as_str() {
            "index" | "bundled" => {
                let (index, trust, bundle) = if args.source == "bundled" {
                    let (bundle, index) = tokio::task::spawn_blocking(installed_release_bundle)
                        .await
                        .map_err(|e| Error::Internal(format!("the bundle check failed: {e}")))??;
                    (index, "bundled".to_string(), Some(bundle))
                } else {
                    let (Some(bytes), Some(sig)) = (&args.index_bytes, &args.index_sig) else {
                        return Err(Error::AdapterInvalid(
                            "an adapter from the index needs the index and its signature"
                                .to_string(),
                        ));
                    };
                    let (verified, _) = self.accept_index(bytes.as_bytes(), sig).await?;
                    (
                        verified.index,
                        format!("signed:{}", verified.signed_by.key_id),
                        None,
                    )
                };
                let adapter = index.adapter(&manifest.name).ok_or_else(|| {
                    Error::AdapterInvalid(format!(
                        "the adapter catalog does not list '{}'",
                        manifest.name
                    ))
                })?;
                if adapter.kind != manifest.kind {
                    return Err(Error::AdapterInvalid(format!(
                        "the index lists '{}' as a {}, the manifest says {}",
                        manifest.name,
                        kind_text(adapter.kind),
                        kind_text(manifest.kind)
                    )));
                }
                let release = adapter
                    .releases
                    .iter()
                    .find(|r| r.version == manifest.version)
                    .ok_or_else(|| {
                        Error::AdapterInvalid(format!(
                            "the adapter catalog does not list {} {}",
                            manifest.name, manifest.version
                        ))
                    })?;
                if release.yanked {
                    return Err(Error::AdapterInvalid(format!(
                        "{} {} is yanked: {}",
                        manifest.name,
                        manifest.version,
                        release.yank_reason.as_deref().unwrap_or("no reason given")
                    )));
                }
                let target = host_target();
                let artifact = release.artifacts.get(&target).ok_or_else(|| {
                    Error::AdapterInvalid(format!(
                        "the index has no archive of {} {} for {target}",
                        manifest.name, manifest.version
                    ))
                })?;
                let equal: bool = artifact
                    .exe_sha256
                    .to_ascii_lowercase()
                    .as_bytes()
                    .ct_eq(exe_sha256.as_bytes())
                    .into();
                if !equal {
                    return Err(Error::ChecksumMismatch(format!(
                        "the executable has sha256 {exe_sha256}; the adapter catalog says {}",
                        artifact.exe_sha256
                    )));
                }
                if index.is_revoked(exe_sha256) {
                    return Err(Error::AdapterInvalid(format!(
                        "the index revokes this executable of {}",
                        manifest.name
                    )));
                }
                let digest = artifact.manifest_sha256.as_ref().ok_or_else(|| {
                    Error::AdapterInvalid("the published artifact has no manifest digest".into())
                })?;
                let actual = hex(&Sha256::digest(&inspected.manifest_bytes));
                if !bool::from(
                    actual
                        .as_bytes()
                        .ct_eq(digest.to_ascii_lowercase().as_bytes()),
                ) {
                    return Err(Error::ChecksumMismatch(format!(
                        "the manifest of {} does not match its published digest",
                        manifest.name
                    )));
                }
                let root = root.to_path_buf();
                let files = inspected.auxiliary_files.clone();
                let artifact = artifact.clone();
                let signed = bundle.is_none();
                tokio::task::spawn_blocking(move || {
                    if let Some(bundle) = bundle {
                        let name = release_bundle::archive_name(&artifact.url)
                            .map_err(|e| Error::AdapterInvalid(e.to_string()))?;
                        let actual = bundle
                            .copy_verified(name, artifact.size, std::io::sink())
                            .map_err(|e| Error::ChecksumMismatch(e.to_string()))?;
                        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                            return Err(Error::ChecksumMismatch(format!(
                                "the bundle catalog has a different sha256 for {name}"
                            )));
                        }
                    }
                    verify_auxiliary_files(&root, &files, &artifact.file_sha256)
                })
                .await
                .map_err(|e| Error::Internal(format!("the staged file check failed: {e}")))??;
                Ok((trust, args.source.clone(), signed))
            }
            other if other.starts_with("pinned:") => {
                if other.len() > 600 || other.chars().any(char::is_control) {
                    return Err(Error::AdapterInvalid(
                        "the source of a pinned adapter is not printable text".to_string(),
                    ));
                }
                Ok(("pinned".to_string(), other.to_string(), false))
            }
            other => Err(Error::AdapterInvalid(format!("unknown source '{other}'"))),
        }
    }

    // ---- enable ----------------------------------------------------------------------------

    async fn enable(&self, args: EnableArgs) -> Result<AdapterReply> {
        let ctx = self.ctx()?;
        let paths = Arc::clone(&ctx.paths);
        let _admin = self.admin.lock().await;
        let store = self.store_or_create().await?;
        let repo = store.repo();
        let mut row = repo.get(&args.name).await?.ok_or_else(|| {
            Error::AdapterMissing(format!(
                "adapter '{}' is not installed. Import it with: oxidepm adapter import {}",
                args.name, args.name
            ))
        })?;
        self.check_revocation(&store, &row.name, &row.exe_sha256)
            .await?;
        let manifest = Manifest::parse(row.manifest_json.as_bytes()).map_err(manifest_error)?;
        let fingerprint = manifest.fingerprint(&row.exe_sha256);

        let official_release = is_official_release_trust(&row.trust);

        // the files are what was imported
        let exec = paths.adapter_exec(&row.name, &row.version, &manifest.exec);
        if std::fs::symlink_metadata(&exec).is_err() {
            return Err(Error::AdapterMissing(format!(
                "adapter '{}' has no files on this machine (needs_reinstall). Import it again: oxidepm adapter import {}",
                row.name, row.name
            )));
        }

        // consent
        match (&args.accept, args.yes) {
            (Some(given), _) => {
                if !given.eq_ignore_ascii_case(&fingerprint) {
                    return Err(Error::AdapterInvalid(format!(
                        "--accept {given} is not the fingerprint of {} {}. The fingerprint of this release is {fingerprint}; check what it needs first: oxidepm adapter info {}",
                        row.name, row.version, row.name
                    )));
                }
            }
            (None, true) => {
                if (!manifest.provides.runtimes.is_empty()
                    || !manifest.provides.programs.is_empty())
                    && !official_release
                {
                    return Err(Error::AdapterInvalid(format!(
                        "{} provides runtimes or programs and requires explicit consent. Pass --accept {fingerprint}",
                        row.name
                    )));
                }
                if !manifest.permissions.is_pure_class()
                    || (manifest
                        .permissions
                        .fs
                        .read
                        .iter()
                        .any(|p| p == "/etc/hosts")
                        && !std::fs::symlink_metadata("/etc/hosts").is_ok_and(|m| m.is_file()))
                {
                    return Err(Error::AdapterInvalid(format!(
                        "--yes is honoured only for an adapter with read-only host access, no network, spawning or environment access, and filesystem access limited to adapter state and reading a regular /etc/hosts. {} requests: {}. Pass --accept {fingerprint} after reviewing the permissions",
                        row.name,
                        manifest.permissions.broad_items().join(", ")
                    )));
                }
            }
            (None, false) => {
                return Err(Error::AdapterInvalid(format!(
                    "enabling {} needs your consent to what it asks for. Pass --accept {fingerprint}",
                    row.name
                )));
            }
        }

        // the grant: what the manifest asks, possibly lowered, never raised
        let mut grant: Permissions = manifest.permissions.clone();
        if let Some(text) = &args.level {
            let wanted = level_of(text)?;
            if wanted > grant.host.level {
                return Err(Error::AdapterInvalid(format!("--level {text} is higher than the {} the manifest asks for; a level can only be lowered", level_text(grant.host.level))));
            }
            grant.host.level = wanted;
        }
        if let Some(text) = &args.scope {
            let wanted = match text.as_str() {
                "all" => HostScope::All,
                "opted-in" => HostScope::OptedIn,
                other => {
                    return Err(Error::AdapterInvalid(format!(
                        "--scope takes all or opted-in, not '{other}'"
                    )))
                }
            };
            if wanted > grant.host.scope {
                return Err(Error::AdapterInvalid(format!("--scope {text} is wider than the scope the manifest asks for; a scope can only be lowered")));
            }
            grant.host.scope = wanted;
        }
        if grant.host.level == HostLevel::Admin && !args.interactive {
            return Err(Error::AdapterInvalid(
                "an admin grant lets the adapter start arbitrary commands; it needs an interactive terminal, and --accept and --yes do not replace it".to_string(),
            ));
        }

        check_enabled_set(&repo.list().await?, &manifest, &grant)?;

        // what the OS layer enforces here, measured under this adapter's own profile
        let record = self
            .measure_enforcement(&paths, &row, &grant, args.allow_unenforced)
            .await?;
        let enforcement_note = record.enforcement_note(&grant.requires_enforcement);
        if let Err(missing) = record.check_with_unix_reachability(&grant.requires_enforcement) {
            let names: Vec<&str> = missing.iter().map(|e| enforce_name(*e)).collect();
            let unix_note = enforcement_note.map_or(String::new(), |note| format!(". {note}"));
            return Err(Error::AdapterInvalid(format!(
                "{} requires the sandbox to enforce {}, which this machine cannot ({}){unix_note}. If you accept that, pass --allow-unenforced",
                row.name,
                names.join(", "),
                self.caps().summary()
            )));
        }
        if args.yes && args.accept.is_none() && !record.allow_unenforced && !official_release {
            if let Some(note) = record.unix_note() {
                return Err(Error::AdapterInvalid(format!(
                    "Enabling {} requires explicit consent. {note}. Pass --accept {fingerprint} or --allow-unenforced",
                    row.name
                )));
            }
        }

        // record the grant, then prove the adapter starts and answers
        let previous = row.clone();
        row.enabled = true;
        row.grant_json =
            Some(serde_json::to_string(&grant).map_err(|e| Error::Internal(e.to_string()))?);
        row.grant_fingerprint = Some(fingerprint.clone());
        row.enforced_json =
            Some(serde_json::to_string(&record).map_err(|e| Error::Internal(e.to_string()))?);
        row.state_note = None;
        row.updated_at = now_text();
        repo.update(&row).await?;
        let entry = self.replace(&row, &store).await?;
        if let Err(e) = self.prove(&entry, manifest.kind).await {
            // a broken adapter fails here, not at first use: back to what it was, with the reason
            let reverted = AdapterRow {
                state_note: Some(format!("the last attempt to enable it failed: {e}")),
                updated_at: now_text(),
                ..previous.clone()
            };
            self.retire(&row.name).await;
            let _ = repo.update(&reverted).await;
            self.register(&reverted, &store)?;
            return Err(e);
        }
        self.audit_event(
            &row.name,
            "enabled",
            Some(&row.version),
            Some(&row.exe_sha256),
        );

        let enforced: Vec<String> = record
            .enforced
            .iter()
            .map(|e| enforce_name(*e).to_string())
            .collect();
        Ok(AdapterReply::Enabled {
            message: format!(
                "Enabled {} {} (level {}, scope {})",
                row.name,
                row.version,
                level_text(grant.host.level),
                scope_text(grant.host.scope)
            ),
            name: row.name,
            fingerprint,
            level: level_text(grant.host.level).to_string(),
            scope: scope_text(grant.host.scope).to_string(),
            enforced,
            unix_note: enforcement_note,
        })
    }

    /// Run the sandbox self-test under the profile of `grant`. Without the launcher nothing is
    /// enforced, which is what the record then says.
    async fn measure_enforcement(
        &self,
        paths: &Paths,
        row: &AdapterRow,
        grant: &Permissions,
        allow_unenforced: bool,
    ) -> Result<EnforcedRecord> {
        let caps = self.caps().clone();
        let Some(launcher) = self.cfg.launcher.clone() else {
            return Ok(EnforcedRecord {
                allow_unenforced,
                enforced: Vec::new(),
                unix_reachable: Vec::new(),
                caps,
            });
        };
        let state_dir = paths.adapter_state(&row.name);
        paths.ensure_adapter_state_dir(&row.name)?;
        let search: Vec<PathBuf> = ADAPTER_PATH.iter().map(PathBuf::from).collect();
        let input = ProfileInput {
            state_dir: &state_dir,
            daemon_home: paths.home(),
            cwd: None,
            search_path: &search,
        };
        let profile = Profile::from_permissions(grant, &input);
        let env = SelftestEnv {
            daemon_socket: paths.socket(),
            home_file: paths.adapters_db(),
            scratch_base: None,
        };
        let launcher =
            tokio::task::spawn_blocking(move || super::proc::open_verified_launcher(&launcher))
                .await
                .map_err(|e| {
                    Error::AdapterUnavailable(format!(
                        "the launcher integrity check failed to run: {e}"
                    ))
                })?
                .map_err(Error::AdapterUnavailable)?;
        let launcher_path = PathBuf::from(format!("/proc/self/fd/{}", launcher.as_raw_fd()));
        let report = selftest(&launcher_path, &profile, &env, allow_unenforced)
            .await
            .map_err(|e| {
                Error::AdapterUnavailable(format!(
                    "the sandbox self-test of '{}' failed: {e}",
                    row.name
                ))
            })?;
        let read_any = grant.fs.read.iter().any(|p| p == "any");
        Ok(
            EnforcedRecord::from_report(&report, caps, allow_unenforced, read_any)
                .with_required_enforcement(&grant.requires_enforcement),
        )
    }

    /// Start the entry and make one `ping` round trip; a tool is stopped again after it.
    async fn prove(&self, entry: &Arc<AdapterEntry>, kind: Kind) -> Result<()> {
        if kind == Kind::Service {
            entry.start();
        }
        let answer = entry
            .call(
                method::PING,
                &serde_json::json!({}),
                Duration::from_secs(15),
            )
            .await;
        let result = match answer {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        if kind == Kind::Tool {
            entry.stop(false).await;
        }
        result
    }

    // ---- disable, remove -------------------------------------------------------------------

    async fn disable(&self, name: &str) -> Result<AdapterReply> {
        let _admin = self.admin.lock().await;
        let store = self.store_or_create().await?;
        let mut row =
            store.repo().get(name).await?.ok_or_else(|| {
                Error::AdapterMissing(format!("adapter '{name}' is not installed"))
            })?;
        let was_enabled = row.enabled;
        // a disabled adapter holds no rights: enabling it again asks for consent again
        row.enabled = false;
        row.grant_json = None;
        row.grant_fingerprint = None;
        row.state_note = None;
        row.updated_at = now_text();
        store.repo().update(&row).await?;
        self.replace(&row, &store).await?;
        self.audit_event(
            &row.name,
            "disabled",
            Some(&row.version),
            Some(&row.exe_sha256),
        );
        Ok(AdapterReply::Done {
            message: if was_enabled {
                format!("Disabled {name}")
            } else {
                format!("{name} was already disabled")
            },
            warnings: Vec::new(),
        })
    }

    async fn remove(&self, name: &str, purge: bool) -> Result<AdapterReply> {
        let ctx = self.ctx()?;
        let paths = Arc::clone(&ctx.paths);
        let _admin = self.admin.lock().await;
        let store = self.store_or_create().await?;
        let row =
            store.repo().get(name).await?.ok_or_else(|| {
                Error::AdapterMissing(format!("adapter '{name}' is not installed"))
            })?;
        self.retire(name).await;
        // the files first: a crash after this leaves a row without files (needs_reinstall), which
        // `remove` can finish; the other way round would leave files nobody refers to
        let tree = paths.adapter_installed_root().join(plain_component(name)?);
        remove_any(&tree).map_err(|e| io_error("deleting the installed files", e))?;
        store.repo().delete(name).await?;
        // the plans it made: a later adapter of the same name must not find them
        if let Err(e) = store.repo().delete_plans_of(name).await {
            tracing::warn!("Could not drop the stored launch plans of the adapter {name}: {e}");
        }
        self.revoked.lock().remove(name);
        let mut warnings = Vec::new();
        if purge {
            remove_any(&paths.adapter_state(name))
                .map_err(|e| io_error("deleting the state directory", e))?;
        } else if paths.adapter_state(name).exists() {
            warnings.push(format!(
                "the state directory {} was kept (credentials may be in it); add --purge to delete it",
                paths.adapter_state(name).display()
            ));
        }
        self.audit_event(name, "removed", Some(&row.version), Some(&row.exe_sha256));
        Ok(AdapterReply::Done {
            message: format!("Removed {name} {}", row.version),
            warnings,
        })
    }

    // ---- list, info ------------------------------------------------------------------------

    fn info_of(&self, row: &AdapterRow, cached: Option<&VerifiedIndex>) -> AdapterInfo {
        let paths = self.ctx.get().map(|c| Arc::clone(&c.paths));
        let manifest = Manifest::parse(row.manifest_json.as_bytes()).ok();
        let (summary, fingerprint) = match &manifest {
            Some(m) => (m.summary.clone(), m.fingerprint(&row.exe_sha256)),
            None => (String::new(), String::new()),
        };
        let missing = match (&paths, &manifest) {
            (Some(p), Some(m)) => {
                std::fs::symlink_metadata(p.adapter_exec(&row.name, &row.version, &m.exec)).is_err()
            }
            _ => false,
        };
        let entry = self.entry(&row.name);
        let (mut state, mut note, stderr_tail, crashes, strikes) = match &entry {
            Some(e) => {
                let view = e.view();
                let note = match (&view.state, &view.note) {
                    (EntryState::Failed(reason), Some(n)) => {
                        Some(format!("{}: {}", reason.as_str(), n))
                    }
                    (EntryState::Failed(reason), None) => Some(reason.as_str().to_string()),
                    (_, n) => n.clone(),
                };
                (
                    view.state.name().to_string(),
                    note,
                    e.stderr_tail(),
                    view.crashes,
                    view.strikes,
                )
            }
            None => (
                if row.enabled { "stopped" } else { "disabled" }.to_string(),
                None,
                Vec::new(),
                0,
                0,
            ),
        };
        if missing {
            state = "needs_reinstall".to_string();
            note = Some(format!("the files are missing; import {} again", row.name));
        } else if note.is_none() {
            note = row.state_note.clone();
        }
        let yanked = cached.and_then(|v| {
            let release = v
                .index
                .adapter(&row.name)?
                .releases
                .iter()
                .find(|r| r.version == row.version)?;
            release.yanked.then(|| {
                release
                    .yank_reason
                    .clone()
                    .unwrap_or_else(|| "no reason given".to_string())
            })
        });
        AdapterInfo {
            name: row.name.clone(),
            kind: row.kind.clone(),
            version: row.version.clone(),
            summary,
            source: row.source.clone(),
            trust: row.trust.clone(),
            enabled: row.enabled,
            state,
            note,
            fingerprint,
            exe_sha256: row.exe_sha256.clone(),
            manifest_json: row.manifest_json.clone(),
            grant_json: row.grant_json.clone(),
            enforced_json: row.enforced_json.clone(),
            prev_version: row.prev_version.clone(),
            yanked,
            stderr_tail,
            crashes,
            strikes,
            installed_at: row.installed_at.clone(),
        }
    }

    /// The rows, without creating anything for a home that has no adapters.
    async fn rows_if_any(&self) -> Result<Vec<AdapterRow>> {
        let store = self.store.lock().clone();
        match store {
            Some(store) => store.repo().list().await,
            None => Ok(Vec::new()),
        }
    }

    async fn list(&self) -> Result<AdapterReply> {
        let rows = self.rows_if_any().await?;
        let cached = self.cached_index();
        Ok(AdapterReply::List {
            adapters: rows
                .iter()
                .map(|r| self.info_of(r, cached.as_ref()))
                .collect(),
        })
    }

    async fn info(&self, name: &str) -> Result<AdapterReply> {
        let rows = self.rows_if_any().await?;
        let row = rows
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| Error::AdapterMissing(format!("adapter '{name}' is not installed")))?;
        let cached = self.cached_index();
        Ok(AdapterReply::Info {
            adapter: Box::new(self.info_of(row, cached.as_ref())),
        })
    }

    // ---- update and rollback ---------------------------------------------------------------

    /// An installed name arrives again. The release must come from the same source (or the
    /// user passes `--switch-source`), a lower version needs `--allow-downgrade`, a release that
    /// adds permissions to an enabled adapter needs the new fingerprint, and the install is ordered
    /// so that a crash at any point leaves something that runs: (1) the new version's directory is
    /// in place, (2) one `adapters.db` update records it with the old version as the rollback
    /// step, (3) the older rollback step's directory is deleted. An enabled adapter is then
    /// restarted and watched through probation; if it fails, the old version is put back.
    #[allow(clippy::too_many_arguments)]
    async fn update_installed(
        &self,
        store: &Arc<AdapterStore>,
        paths: &Arc<Paths>,
        old: AdapterRow,
        mut incoming: Pending,
        inspected: Inspected,
        trust: String,
        source: String,
        args: &InstallArgs,
    ) -> Result<AdapterReply> {
        let manifest = inspected.manifest;
        let name = manifest.name.clone();
        let old_manifest = Manifest::parse(old.manifest_json.as_bytes()).map_err(manifest_error)?;
        let new_version = semver::Version::parse(&manifest.version)
            .map_err(|e| Error::AdapterInvalid(e.to_string()))?;
        let old_version = semver::Version::parse(&old.version)
            .map_err(|e| Error::AdapterInvalid(e.to_string()))?;

        // the same source, or the user says so
        if old.trust != trust && !args.switch_source {
            return Err(Error::AdapterInvalid(format!(
                "'{name}' was installed from {} and this release comes from {}; a release must come from the same source. If you mean to change the source, pass --switch-source",
                describe_trust(&old.trust),
                describe_trust(&trust)
            )));
        }
        let target = paths.adapter_installed(&name, &manifest.version);
        let new_fingerprint = manifest.fingerprint(&inspected.exe_sha256);

        // the same version again: only to bring back files that are gone
        if new_version == old_version {
            if std::fs::symlink_metadata(&target).is_ok() {
                return Err(Error::AdapterInvalid(format!(
                    "{name} {} is installed already",
                    old.version
                )));
            }
            let equal: bool = old
                .exe_sha256
                .as_bytes()
                .ct_eq(inspected.exe_sha256.as_bytes())
                .into();
            if !equal {
                return Err(Error::ChecksumMismatch(format!(
                    "this is not the release that was installed: the executable has sha256 {}, the installed one {}",
                    inspected.exe_sha256, old.exe_sha256
                )));
            }
            if old.manifest_json.as_bytes() != inspected.manifest_bytes {
                return Err(Error::ChecksumMismatch(format!(
                    "the manifest differs from the installed release of {name} {}; remove it and import the release again",
                    old.version
                )));
            }
            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(parent) {
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(io_error("making the install directory", e));
                    }
                }
            }
            std::fs::rename(&incoming.path, &target)
                .map_err(|e| io_error("bringing the adapter's files back", e))?;
            incoming.keep = true;
            let mut row = old.clone();
            row.state_note = None;
            row.updated_at = now_text();
            store.repo().update(&row).await?;
            self.replace(&row, store).await?;
            self.audit_event(&name, "imported", Some(&row.version), Some(&row.exe_sha256));
            return Ok(AdapterReply::Installed {
                message: format!("Reinstalled the files of {name} {}", row.version),
                name,
                version: row.version,
                fingerprint: new_fingerprint,
                replaced: None,
                warnings: Vec::new(),
            });
        }
        if new_version < old_version && !args.allow_downgrade {
            return Err(Error::AdapterInvalid(format!(
                "{name} {} is older than the installed {}; pass --allow-downgrade if you mean it",
                manifest.version, old.version
            )));
        }

        // what grew (an adapter that is not enabled holds no rights: consent comes at enable)
        let growth = manifest.grows_over(&old_manifest);
        let old_grant: Option<Permissions> = old
            .grant_json
            .as_deref()
            .and_then(|g| serde_json::from_str(g).ok());
        let mut grant_json = old.grant_json.clone();
        let mut grant_fingerprint = old.grant_fingerprint.clone();
        let mut enforced_json = old.enforced_json.clone();
        if old.enabled {
            if !growth.is_empty() {
                let accepted = args
                    .accept
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&new_fingerprint));
                if !accepted {
                    let what: Vec<String> = growth
                        .iter()
                        .map(|g| format!("{} ({})", g.field, g.detail))
                        .collect();
                    return Err(Error::AdapterInvalid(format!(
                        "{name} {} asks for more than {} has: {}. It was not installed and {name} stays on {}. To accept it, pass --accept {new_fingerprint}",
                        manifest.version,
                        old.version,
                        what.join("; "),
                        old.version
                    )));
                }
            }
            let old_grant = old_grant.ok_or_else(|| {
                Error::AdapterInvalid(format!("the enabled adapter '{name}' has no valid grant"))
            })?;
            let mut grant = manifest.permissions.clone();
            if old_grant.host.level < old_manifest.permissions.host.level {
                grant.host.level = grant.host.level.min(old_grant.host.level);
            }
            if old_grant.host.scope < old_manifest.permissions.host.scope {
                grant.host.scope = grant.host.scope.min(old_grant.host.scope);
            }
            check_enabled_set(&store.repo().list().await?, &manifest, &grant)?;
            if grant != old_grant {
                let allow = EnforcedRecord::parse(old.enforced_json.as_deref())
                    .is_some_and(|r| r.allow_unenforced);
                let probe_row = AdapterRow {
                    version: manifest.version.clone(),
                    ..old.clone()
                };
                let record = self
                    .measure_enforcement(paths, &probe_row, &grant, allow)
                    .await?;
                if let Err(missing) =
                    record.check_with_unix_reachability(&grant.requires_enforcement)
                {
                    let names: Vec<&str> = missing.iter().map(|e| enforce_name(*e)).collect();
                    let unix_note = record
                        .enforcement_note(&grant.requires_enforcement)
                        .map_or(String::new(), |note| format!(". {note}"));
                    return Err(Error::AdapterInvalid(format!(
                        "{name} {} requires the sandbox to enforce {}, which this machine cannot{unix_note}. {name} stays on {}",
                        manifest.version,
                        names.join(", "),
                        old.version
                    )));
                }
                enforced_json = Some(
                    serde_json::to_string(&record).map_err(|e| Error::Internal(e.to_string()))?,
                );
            }
            grant_json =
                Some(serde_json::to_string(&grant).map_err(|e| Error::Internal(e.to_string()))?);
            grant_fingerprint = Some(new_fingerprint.clone());
        }

        // (1) the new version's directory. A directory of that version that is kept as the rollback
        // step is replaced by this release.
        let reuses_previous = old.prev_version.as_deref() == Some(manifest.version.as_str());
        remove_any(&target).map_err(|e| io_error("clearing the install directory", e))?;
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(parent) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(io_error("making the install directory", e));
                }
            }
        }
        std::fs::rename(&incoming.path, &target)
            .map_err(|e| io_error("installing the new version", e))?;
        incoming.keep = true;
        let mut placed = Pending::new(target);

        // (2) one update of the row: the new version, the old one as the rollback step
        let older_prev = old.prev_version.clone();
        let mut row = AdapterRow {
            name: name.clone(),
            kind: kind_text(manifest.kind).to_string(),
            version: manifest.version.clone(),
            exe_sha256: inspected.exe_sha256.clone(),
            manifest_json: String::from_utf8_lossy(&inspected.manifest_bytes).to_string(),
            source,
            trust,
            enabled: old.enabled,
            grant_json,
            grant_fingerprint,
            enforced_json,
            prev_version: None,
            prev_exe_sha256: None,
            prev_manifest_json: None,
            prev_grant_json: None,
            state_note: None,
            installed_at: old.installed_at.clone(),
            updated_at: now_text(),
        };
        remember_previous(&mut row, &old)?;
        store.repo().update(&row).await?;
        placed.keep = true;

        // (3) the rollback step before this one goes
        if let Some(older) = older_prev.filter(|v| !reuses_previous && *v != row.version) {
            let dir = paths.adapter_installed(&name, &older);
            if let Err(e) = remove_any(&dir) {
                warn!(
                    "Could not delete the older version {}: {}",
                    dir.display(),
                    e
                );
            }
        }
        self.audit_event(&name, "updated", Some(&row.version), Some(&row.exe_sha256));

        // the new version runs; an enabled adapter is watched until it has proved itself
        let entry = self.replace(&row, store).await?;
        if row.enabled {
            if let Err(why) = self.probation(&entry, manifest.kind).await {
                let reason = why.to_string();
                let restored = self.restore_previous(store, paths, &row, false).await;
                if let Err(e) = restored {
                    self.retire(&name).await;
                    row.enabled = false;
                    row.state_note =
                        Some(format!("failed probation: {reason}; rollback failed: {e}"));
                    row.updated_at = now_text();
                    store.repo().update(&row).await?;
                    self.register(&row, store)?;
                    return Err(Error::AdapterUnavailable(format!(
                        "{name} {} was disabled after failing probation ({reason}). Rollback to {} failed: {e}",
                        row.version, old.version
                    )));
                }
                self.audit_event(
                    &name,
                    "rolled_back",
                    Some(&old.version),
                    Some(&old.exe_sha256),
                );
                self.events.publish_adapter(
                    &name,
                    "rolled_back",
                    Some(&old.version),
                    Some("failed probation"),
                );
                return Err(Error::AdapterUnavailable(format!(
                    "{name} {} did not pass its probation ({reason}). It was rolled back to {}",
                    row.version, old.version
                )));
            }
        }
        if old.trust != row.trust {
            self.forget_previous_across_sources(store, paths, &row)
                .await;
        }
        Ok(AdapterReply::Installed {
            message: format!(
                "Updated {name} {} to {}{}",
                old.version,
                row.version,
                if row.enabled {
                    " (it passed its probation)"
                } else {
                    " (disabled)"
                }
            ),
            name,
            version: row.version,
            fingerprint: new_fingerprint,
            replaced: Some(old.version),
            warnings: Vec::new(),
        })
    }

    /// An update that changed the trust source keeps its previous release through probation.
    /// The earlier version's files are removed after promotion.
    async fn forget_previous_across_sources(
        &self,
        store: &Arc<AdapterStore>,
        paths: &Arc<Paths>,
        row: &AdapterRow,
    ) {
        let Some(prev) = row.prev_version.clone() else {
            return;
        };
        let cleared = AdapterRow {
            prev_version: None,
            prev_exe_sha256: None,
            prev_manifest_json: None,
            prev_grant_json: None,
            updated_at: now_text(),
            ..row.clone()
        };
        if let Err(e) = store.repo().update(&cleared).await {
            warn!("Could not clear the rollback step of '{}': {}", row.name, e);
            return;
        }
        let dir = paths.adapter_installed(&row.name, &prev);
        if let Err(e) = remove_any(&dir) {
            warn!(
                "Could not delete the earlier version {}: {}",
                dir.display(),
                e
            );
        }
    }

    /// The probation of an updated adapter: it must start and answer, and a service must
    /// stay up for the probation period.
    async fn probation(&self, entry: &Arc<AdapterEntry>, kind: Kind) -> Result<()> {
        self.prove(entry, kind).await?;
        if kind == Kind::Service {
            let until = std::time::Instant::now() + self.cfg.probation;
            loop {
                let state = entry.state();
                if state != EntryState::Ready {
                    return Err(Error::AdapterCrashed(format!(
                        "it left the ready state during probation (it is {})",
                        state.name()
                    )));
                }
                if std::time::Instant::now() >= until {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    /// Put the rollback step of `row` back as the version. With `keep_current` the version that
    /// is left becomes the new rollback step (the swap that `--rollback` makes); without it the
    /// version that is left is deleted (a failed probation). The adapter is started again if it
    /// is enabled and has a grant for the earlier version.
    async fn restore_previous(
        &self,
        store: &Arc<AdapterStore>,
        paths: &Arc<Paths>,
        row: &AdapterRow,
        keep_current: bool,
    ) -> Result<()> {
        let name = &row.name;
        let (Some(prev_version), Some(prev_manifest), Some(prev_exe)) = (
            row.prev_version.clone(),
            row.prev_manifest_json.clone(),
            row.prev_exe_sha256.clone(),
        ) else {
            return Err(Error::AdapterInvalid(format!(
                "'{name}' has no earlier version to go back to"
            )));
        };
        self.check_revocation(store, name, &prev_exe).await?;
        let prev_dir = paths.adapter_installed(name, &prev_version);
        if std::fs::symlink_metadata(&prev_dir).is_err() {
            return Err(Error::AdapterMissing(format!("the files of {name} {prev_version} are gone; import it again with --version {prev_version} --allow-downgrade")));
        }
        let prev_parsed = Manifest::parse(prev_manifest.as_bytes()).map_err(manifest_error)?;
        let metadata: PreviousRelease = row.prev_grant_json.as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .ok_or_else(|| Error::AdapterInvalid(format!(
                "the saved rollback release has no trust and enforcement record; import {name} {prev_version} again"
            )))?;
        if let Some(json) = metadata.grant.as_deref().filter(|_| row.enabled) {
            let grant: Permissions = serde_json::from_str(json)
                .map_err(|e| Error::AdapterInvalid(format!("invalid rollback grant: {e}")))?;
            check_enabled_set(&store.repo().list().await?, &prev_parsed, &grant)?;
        }
        let mut restored = AdapterRow {
            kind: kind_text(prev_parsed.kind).to_string(),
            source: metadata.source,
            trust: metadata.trust,
            version: prev_version,
            exe_sha256: prev_exe.clone(),
            manifest_json: prev_manifest,
            grant_fingerprint: metadata
                .grant
                .as_ref()
                .map(|_| prev_parsed.fingerprint(&prev_exe)),
            enabled: row.enabled && metadata.grant.is_some(),
            grant_json: metadata.grant,
            enforced_json: metadata.enforcement,
            prev_version: None,
            prev_exe_sha256: None,
            prev_manifest_json: None,
            prev_grant_json: None,
            state_note: None,
            updated_at: now_text(),
            ..row.clone()
        };
        if keep_current {
            remember_previous(&mut restored, row)?;
        }
        store.repo().update(&restored).await?;
        if !keep_current {
            let current = paths.adapter_installed(name, &row.version);
            if let Err(e) = remove_any(&current) {
                warn!("Could not delete {}: {}", current.display(), e);
            }
        }
        let entry = self.replace(&restored, store).await?;
        if restored.enabled && restored.kind == "service" {
            entry.start();
        }
        Ok(())
    }

    async fn rollback(&self, name: &str) -> Result<AdapterReply> {
        let ctx = self.ctx()?;
        let paths = Arc::clone(&ctx.paths);
        let _admin = self.admin.lock().await;
        let store = self.store_or_create().await?;
        let row =
            store.repo().get(name).await?.ok_or_else(|| {
                Error::AdapterMissing(format!("adapter '{name}' is not installed"))
            })?;
        let from = row.version.clone();
        let to = row.prev_version.clone().ok_or_else(|| {
            Error::AdapterInvalid(format!("'{name}' has no earlier version to go back to"))
        })?;
        self.restore_previous(&store, &paths, &row, true).await?;
        self.audit_event(name, "rolled_back", Some(&to), None);
        self.events
            .publish_adapter(name, "rolled_back", Some(&to), Some("rollback"));
        Ok(AdapterReply::Done {
            message: format!("Rolled {name} back from {from} to {to} (no network was used)"),
            warnings: Vec::new(),
        })
    }

    // ---- dry run ---------------------------------------------------------------------------

    /// What an `adapter` request would do, from read-only state: nothing is stopped, started,
    /// written or deleted.
    pub async fn plan(&self, request: &AdapterRequest) -> Result<(Vec<PlannedItem>, Vec<String>)> {
        let rows = self.rows_if_any().await?;
        let find = |name: &str| rows.iter().find(|r| r.name == name);
        let missing =
            |name: &str| Error::AdapterMissing(format!("adapter '{name}' is not installed"));
        let mut items = Vec::new();
        let mut warnings = Vec::new();
        match request {
            AdapterRequest::Enable(args) => {
                let row = find(&args.name).ok_or_else(|| missing(&args.name))?;
                let manifest =
                    Manifest::parse(row.manifest_json.as_bytes()).map_err(manifest_error)?;
                let level = args
                    .level
                    .clone()
                    .unwrap_or_else(|| level_text(manifest.permissions.host.level).to_string());
                let scope = args
                    .scope
                    .clone()
                    .unwrap_or_else(|| scope_text(manifest.permissions.host.scope).to_string());
                items.push(PlannedItem::new(None, &row.name, "grant").detail(format!(
                    "level {level}, scope {scope}, memory {} MiB",
                    manifest.permissions.memory_mb
                )));
                items.push(
                    PlannedItem::new(None, &row.name, "self-test")
                        .detail("the sandbox self-test would run under this grant"),
                );
                items.push(
                    PlannedItem::new(
                        None,
                        &row.name,
                        if manifest.kind == Kind::Service {
                            "start"
                        } else {
                            "check"
                        },
                    )
                    .detail(if manifest.kind == Kind::Service {
                        "the service would be started"
                    } else {
                        "the tool would be started once for a round trip and stopped"
                    }),
                );
                if row.enabled {
                    warnings.push(format!(
                        "{} is enabled already; the grant would be replaced",
                        row.name
                    ));
                }
            }
            AdapterRequest::Disable { name } => {
                let row = find(name).ok_or_else(|| missing(name))?;
                items.push(
                    PlannedItem::new(None, name, if row.enabled { "stop" } else { "noop" }).detail(
                        if row.enabled {
                            "the adapter would be stopped and its grant cleared"
                        } else {
                            "already disabled"
                        },
                    ),
                );
            }
            AdapterRequest::Remove { name, purge } => {
                let row = find(name).ok_or_else(|| missing(name))?;
                items.push(PlannedItem::new(None, name, "stop").detail(if row.enabled {
                    "the adapter would be stopped"
                } else {
                    "not running"
                }));
                items.push(
                    PlannedItem::new(None, name, "delete")
                        .detail("the installed files and the row would be deleted"),
                );
                let state = self.ctx()?.paths.adapter_state(name);
                items.push(
                    PlannedItem::new(
                        None,
                        name,
                        if *purge { "delete_state" } else { "keep_state" },
                    )
                    .detail(state.display().to_string()),
                );
            }
            AdapterRequest::Rollback { name } => {
                let row = find(name).ok_or_else(|| missing(name))?;
                match &row.prev_version {
                    Some(prev) => items.push(
                        PlannedItem::new(None, name, "rollback")
                            .detail(format!("{} would go back to {prev}", row.version)),
                    ),
                    None => items
                        .push(PlannedItem::new(None, name, "refuse").detail(
                            "E_ADAPTER_INVALID: there is no earlier version to go back to",
                        )),
                }
            }
            AdapterRequest::Install(args) => {
                items.push(
                    PlannedItem::new(None, &args.staging, "would_import")
                        .detail(format!("source {}", args.source)),
                );
            }
            AdapterRequest::IndexUpdate {
                index_bytes,
                index_sig,
            } => {
                self.plan_index(index_bytes.as_bytes(), index_sig).await?;
                items
                    .push(PlannedItem::new(None, "index", "index_checked").detail(
                        "the index passed verification and its sequence would be recorded",
                    ));
            }
            AdapterRequest::List | AdapterRequest::Info { .. } => {}
            // a relayed verb is rehearsed by the tool itself (`dry_run` in `command.invoke`)
            AdapterRequest::Invoke(_) => {
                return Err(Error::Internal(
                    "a relayed verb has no plan of its own".to_string(),
                ))
            }
            AdapterRequest::SessionOpen(_) => {
                return Err(Error::Internal(
                    "a session has no plan of its own".to_string(),
                ))
            }
        }
        Ok((items, warnings))
    }
}
