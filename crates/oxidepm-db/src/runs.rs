//! Runs repository - execution history tracking

use oxidepm_core::{AppStatus, Error, Result, RunState};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// Repository for run history operations
pub struct RunsRepository {
    pool: SqlitePool,
}

impl RunsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new run record
    pub async fn insert(&self, app_id: u32, state: &RunState) -> Result<u32> {
        let result = sqlx::query(
            r#"
            INSERT INTO runs (app_id, pid, status, restarts)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(app_id as i64)
        .bind(state.pid.map(|p| p as i64))
        .bind(state.status.as_str())
        .bind(state.restarts as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.last_insert_rowid() as u32)
    }

    /// Update run status
    pub async fn update_status(&self, run_id: u32, status: AppStatus) -> Result<()> {
        sqlx::query("UPDATE runs SET status = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(run_id as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(())
    }

    /// Update run on stop
    pub async fn update_stop(&self, run_id: u32, exit_code: Option<i32>) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE runs
            SET status = 'stopped',
                stop_time = CURRENT_TIMESTAMP,
                exit_code = ?
            WHERE id = ?
            "#,
        )
        .bind(exit_code)
        .bind(run_id as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(())
    }

    /// Increment restart count
    pub async fn increment_restarts(&self, run_id: u32) -> Result<()> {
        sqlx::query("UPDATE runs SET restarts = restarts + 1 WHERE id = ?")
            .bind(run_id as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(())
    }

    /// Get latest run for an app
    pub async fn get_latest(&self, app_id: u32) -> Result<Option<RunRecord>> {
        let row = sqlx::query(
            r#"
            SELECT id, app_id, pid, status, restarts, start_time, stop_time, exit_code
            FROM runs
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
            Some(row) => Ok(Some(row_to_run_record(&row)?)),
            None => Ok(None),
        }
    }

    /// Get all runs for an app
    pub async fn get_by_app(&self, app_id: u32, limit: usize) -> Result<Vec<RunRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT id, app_id, pid, status, restarts, start_time, stop_time, exit_code
            FROM runs
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

        rows.iter().map(row_to_run_record).collect()
    }

    /// Delete all runs for an app
    pub async fn delete_by_app(&self, app_id: u32) -> Result<u64> {
        let result = sqlx::query("DELETE FROM runs WHERE app_id = ?")
            .bind(app_id as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected())
    }
}

/// Run record from database
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: u32,
    pub app_id: u32,
    pub pid: Option<u32>,
    pub status: AppStatus,
    pub restarts: u32,
    pub start_time: String,
    pub stop_time: Option<String>,
    pub exit_code: Option<i32>,
}

fn row_to_run_record(row: &sqlx::sqlite::SqliteRow) -> Result<RunRecord> {
    let id: i64 = row.get("id");
    let app_id: i64 = row.get("app_id");
    let pid: Option<i64> = row.get("pid");
    let status_str: String = row.get("status");
    let restarts: i64 = row.get("restarts");
    let start_time: String = row.get("start_time");
    let stop_time: Option<String> = row.get("stop_time");
    let exit_code: Option<i32> = row.get("exit_code");

    let status: AppStatus = status_str.parse()?;

    Ok(RunRecord {
        id: id as u32,
        app_id: app_id as u32,
        pid: pid.map(|p| p as u32),
        status,
        restarts: restarts as u32,
        start_time,
        stop_time,
        exit_code,
    })
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
    async fn test_insert_and_get_run() {
        let (db, app_id, _dir) = setup_db_with_app().await;
        let runs = db.runs();

        let state = RunState::running(app_id, 12345);
        let run_id = runs.insert(app_id, &state).await.unwrap();

        let latest = runs.get_latest(app_id).await.unwrap();
        assert!(latest.is_some());
        let latest = latest.unwrap();
        assert_eq!(latest.id, run_id);
        assert_eq!(latest.pid, Some(12345));
        assert_eq!(latest.status, AppStatus::Running);
    }

    #[tokio::test]
    async fn test_update_stop() {
        let (db, app_id, _dir) = setup_db_with_app().await;
        let runs = db.runs();

        let state = RunState::running(app_id, 12345);
        let run_id = runs.insert(app_id, &state).await.unwrap();

        runs.update_stop(run_id, Some(0)).await.unwrap();

        let latest = runs.get_latest(app_id).await.unwrap().unwrap();
        assert_eq!(latest.status, AppStatus::Stopped);
        assert_eq!(latest.exit_code, Some(0));
    }
}
