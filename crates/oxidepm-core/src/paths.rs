//! Filesystem layout of an OxidePM home directory.
//!
//! `Paths` is the single place that knows where the daemon keeps its files.
//! Production code builds it with [`Paths::from_env`]; tests build one over a
//! temporary directory with [`Paths::new`], so they never touch the real
//! `~/.oxidepm`.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::constants::{
    ADAPTERS_DB_FILE, ADAPTERS_DIR, ADAPTER_PIDS_FILE, AGENT_SOCKET_FILE, AUDIT_DIR,
    AUDIT_LOG_FILE, AUDIT_SECURITY_FILE, DAEMON_CONFIG_FILE, DAEMON_LOG_FILE, DB_FILE,
    LEGACY_DB_FILE, LOCK_FILE, LOGS_DIR, OXIDEPM_DIR, OXIDEPM_HOME_ENV, PIDS_FILE, SAVED_FILE,
    SOCKET_FILE,
};
use crate::error::{Error, Result};

/// Locations of every file the daemon owns, derived from one home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
}

/// An open, verified OxidePM home directory.
#[derive(Debug)]
pub struct HomeDir {
    file: File,
    path: PathBuf,
    uid: u32,
}

impl Paths {
    /// Paths rooted at an explicit home directory.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Paths rooted at `OXIDEPM_HOME` if it is set and non-empty, otherwise at
    /// `$HOME/.oxidepm`. When neither is available this is an error: files are
    /// never silently put in the current directory.
    pub fn from_env() -> Result<Self> {
        Self::resolve(std::env::var_os(OXIDEPM_HOME_ENV), dirs::home_dir())
    }

    /// The `from_env` rule over explicit inputs (kept separate so it can be
    /// tested without touching process-global environment variables).
    fn resolve(override_dir: Option<OsString>, user_home: Option<PathBuf>) -> Result<Self> {
        match override_dir {
            Some(dir) if !dir.is_empty() => {
                let dir = PathBuf::from(dir);
                if !dir.is_absolute() {
                    return Err(Error::ConfigError(format!(
                        "{OXIDEPM_HOME_ENV} must be an absolute path"
                    )));
                }
                Ok(Self::new(dir))
            }
            _ => user_home
                .map(|h| Self::new(h.join(OXIDEPM_DIR)))
                .ok_or_else(|| {
                    Error::ConfigError(format!(
                        "cannot locate the OxidePM home directory: set {} or HOME",
                        OXIDEPM_HOME_ENV
                    ))
                }),
        }
    }

    /// The home directory itself.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// IPC socket.
    pub fn socket(&self) -> PathBuf {
        self.home.join(SOCKET_FILE)
    }

    /// The agent socket: every connection on it is class `ai`.
    pub fn agent_socket(&self) -> PathBuf {
        self.home.join(AGENT_SOCKET_FILE)
    }

    /// Directory of the audit log. Not created by [`ensure_dirs`](Self::ensure_dirs): the audit
    /// writer creates it when it first writes, so a home whose audit path is unusable still boots.
    pub fn audit_dir(&self) -> PathBuf {
        self.home.join(AUDIT_DIR)
    }

    /// Everything that was recorded (`audit/audit.jsonl`).
    pub fn audit_log(&self) -> PathBuf {
        self.audit_dir().join(AUDIT_LOG_FILE)
    }

    /// Denials, dangerous requests and what non-humans did that changes something
    /// (`audit/audit-security.jsonl`); routine reads cannot rotate it away.
    pub fn audit_security_log(&self) -> PathBuf {
        self.audit_dir().join(AUDIT_SECURITY_FILE)
    }

    /// The adapter database (`adapters.db`): installed adapters, grants, the plan cache. Created by
    /// the first import or enable, never by [`ensure_dirs`](Self::ensure_dirs).
    pub fn adapters_db(&self) -> PathBuf {
        self.home.join(ADAPTERS_DB_FILE)
    }

