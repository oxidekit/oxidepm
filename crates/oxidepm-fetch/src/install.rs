//! Atomic installation of a file, also over an executable that is running.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{FetchError, Result};

fn dir_of(dst: &Path) -> &Path {
    dst.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_stage(dir: &Path, name: &std::ffi::OsStr) -> io::Result<tempfile::NamedTempFile> {
    let mut staging_name = name.to_os_string();
    staging_name.push(".new");
    let preferred = dir.join(staging_name);
    let preferred = if preferred.is_absolute() {
        preferred
    } else {
        std::env::current_dir()?.join(preferred)
    };
    // The conventional staging path can be reserved only by its creator. Another staged copy
    // gets a separate file and leaves the existing entry alone.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&preferred)
    {
        Ok(file) => Ok(tempfile::NamedTempFile::from_parts(
            file,
            tempfile::TempPath::try_from_path(preferred)?,
        )),
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists
                && !fs::symlink_metadata(&preferred).is_ok_and(|meta| meta.is_dir()) =>
        {
            tempfile::Builder::new()
                .prefix(".oxidepm-stage-")
                .tempfile_in(dir)
        }
        Err(e) => Err(e),
    }
}

/// A copy of a file, complete and on disk next to its destination, waiting for
/// [`commit`](StagedFile::commit). Dropping it without committing removes the copy. Staging
/// several files first and committing them afterwards means a failure while writing never
/// leaves some of the destinations replaced and others not.
#[derive(Debug)]
pub struct StagedFile {
    new: tempfile::TempPath,
    dst: PathBuf,
}

impl StagedFile {
    /// Write a unique temporary file in the directory of `dst` from `src` and sync it.
    pub fn stage(src: &Path, dst: &Path, mode: u32) -> Result<Self> {
        let name = dst.file_name().ok_or_else(|| FetchError::Unsafe {
            path: dst.into(),
            reason: "has no file name".into(),
        })?;
        let mut input =
            File::open(src).map_err(|e| FetchError::io(format!("reading {}", src.display()), e))?;
        let dir = dir_of(dst);
        let mut output = create_stage(dir, name).map_err(|e| {
            FetchError::io(format!("creating a staging file in {}", dir.display()), e)
        })?;
        io::copy(&mut input, output.as_file_mut())
            .map_err(|e| FetchError::io(format!("writing {}", output.path().display()), e))?;
        output
            .as_file()
            .set_permissions(Permissions::from_mode(mode))
            .map_err(|e| {
                FetchError::io(
                    format!("setting the mode of {}", output.path().display()),
                    e,
                )
            })?;
        output
            .as_file()
            .sync_all()
            .map_err(|e| FetchError::io(format!("syncing {}", output.path().display()), e))?;
        Ok(Self {
            new: output.into_temp_path(),
            dst: dst.to_path_buf(),
        })
    }

    /// The staged copy.
    pub fn staged_path(&self) -> &Path {
        &self.new
    }

    pub fn destination(&self) -> &Path {
        &self.dst
    }

    /// `rename` the copy over the destination (atomic; it works on a running executable) and
    /// sync the directory. A sync error after the rename leaves the new file in place.
    pub fn commit(self) -> Result<()> {
        let dir = dir_of(&self.dst);
        let directory =
            File::open(dir).map_err(|e| FetchError::io(format!("opening {}", dir.display()), e))?;
        self.new.persist(&self.dst).map_err(|e| {
            FetchError::io(
                format!("renaming {} to {}", e.path.display(), self.dst.display()),
                e.error,
            )
        })?;
        directory
            .sync_all()
            .map_err(|e| FetchError::io(format!("syncing {}", dir.display()), e))
    }
}

/// Replace `dst` by a copy of `src`: write a temporary file in the same directory, `fsync`, `rename`
/// over the destination, `fsync` the directory. A reader sees the old file or the new one, never
/// half of it. The rename works on a running executable on Linux and macOS; copying onto it
/// (`fs::copy`) fails with `ETXTBSY`.
pub fn install_file_atomic(src: &Path, dst: &Path, mode: u32) -> Result<()> {
    StagedFile::stage(src, dst, mode)?.commit()
}
