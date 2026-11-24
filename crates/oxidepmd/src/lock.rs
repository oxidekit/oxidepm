//! Single-instance lock: one daemon per home directory.
//!
//! The lock is an exclusive `flock` on `{home}/oxidepmd.lock`, taken before anything slow
//! (database, resurrect), so daemons that are started at the same moment serialise correctly.
//! The kernel drops it when the process dies, so a crashed daemon leaves no stale lock.

use std::fs::File;
use std::io::{Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use oxidepm_core::{Error, Result};

/// The held lock. Dropping it (or exiting) releases it.
#[derive(Debug)]
pub struct DaemonLock {
    _flock: Flock<File>,
    path: PathBuf,
}

impl DaemonLock {
    /// Take the lock at `path` (created with mode 0600) and write this process's pid into it
    /// for diagnostics. Fails with [`Error::DaemonAlreadyRunning`] when another process holds it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let mut flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => flock,
            Err((_file, Errno::EWOULDBLOCK)) => return Err(Error::DaemonAlreadyRunning),
            Err((_file, errno)) => return Err(Error::IoError(errno.into())),
        };
        // The pid is a courtesy for people reading the file; failing to write it is harmless.
        let _ = flock
            .set_len(0)
            .and_then(|_| flock.rewind())
            .and_then(|_| writeln!(flock, "{}", std::process::id()));
        Ok(Self {
            _flock: flock,
            path: path.to_path_buf(),
        })
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pid recorded in the lock file by the process that holds it, if it can be read.
    pub fn holder_pid(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidepmd.lock");
        let first = DaemonLock::acquire(&path).expect("first lock");
        assert!(matches!(
            DaemonLock::acquire(&path),
            Err(Error::DaemonAlreadyRunning)
        ));
        assert_eq!(DaemonLock::holder_pid(&path), Some(std::process::id()));
        drop(first);
        DaemonLock::acquire(&path).expect("lock again after release");
    }

    #[test]
    fn the_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidepmd.lock");
        let _lock = DaemonLock::acquire(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