    /// Directory of everything adapters own. Not created by [`ensure_dirs`](Self::ensure_dirs):
    /// an `adapters` entry that is unusable must not stop a daemon that has no adapters from
    /// booting. [`ensure_adapter_dirs`](Self::ensure_adapter_dirs) makes it when it is first needed.
    pub fn adapters_dir(&self) -> PathBuf {
        self.home.join(ADAPTERS_DIR)
    }

    /// Directory of the cached adapter index (`index.json`, its signature, `fetched_at`).
    pub fn adapter_index(&self) -> PathBuf {
        self.adapters_dir().join("index")
    }

    /// The cached index (`adapters/index/index.json`), re-verified every time it is read.
    pub fn adapter_index_file(&self) -> PathBuf {
        self.adapter_index().join("index.json")
    }

    /// The signature of the cached index.
    pub fn adapter_index_sig(&self) -> PathBuf {
        self.adapter_index().join("index.json.minisig")
    }

    /// Directory of everything installed: `adapters/installed`.
    pub fn adapter_installed_root(&self) -> PathBuf {
        self.adapters_dir().join("installed")
    }

    /// Where an update puts a replaced version until it is deleted: `adapters/trash`.
    pub fn adapter_trash(&self) -> PathBuf {
        self.adapters_dir().join("trash")
    }

    /// Directory of one installed version: `adapters/installed/<name>/<version>`.
    ///
    /// `name` and `version` come from a validated manifest, but this is the one place that turns
    /// them into a path, so a value that is not a single plain path component (empty, `.`, `..`,
    /// a separator, NUL) is replaced by `_invalid_`: the result always stays inside `installed/`.
    pub fn adapter_installed(&self, name: &str, version: &str) -> PathBuf {
        self.adapters_dir()
            .join("installed")
            .join(safe_component(name))
            .join(safe_component(version))
    }

    /// The executable of an installed version. `exec` is the manifest's relative path; every
    /// component that is not a plain name is replaced like in [`adapter_installed`](Self::adapter_installed),
    /// so the path never leaves the version's directory.
    pub fn adapter_exec(&self, name: &str, version: &str, exec: &str) -> PathBuf {
        let mut path = self.adapter_installed(name, version);
        for part in exec.split('/') {
            path.push(safe_component(part));
        }
        path
    }

    /// The writable directory of one adapter (`adapters/state/<name>`): config, data, stderr log.
    pub fn adapter_state(&self, name: &str) -> PathBuf {
        self.adapters_dir().join("state").join(safe_component(name))
    }

    /// Directory in which the CLI makes one private staging directory per import.
    pub fn adapter_staging(&self) -> PathBuf {
        self.adapters_dir().join("staging")
    }

    /// Where the sockets of interactive sessions live (`adapters/sessions`). The daemon empties
    /// it at start; a session socket is never meant to outlive the daemon.
    pub fn adapter_sessions(&self) -> PathBuf {
        self.adapters_dir().join("sessions")
    }

    /// The verbs of the enabled adapters, written by the daemon for the CLI (`adapters/verbs.json`).
    pub fn adapter_verbs(&self) -> PathBuf {
        self.adapters_dir().join("verbs.json")
    }

    /// The process groups of the running adapters, so a restarted daemon can find orphaned processes.
    pub fn adapter_pids(&self) -> PathBuf {
        self.home.join(ADAPTER_PIDS_FILE)
    }

    /// Prepare the private adapter directories on first import or enable. Existing directories
    /// must belong to this user and must deny group and other write access.
    pub fn ensure_adapter_dirs(&self) -> Result<()> {
        self.open_home()?.ensure_adapter_dirs().map(drop)
    }

    /// Create `adapters/sessions`, mode 0700 (and `adapters/` itself if needed). A symbolic link
    /// or a file in the way is an error.
    pub fn ensure_adapter_sessions_dir(&self) -> Result<()> {
        let home = self.open_home()?;
        let adapters = home.ensure_child(ADAPTERS_DIR)?;
        ensure_private_dir_no_link(&adapters, &self.adapters_dir(), "sessions", home.uid).map(drop)
    }

