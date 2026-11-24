//! Schema versioning with `PRAGMA user_version`.

use std::path::{Path, PathBuf};

use oxidepm_core::{Error, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tracing::info;

/// One schema step. `up` may hold several statements.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub up: &'static str,
}

/// Applies a list of migrations to a database file.
///
/// The list is a parameter so a test can run a synthetic two-step list without
/// waiting for a real second migration.
pub struct Migrator<'a> {
    migrations: &'a [Migration],
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::DbError(e.to_string())
}

impl<'a> Migrator<'a> {
    pub fn new(migrations: &'a [Migration]) -> Self {
        Self { migrations }
    }

    fn latest(&self) -> u32 {
        self.migrations.last().map(|m| m.version).unwrap_or(0)
    }

    /// Bring the database behind `pool` (the file at `path`) up to the latest version.
    ///
    /// - A fresh file gets every migration.
    /// - A file written by a newer OxidePM, or one that has tables but no version, is refused.
    /// - Before any migration touches an existing database its file is copied to
    ///   `<path>.pre-v<first version applied>`, so a failed migration cannot lose the
    ///   process list. Each migration runs in its own transaction that also sets the
    ///   version, so a failure leaves the previous version and all rows.
    pub async fn run(&self, pool: &SqlitePool, path: &Path) -> Result<()> {
        let current = user_version(pool).await?;
        let latest = self.latest();

        if current > latest {
            return Err(Error::StateIncompatible(format!(
                "database {} is from a newer OxidePM (schema version {}, this build knows {}); \
                 rolling the binary back means moving it aside (mv {} {}.new)",
                path.display(),
                current,
                latest,
                path.display(),
                path.display()
            )));
        }

        if current == 0 && has_tables(pool).await? {
            return Err(Error::StateIncompatible(format!(
                "{} is not a {} written by this OxidePM (it has tables but no schema \
                 version). Move it aside (mv {} {}.old) and re-create your apps",
                path.display(),
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "state.db".into()),
                path.display(),
                path.display()
            )));
        }

        let pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| m.version > current)
            .collect();
        let Some(first) = pending.first() else {
            return Ok(());
        };

        if current > 0 {
            backup_before(pool, path, first.version).await?;
        }

        for m in pending {
            let mut tx = pool.begin().await.map_err(db_err)?;
            sqlx::query(m.up)
                .execute(&mut *tx)
                .await
                .map_err(|e| db_err(format!("migration to v{} failed: {}", m.version, e)))?;
            sqlx::query(&format!("PRAGMA user_version = {}", m.version))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            info!("Database {} migrated to v{}", path.display(), m.version);
        }
        Ok(())
    }
}

/// Current `PRAGMA user_version`.
pub async fn user_version(pool: &SqlitePool) -> Result<u32> {
    let row = sqlx::query("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    let v: i64 = row.get(0);
    Ok(v as u32)
}

async fn has_tables(pool: &SqlitePool) -> Result<bool> {
    let row = sqlx::query(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    let n: i64 = row.get(0);
    Ok(n > 0)
}

/// Path of the pre-migration copy for a migration that goes to `version`.
pub fn backup_path(path: &Path, version: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".pre-v{}", version));
    PathBuf::from(name)
}

/// Ensure all WAL frames have been written to the database file.
pub(crate) async fn checkpoint(pool: &SqlitePool) -> Result<()> {
    let (busy, frames, written) =
        sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    if busy != 0 || frames != written {
        return Err(db_err(format!(
            "WAL checkpoint incomplete ({written} of {frames} frames; busy={busy})"
        )));
    }
    Ok(())
}

/// Checkpoint the WAL and copy the database file (mode 0600).
async fn backup_before(pool: &SqlitePool, path: &Path, version: u32) -> Result<()> {
    checkpoint(pool).await?;

    let target = backup_path(path, version);
    copy_private(path, &target).map_err(|e| {
        db_err(format!(
            "cannot back up {} to {}: {}",
            path.display(),
            target.display(),
            e
        ))
    })?;
    info!("Database backed up to {}", target.display());
    Ok(())
}

fn copy_private(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let parent = to
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut dst = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut src, dst.as_file_mut())?;
    dst.as_file().sync_all()?;
    dst.persist(to)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::time::Duration;

    #[tokio::test]
    async fn a_busy_checkpoint_cannot_publish_a_migration_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(Duration::ZERO),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE test (value INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let mut reader = pool.begin().await.unwrap();
        sqlx::query("SELECT * FROM test")
            .fetch_all(&mut *reader)
            .await
            .unwrap();
        sqlx::query("INSERT INTO test VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();

        let result = backup_before(&pool, &path, 2).await;
        reader.rollback().await.unwrap();
        assert!(result.is_err());
        assert!(!backup_path(&path, 2).exists());
        backup_before(&pool, &path, 2).await.unwrap();
        assert!(backup_path(&path, 2).exists());
    }

    #[test]
    fn a_failed_copy_preserves_the_previous_backup() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("state.db.pre-v2");
        std::fs::write(&backup, b"previous backup").unwrap();

        // A directory can be opened, but its contents cannot be copied through read().
        assert!(copy_private(dir.path(), &backup).is_err());
        assert_eq!(std::fs::read(&backup).unwrap(), b"previous backup");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
