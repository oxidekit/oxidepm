//! Metrics repository - CPU/memory tracking

use oxidepm_core::{Error, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// Repository for metrics operations
pub struct MetricsRepository {
    pool: SqlitePool,
}

impl MetricsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a metrics snapshot
    pub async fn insert(&self, app_id: u32, cpu_percent: f32, memory_bytes: u64) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO metrics (app_id, cpu_percent, memory_bytes)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(app_id as i64)
        .bind(cpu_percent as f64)
        .bind(memory_bytes as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(())
    }

    /// Get latest metrics for an app
    pub async fn get_latest(&self, app_id: u32) -> Result<Option<MetricsSnapshot>> {
        let row = sqlx::query(
            r#"
            SELECT cpu_percent, memory_bytes, timestamp
            FROM metrics
            WHERE app_id = ?
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .bind(app_id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        match row {
            Some(row) => {
                let cpu: f64 = row.get("cpu_percent");
                let mem: i64 = row.get("memory_bytes");
                let ts: String = row.get("timestamp");
                Ok(Some(MetricsSnapshot {
                    cpu_percent: cpu as f32,
                    memory_bytes: mem as u64,
                    timestamp: ts,
                }))
            }
            None => Ok(None),
        }
    }

    /// Get metrics history for an app
    pub async fn get_history(
        &self,
        app_id: u32,
        limit: usize,
    ) -> Result<Vec<MetricsSnapshot>> {
        let rows = sqlx::query(
            r#"
            SELECT cpu_percent, memory_bytes, timestamp
            FROM metrics
            WHERE app_id = ?
            ORDER BY id DESC
            LIMIT ?
            "#,
        )
        .bind(app_id as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(rows
            .iter()
            .map(|row| {
                let cpu: f64 = row.get("cpu_percent");
                let mem: i64 = row.get("memory_bytes");
                let ts: String = row.get("timestamp");
                MetricsSnapshot {
                    cpu_percent: cpu as f32,
                    memory_bytes: mem as u64,
                    timestamp: ts,
                }
            })
            .collect())
    }

    /// Cleanup old metrics (keep last N per app)
    pub async fn cleanup(&self, keep_per_app: usize) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM metrics
            WHERE id NOT IN (
                SELECT id FROM (
                    SELECT id, ROW_NUMBER() OVER (PARTITION BY app_id ORDER BY id DESC) as rn
                    FROM metrics
                ) WHERE rn <= ?
            )
            "#,
        )
        .bind(keep_per_app as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected())
    }

    /// Delete all metrics for an app
    pub async fn delete_by_app(&self, app_id: u32) -> Result<u64> {
        let result = sqlx::query("DELETE FROM metrics WHERE app_id = ?")
            .bind(app_id as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected())
    }
}

/// Metrics snapshot
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use oxidepm_core::{AppMode, AppSpec};
    use std::path::PathBuf;
    use tempfile::{tempdir, TempDir};

    // Return TempDir to keep it alive during test
    async fn setup_db_with_app() -> (Database, u32, TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::new(&db_path).await.unwrap();

        let spec = AppSpec::new(
            "test".to_string(),
            AppMode::Node,
            "app.js".to_string(),
            PathBuf::from("/"),
        );
        let app_id = db.apps().insert(&spec).await.unwrap();

        (db, app_id, dir)
    }

    #[tokio::test]
    async fn test_insert_and_get_metrics() {
        let (db, app_id, _dir) = setup_db_with_app().await;
        let metrics = MetricsRepository::new(db.pool().clone());

        metrics.insert(app_id, 25.5, 1024 * 1024).await.unwrap();

        let latest = metrics.get_latest(app_id).await.unwrap();
        assert!(latest.is_some());
        let latest = latest.unwrap();
        assert!((latest.cpu_percent - 25.5).abs() < 0.1);
        assert_eq!(latest.memory_bytes, 1024 * 1024);
    }
}
