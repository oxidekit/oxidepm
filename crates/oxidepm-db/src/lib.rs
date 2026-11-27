#![forbid(unsafe_code)]

//! OxidePM Database - SQLite persistence layer

pub mod adapters;
pub mod apps;
pub mod migrate;
pub mod schema;

use oxidepm_core::constants::LEGACY_DB_FILE;
use oxidepm_core::{Error, Result};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

pub use adapters::{AdapterRepository, AdapterRow, PlanRow};
pub use apps::{AppRecord, AppsRepository};
pub use migrate::{Migration, Migrator};
pub use oxidepm_core::Desired;

/// Database connection and operations
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open (creating if needed) the database at `path` with the current schema.
    pub async fn new(path: &Path) -> Result<Self> {
        Self::open(path, schema::MIGRATIONS).await
    }

    /// Open the database at `path` and migrate it with `migrations`.
    ///
    /// Refuses (`Error::StateIncompatible`) a home that only has the database of an
    /// older OxidePM, a file from a newer one, and a file that is not ours.
    pub async fn open(path: &Path, migrations: &[Migration]) -> Result<Self> {
        Self::open_inner(path, migrations, true).await
    }

    /// Open (creating if needed) the adapter database at `path` with its own migrations. Same
    /// file mode, backup and refusal rules as [`Database::open`]; the check for a database of an
    /// older release belongs to `state.db` and is not made.
    pub async fn open_adapters(path: &Path) -> Result<Self> {
        Self::open_inner(path, schema::ADAPTERS_MIGRATIONS, false).await
    }

    async fn open_inner(path: &Path, migrations: &[Migration], legacy_check: bool) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        if legacy_check {
            check_legacy_file(path)?;
        }
        if !path.try_exists()? {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            // SQLite can leave WAL sidecars at the staging path after the pool closes.
            let staging_dir = tempfile::tempdir_in(parent)?;
            let staged = tempfile::NamedTempFile::new_in(staging_dir.path())?;
            let pool = connect(staged.path()).await?;
            let result = async {
                Migrator::new(migrations).run(&pool, staged.path()).await?;
                migrate::checkpoint(&pool).await
            }
            .await;
            pool.close().await;
            result?;

            match staged.persist_noclobber(path) {
                Ok(_) => {}
                // Another opener published its initialized database first.
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
        }

        precreate_private(path)?;
        let pool = connect(path).await?;
        if let Err(e) = Migrator::new(migrations).run(&pool, path).await {
            pool.close().await;
            return Err(e);
        }

        info!("Database initialized at {}", path.display());
        Ok(Self { pool })
    }

    /// Get the connection pool
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Get apps repository
    pub fn apps(&self) -> AppsRepository {
        AppsRepository::new(self.pool.clone())
    }

    /// Get the adapter repository (meaningful on a database opened with [`Database::open_adapters`])
    pub fn adapters(&self) -> AdapterRepository {
        AdapterRepository::new(self.pool.clone())
    }

    /// Fold the write-ahead log into the main file and close every connection, so the
    /// file alone is a complete copy of the state (what `oxidepm backup` copies).
    pub async fn close(&self) {
        if let Err(e) = migrate::checkpoint(&self.pool).await {
            warn!("WAL checkpoint on close failed: {}", e);
        }
        self.pool.close().await;
    }
}

async fn connect(path: &Path) -> Result<SqlitePool> {
    // No URL string: `?`, `#`, `%` and spaces in the path are fine.
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|e| Error::DbError(format!("cannot open database {}: {}", path.display(), e)))
}

/// `oxidepm.db` next to a missing `state.db` is a home of an older release: its
/// schema cannot be migrated, so say what to do instead of starting empty.
fn check_legacy_file(path: &Path) -> Result<()> {
    let legacy = path.with_file_name(LEGACY_DB_FILE);
    if legacy == path || !legacy.exists() {
        return Ok(());
    }
    if path.exists() {
        warn!(
            "ignoring {} left behind by an older OxidePM (the state is in {})",
            legacy.display(),
            path.display()
        );
        return Ok(());
    }
    Err(Error::StateIncompatible(format!(
        "found {} from an older OxidePM; it is not migrated (the new format stores the full \
         app spec). Move it aside (mv {} {}.old) and re-create your apps",
        legacy.display(),
        legacy.display(),
        legacy.display()
    )))
}

fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| Error::DbError(format!("cannot create {}: {}", dir.display(), e)))
}

/// Create the file with mode 0600 before SQLite sees it (so it is never readable by
/// others, not even for an instant), and tighten an existing one.
fn precreate_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err =
        |e: std::io::Error| Error::DbError(format!("cannot create {}: {}", path.display(), e));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(err)?;
    let mode = std::fs::metadata(path).map_err(err)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(
            "{} had mode {:04o}; restricting it to 0600",
            path.display(),
            mode
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn a_failed_first_migration_removes_only_a_new_database() {
        let dir = tempdir().unwrap();
        let new = dir.path().join("new.db");
        let existing = dir.path().join("existing.db");
        std::fs::File::create(&existing).unwrap();
        let invalid = [Migration {
            version: 1,
            up: "INVALID STATEMENT",
        }];

        assert!(Database::open(&new, &invalid).await.is_err());
        assert!(!new.exists());
        assert!(Database::open(&existing, &invalid).await.is_err());
        assert!(existing.exists());
    }

    #[tokio::test]
    async fn test_database_creation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let db = Database::new(&db_path).await.unwrap();
        assert!(db_path.exists());
        db.close().await;
    }
}
