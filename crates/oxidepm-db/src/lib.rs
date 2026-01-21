//! OxidePM Database - SQLite persistence layer

pub mod apps;
pub mod metrics;
pub mod runs;
pub mod schema;

use oxidepm_core::{Error, Result};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;
use tracing::info;

pub use apps::AppsRepository;
pub use runs::RunsRepository;

/// Database connection and operations
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Create a new database connection
    pub async fn new(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::DbError(e.to_string()))?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        info!("Connecting to database: {}", url);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        // Set database file permissions to owner-only (0600) for security
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("Failed to set database file permissions: {}", e);
            }
        }

        // Initialize schema
        sqlx::query(schema::SCHEMA)
            .execute(&pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        info!("Database initialized");
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

    /// Get runs repository
    pub fn runs(&self) -> RunsRepository {
        RunsRepository::new(self.pool.clone())
    }

    /// Close the database connection
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_database_creation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let db = Database::new(&db_path).await.unwrap();
        assert!(db_path.exists());
        db.close().await;
    }
}