    /// Create the state directory of one adapter (and `adapters/` itself if needed), mode 0700.
    pub fn ensure_adapter_state_dir(&self, name: &str) -> Result<()> {
        let home = self.open_home()?;
        let state = home.ensure_adapter_dirs()?;
        ensure_private_dir_no_link(
            &state,
            &self.adapters_dir().join("state"),
            safe_component(name),
            home.uid,
        )
        .map(drop)
    }

    /// SQLite database (`state.db`).
    pub fn db(&self) -> PathBuf {
        self.home.join(DB_FILE)
    }

    /// Database of releases before schema v1 (`oxidepm.db`); only ever
    /// checked for, never opened.
    pub fn legacy_db(&self) -> PathBuf {
        self.home.join(LEGACY_DB_FILE)
    }

    /// Daemon log: fatal startup errors and the stderr of an auto-started daemon.
    pub fn daemon_log(&self) -> PathBuf {
        self.home.join(DAEMON_LOG_FILE)
    }

    /// Daemon settings (`daemon.toml`); the file is optional.
    pub fn daemon_config(&self) -> PathBuf {
        self.home.join(DAEMON_CONFIG_FILE)
    }

    /// Create the home with mode 0700, or check its ownership and permissions before using it.
    pub fn ensure_home(&self) -> Result<()> {
        self.open_home().map(drop)
    }

    /// Prepare the home and keep its directory open for operations relative to it.
    pub fn open_home(&self) -> Result<HomeDir> {
        // A trailing slash asks the filesystem to follow even a final symlink.
        let path: PathBuf = self.home.components().collect();
        check_home_parent(&path)?;
        let file = match open_dir(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&path)?;
                check_home_parent(&path)?;
                open_dir(&path)?
            }
            result => result?,
        };
        let uid = effective_uid()?;
        check_and_tighten_dir(&file, &path, "OxidePM home", uid)?;
        Ok(HomeDir { file, path, uid })
    }

    /// Prepare the home and `logs/`. Tighten read or search access for group or others to 0700.
    pub fn ensure_dirs(&self) -> Result<()> {
        self.open_home()?.ensure_logs_dir()
    }

    /// `saved.json` written by `save` and read by `resurrect`.
    pub fn saved(&self) -> PathBuf {
        self.home.join(SAVED_FILE)
    }

    /// Daemon single-instance lock file.
    pub fn lock(&self) -> PathBuf {
        self.home.join(LOCK_FILE)
    }

    /// File that records live app processes.
    pub fn pids(&self) -> PathBuf {
        self.home.join(PIDS_FILE)
    }

    /// Directory holding every app log.
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join(LOGS_DIR)
    }

    /// Log file of an app stream (`"out"` or `"err"`).
    pub fn log_path(&self, app_name: &str, stream: &str) -> PathBuf {
        self.logs_dir().join(format!("{}-{}.log", app_name, stream))
    }

    /// Hook output log of an app.
    pub fn hooks_log(&self, app_name: &str) -> PathBuf {
        self.logs_dir().join(format!("{}-hooks.log", app_name))
    }
}

impl HomeDir {
    /// Prepare the private logs directory relative to this open home.
    pub fn ensure_logs_dir(&self) -> Result<()> {
        self.ensure_child(LOGS_DIR).map(drop)
    }

    /// The procfs path to this directory. Valid while this handle stays open.
    pub fn descriptor_path(&self) -> PathBuf {
        descriptor_path(&self.file)
    }

    /// Check that the configured pathname still names this directory before using a path-based API.
    pub fn verify_path(&self) -> Result<()> {
        let current = open_dir(&self.path)?.metadata()?;
        let held = self.file.metadata()?;
        if (current.dev(), current.ino()) != (held.dev(), held.ino()) {
            return Err(Error::ConfigError(format!(
                "OxidePM home {} was replaced while in use",
                self.path.display()
            )));
        }
        check_owner_and_mode(&self.path, "OxidePM home", &current, self.uid)
    }

    fn ensure_child(&self, name: &str) -> Result<File> {
        ensure_private_dir_no_link(&self.file, &self.path, name, self.uid)
    }

    fn ensure_adapter_dirs(&self) -> Result<File> {
        let adapters = self.ensure_child(ADAPTERS_DIR)?;
        let path = self.path.join(ADAPTERS_DIR);
        for name in ["index", "installed", "staging", "trash"] {
            ensure_private_dir_no_link(&adapters, &path, name, self.uid)?;
        }
        ensure_private_dir_no_link(&adapters, &path, "state", self.uid)
    }
}

fn check_home_parent(home: &Path) -> Result<()> {
    // If the parent is missing, check the directory where it would be created.
    for parent in home.ancestors().skip(1) {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let mode = match fs::metadata(parent) {
            Ok(metadata) => metadata.mode(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(Error::ConfigError(format!(
                "OxidePM home {} has a group- or world-writable parent {} without the sticky bit. Use a private parent or remove group and other write access with chmod go-w",
                home.display(),
                parent.display()
            )));
        }
        break;
    }
    Ok(())
}

fn check_owner_and_mode(dir: &Path, label: &str, metadata: &fs::Metadata, uid: u32) -> Result<()> {
    let owner = metadata.uid();
    let mode = metadata.mode();
    let reason = if owner != uid {
        format!("is owned by UID {owner}")
    } else if mode & 0o022 != 0 {
        format!(
            "has group or other write access (mode {:04o})",
            mode & 0o777
        )
    } else {
        return Ok(());
    };
    Err(Error::ConfigError(format!(
        "{label} {} {reason}. Set its owner with chown {uid} and permissions with chmod 700",
        dir.display()
    )))
}

fn effective_uid() -> io::Result<u32> {
    let status = fs::read("/proc/thread-self/status")?;
    status
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))
        .and_then(|uids| {
            // The kernel lists real, effective, saved and filesystem UIDs in that order.
            std::str::from_utf8(uids)
                .ok()?
                .split_ascii_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cannot read the effective UID"))
}

fn open_dir(dir: &Path) -> io::Result<File> {
    // O_PATH also works for directories that the owner can search but cannot read.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
}

fn descriptor_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn check_and_tighten_dir(file: &File, dir: &Path, label: &str, uid: u32) -> Result<()> {
    let metadata = file.metadata()?;
    check_owner_and_mode(dir, label, &metadata, uid)?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            "{} had mode {:04o}; restricting it to 0700",
            dir.display(),
            mode
        );
        fs::set_permissions(descriptor_path(file), fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `part` if it is one plain path component, otherwise `_invalid_`.
fn safe_component(part: &str) -> &str {
    let plain =
        !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\', '\0']);
    if plain {
        part
    } else {
        "_invalid_"
    }
}

fn ensure_private_dir_no_link(parent: &File, path: &Path, name: &str, uid: u32) -> Result<File> {
    let entry = descriptor_path(parent).join(name);
    let dir = match open_dir(&entry) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(&entry) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            open_dir(&entry)?
        }
        result => result?,
    };
    check_and_tighten_dir(&dir, &path.join(name), "OxidePM directory", uid)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_are_rooted_at_home() {
        let p = Paths::new("/x/home");
        assert_eq!(p.home(), Path::new("/x/home"));
        assert_eq!(p.socket(), PathBuf::from("/x/home/daemon.sock"));
        assert_eq!(p.db(), PathBuf::from("/x/home/state.db"));
        assert_eq!(p.legacy_db(), PathBuf::from("/x/home/oxidepm.db"));
        assert_eq!(p.daemon_log(), PathBuf::from("/x/home/daemon.log"));
        assert_eq!(p.saved(), PathBuf::from("/x/home/saved.json"));
        assert_eq!(p.lock(), PathBuf::from("/x/home/oxidepmd.lock"));
        assert_eq!(p.pids(), PathBuf::from("/x/home/pids.json"));
        assert_eq!(p.logs_dir(), PathBuf::from("/x/home/logs"));
        assert_eq!(
            p.log_path("web", "out"),
            PathBuf::from("/x/home/logs/web-out.log")
        );
        assert_eq!(
            p.log_path("web", "err"),
            PathBuf::from("/x/home/logs/web-err.log")
        );
        assert_eq!(
            p.hooks_log("web"),
            PathBuf::from("/x/home/logs/web-hooks.log")
        );
        assert_eq!(p.audit_dir(), PathBuf::from("/x/home/audit"));
        assert_eq!(p.audit_log(), PathBuf::from("/x/home/audit/audit.jsonl"));
        assert_eq!(
            p.audit_security_log(),
            PathBuf::from("/x/home/audit/audit-security.jsonl")
        );
    }

    #[test]
    fn paths_for_adapters_are_under_home() {
        let p = Paths::new("/x/home");
        assert_eq!(p.adapters_dir(), PathBuf::from("/x/home/adapters"));
        assert_eq!(p.adapters_db(), PathBuf::from("/x/home/adapters.db"));
        assert_eq!(p.adapter_index(), PathBuf::from("/x/home/adapters/index"));
        assert_eq!(
            p.adapter_installed("watch", "1.2.3"),
            PathBuf::from("/x/home/adapters/installed/watch/1.2.3")
        );
        assert_eq!(
            p.adapter_exec("watch", "1.2.3", "bin/watch"),
            PathBuf::from("/x/home/adapters/installed/watch/1.2.3/bin/watch")
        );
        assert_eq!(
            p.adapter_state("watch"),
            PathBuf::from("/x/home/adapters/state/watch")
        );
        assert_eq!(
            p.adapter_staging(),
            PathBuf::from("/x/home/adapters/staging")
        );
        assert_eq!(p.adapter_pids(), PathBuf::from("/x/home/adapter-pids.json"));
        assert_eq!(p.agent_socket(), PathBuf::from("/x/home/agent.sock"));
        assert_eq!(p.audit_dir(), PathBuf::from("/x/home/audit"));

        // A name, a version or an executable path that is not one plain component each can never
        // lead out of the tree it belongs to, whatever the caller forgot to validate.
        let installed = p.adapters_dir().join("installed");
        let state = p.adapters_dir().join("state");
        for bad in ["..", ".", "", "a/b", "../x", "/etc", "a\0b"] {
            let path = p.adapter_installed(bad, "1.0.0");
            assert!(path.starts_with(&installed), "{bad:?} -> {path:?}");
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "{path:?}"
            );
            let path = p.adapter_installed("watch", bad);
            assert!(
                path.starts_with(installed.join("watch")),
                "{bad:?} -> {path:?}"
            );
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "{path:?}"
            );
            let path = p.adapter_state(bad);
            assert!(path.starts_with(&state), "{bad:?} -> {path:?}");
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "{path:?}"
            );
        }
        let base = p.adapter_installed("watch", "1.0.0");
        for bad in ["../x", "/bin/sh", "a/../../b", "./x", "a//b", ""] {
            let path = p.adapter_exec("watch", "1.0.0", bad);
            assert!(path.starts_with(&base), "{bad:?} -> {path:?}");
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "{path:?}"
            );
        }
    }

    #[test]
    fn ensure_dirs_does_not_create_adapter_or_audit_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("home"));
        p.ensure_dirs().unwrap();
        assert!(!p.audit_dir().exists());
        assert!(!p.adapters_dir().exists());
        assert!(!p.adapters_db().exists());
        assert!(!p.adapter_pids().exists());
        assert!(!p.agent_socket().exists());
        let mut entries: Vec<String> = std::fs::read_dir(p.home())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["logs"]);

        // A file where the adapter directory would go does not stop the daemon from booting.
        std::fs::write(p.adapters_dir(), b"x").unwrap();
        std::fs::write(p.audit_dir(), b"x").unwrap();
        p.ensure_dirs().unwrap();
    }

    #[test]
    fn adapter_dirs_are_created_lazily_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("home"));
        p.ensure_dirs().unwrap();
        assert!(!p.adapters_dir().exists());

        p.ensure_adapter_dirs().unwrap();
        for d in [
            p.adapters_dir(),
            p.adapter_index(),
            p.adapters_dir().join("installed"),
            p.adapters_dir().join("state"),
            p.adapter_staging(),
        ] {
            assert!(d.is_dir(), "{d:?}");
            assert_eq!(mode_of(&d), 0o700, "{d:?}");
        }
        // Running it again changes nothing; a loosened directory is tightened.
        std::fs::set_permissions(p.adapter_staging(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        p.ensure_adapter_dirs().unwrap();
        assert_eq!(mode_of(&p.adapter_staging()), 0o700);

        // The per-adapter state directory is made on its own, private too.
        p.ensure_adapter_state_dir("watch").unwrap();
        assert!(p.adapter_state("watch").is_dir());
        assert_eq!(mode_of(&p.adapter_state("watch")), 0o700);
    }

    #[test]
    fn adapter_dirs_refuse_a_symbolic_link_and_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("home"));
        p.ensure_dirs().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, p.adapters_dir()).unwrap();
        assert!(p.ensure_adapter_dirs().is_err());
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing was created through the link"
        );
        std::fs::remove_file(p.adapters_dir()).unwrap();
        std::fs::write(p.adapters_dir(), b"x").unwrap();
        assert!(p.ensure_adapter_dirs().is_err());
    }

    #[test]
    fn env_override_wins_over_user_home() {
        let p = Paths::resolve(Some("/tmp/custom".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p.home(), Path::new("/tmp/custom"));
        // The override alone is enough: no user home is needed.
        let p = Paths::resolve(Some("/tmp/custom".into()), None).unwrap();
        assert_eq!(p.home(), Path::new("/tmp/custom"));
    }

    #[test]
    fn relative_home_override_is_rejected() {
        assert!(Paths::resolve(Some("relative".into()), Some("/home/u".into())).is_err());
    }

    #[test]
    fn empty_override_falls_back_to_user_home() {
        let p = Paths::resolve(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p.home(), Path::new("/home/u/.oxidepm"));
        let p = Paths::resolve(None, Some("/home/u".into())).unwrap();
        assert_eq!(p.home(), Path::new("/home/u/.oxidepm"));
    }

    #[test]
    fn no_home_at_all_is_an_error() {
        for override_dir in [None, Some(OsString::new())] {
            let e = Paths::resolve(override_dir, None).unwrap_err().to_string();
            assert!(e.contains("OXIDEPM_HOME") && e.contains("HOME"), "{e}");
        }
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_dirs_creates_home_and_logs_private() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("a").join("home"));
        paths.ensure_dirs().unwrap();
        assert_eq!(mode_of(paths.home()), 0o700);
        assert_eq!(mode_of(&paths.logs_dir()), 0o700);
        paths.ensure_dirs().unwrap();
    }

    #[test]
    fn ensure_dirs_tightens_loose_existing_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        for d in [paths.home().to_path_buf(), paths.logs_dir()] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        paths.ensure_dirs().unwrap();
        assert_eq!(mode_of(paths.home()), 0o700);
        assert_eq!(mode_of(&paths.logs_dir()), 0o700);
    }

    #[test]
    fn refuses_a_group_writable_logs_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path());
        let logs = paths.logs_dir();
        fs::create_dir(&logs).unwrap();
        fs::set_permissions(&logs, fs::Permissions::from_mode(0o770)).unwrap();

        assert!(paths.ensure_dirs().is_err());
        assert_eq!(mode_of(&logs), 0o770);
    }

    #[test]
    fn refuses_a_world_writable_adapter_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path());
        paths.ensure_adapter_dirs().unwrap();
        let staging = paths.adapter_staging();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o707)).unwrap();

        assert!(paths.ensure_adapter_dirs().is_err());
        assert_eq!(mode_of(&staging), 0o707);
    }

    #[test]
    fn refuses_a_home_owned_by_another_user() {
        let tmp = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(tmp.path()).unwrap();
        let uid = metadata.uid().wrapping_add(1);

        let error = check_owner_and_mode(tmp.path(), "OxidePM home", &metadata, uid)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&tmp.path().display().to_string()), "{error}");
        assert!(error.contains(&format!("chown {uid}")), "{error}");
        assert!(error.contains("chmod 700"), "{error}");
    }

    #[test]
    fn refuses_a_group_writable_home() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        std::fs::create_dir(paths.home()).unwrap();
        std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o770)).unwrap();

        let error = paths.ensure_dirs().unwrap_err().to_string();
        assert!(
            error.contains(&paths.home().display().to_string()),
            "{error}"
        );
        assert!(
            error.contains("chown") && error.contains("chmod 700"),
            "{error}"
        );
        assert_eq!(mode_of(paths.home()), 0o770);
        assert!(!paths.logs_dir().exists());
    }

    #[test]
    fn refuses_a_world_writable_home_when_creating_adapter_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        std::fs::create_dir(paths.home()).unwrap();
        std::fs::set_permissions(paths.home(), std::fs::Permissions::from_mode(0o707)).unwrap();

        let error = paths.ensure_adapter_dirs().unwrap_err().to_string();
        assert!(
            error.contains(&paths.home().display().to_string()),
            "{error}"
        );
        assert!(error.contains("chmod 700"), "{error}");
        assert_eq!(mode_of(paths.home()), 0o707);
        assert!(!paths.adapters_dir().exists());
    }

    #[test]
    fn refuses_a_writable_parent_without_the_sticky_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let paths = Paths::new(parent.join("home"));
        for mode in [0o770, 0o707] {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = paths.ensure_dirs().unwrap_err().to_string();
            assert!(error.contains(&parent.display().to_string()), "{error}");
            assert!(error.contains("chmod go-w"), "{error}");
            assert!(!paths.home().exists());
        }
    }

    #[test]
    fn accepts_a_sticky_writable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let paths = Paths::new(parent.join("home"));
        paths.ensure_dirs().unwrap();
        assert_eq!(mode_of(paths.home()), 0o700);
        paths.ensure_dirs().unwrap();
    }

    #[test]
    fn ensure_dirs_refuses_home_and_log_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let paths = Paths::new(tmp.path().join("home"));
        symlink(&target, paths.home()).unwrap();
        let home_result = paths.ensure_dirs();
        std::fs::remove_file(paths.home()).unwrap();
        std::fs::create_dir(paths.home()).unwrap();
        symlink(&target, paths.logs_dir()).unwrap();
        let logs_result = paths.ensure_dirs();

        assert!(home_result.is_err());
        assert!(logs_result.is_err());
        assert_eq!(mode_of(&target), 0o755);
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[test]
    fn trailing_slash_does_not_bypass_the_home_symlink_check() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let link = tmp.path().join("link");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        let mut with_slash = link.into_os_string();
        with_slash.push("/");
        assert!(Paths::new(with_slash).ensure_dirs().is_err());
        assert!(!target.join("logs").exists());
    }

    #[test]
    fn ensure_dirs_refuses_a_file_in_the_way() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::write(&home, b"x").unwrap();
        assert!(Paths::new(home).ensure_dirs().is_err());
    }
}
