//! Apps repository - CRUD operations for applications

use oxidepm_core::{AppMode, AppSpec, Error, RestartPolicy, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;

/// Repository for app operations
pub struct AppsRepository {
    pool: SqlitePool,
}

impl AppsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new app
    pub async fn insert(&self, spec: &AppSpec) -> Result<u32> {
        let args_json = serde_json::to_string(&spec.args)?;
        let env_json = serde_json::to_string(&spec.env)?;
        let ignore_json = serde_json::to_string(&spec.ignore_patterns)?;

        let result = sqlx::query(
            r#"
            INSERT INTO apps (
                name, mode, command, args, cwd, env, watch, ignore_patterns,
                auto_restart, max_restarts, restart_delay_ms, crash_window_secs, kill_timeout_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&spec.name)
        .bind(spec.mode.as_str())
        .bind(&spec.command)
        .bind(&args_json)
        .bind(spec.cwd.to_string_lossy().to_string())
        .bind(&env_json)
        .bind(spec.watch)
        .bind(&ignore_json)
        .bind(spec.restart_policy.auto_restart)
        .bind(spec.restart_policy.max_restarts as i64)
        .bind(spec.restart_policy.restart_delay_ms as i64)
        .bind(spec.restart_policy.crash_window_secs as i64)
        .bind(spec.kill_timeout_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.last_insert_rowid() as u32)
    }

    /// Get app by ID
    pub async fn get_by_id(&self, id: u32) -> Result<Option<AppSpec>> {
        let row = sqlx::query(
            r#"
            SELECT id, name, mode, command, args, cwd, env, watch, ignore_patterns,
                   auto_restart, max_restarts, restart_delay_ms, crash_window_secs,
                   kill_timeout_ms, created_at
            FROM apps WHERE id = ?
            "#,
        )
        .bind(id as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        match row {
            Some(row) => Ok(Some(row_to_app_spec(&row)?)),
            None => Ok(None),
        }
    }

    /// Get app by name
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AppSpec>> {
        let row = sqlx::query(
            r#"
            SELECT id, name, mode, command, args, cwd, env, watch, ignore_patterns,
                   auto_restart, max_restarts, restart_delay_ms, crash_window_secs,
                   kill_timeout_ms, created_at
            FROM apps WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        match row {
            Some(row) => Ok(Some(row_to_app_spec(&row)?)),
            None => Ok(None),
        }
    }

    /// Get all apps
    pub async fn get_all(&self) -> Result<Vec<AppSpec>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, mode, command, args, cwd, env, watch, ignore_patterns,
                   auto_restart, max_restarts, restart_delay_ms, crash_window_secs,
                   kill_timeout_ms, created_at
            FROM apps ORDER BY id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::DbError(e.to_string()))?;

        rows.iter().map(row_to_app_spec).collect()
    }

    /// Delete app by ID
    pub async fn delete(&self, id: u32) -> Result<bool> {
        let result = sqlx::query("DELETE FROM apps WHERE id = ?")
            .bind(id as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete app by name
    pub async fn delete_by_name(&self, name: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM apps WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete all apps
    pub async fn delete_all(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM apps")
            .execute(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(result.rows_affected())
    }

    /// Check if app exists by name
    pub async fn exists(&self, name: &str) -> Result<bool> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apps WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(row.0 > 0)
    }

    /// Get next available ID
    pub async fn next_id(&self) -> Result<u32> {
        let row: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(id), 0) + 1 FROM apps")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::DbError(e.to_string()))?;

        Ok(row.0 as u32)
    }
}

fn row_to_app_spec(row: &sqlx::sqlite::SqliteRow) -> Result<AppSpec> {
    let id: i64 = row.get("id");
    let name: String = row.get("name");
    let mode_str: String = row.get("mode");
    let command: String = row.get("command");
    let args_json: String = row.get("args");
    let cwd_str: String = row.get("cwd");
    let env_json: String = row.get("env");
    let watch: bool = row.get("watch");
    let ignore_json: String = row.get("ignore_patterns");
    let auto_restart: bool = row.get("auto_restart");
    let max_restarts: i64 = row.get("max_restarts");
    let restart_delay_ms: i64 = row.get("restart_delay_ms");
    let crash_window_secs: i64 = row.get("crash_window_secs");
    let kill_timeout_ms: i64 = row.get("kill_timeout_ms");
    let created_at_str: String = row.get("created_at");

    let mode: AppMode = mode_str.parse()?;
    let args: Vec<String> = serde_json::from_str(&args_json)?;
    let env: HashMap<String, String> = serde_json::from_str(&env_json)?;
    let ignore_patterns: Vec<String> = serde_json::from_str(&ignore_json)?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());

    Ok(AppSpec {
        id: id as u32,
        name,
        mode,
        command,
        args,
        cwd: PathBuf::from(cwd_str),
        env,
        watch,
        ignore_patterns,
        restart_policy: RestartPolicy {
            auto_restart,
            max_restarts: max_restarts as u32,
            restart_delay_ms: restart_delay_ms as u64,
            crash_window_secs: crash_window_secs as u64,
        },
        kill_timeout_ms: kill_timeout_ms as u64,
        created_at,
        // Clustering fields (defaults - not persisted in DB yet)
        instances: 1,
        instance_id: None,
        // Port management fields
        port: None,
        port_range: None,
        // Health check field
        health_check: None,
        // Memory limit field
        max_memory_mb: None,
        // Startup delay (defaults - not persisted in DB yet)
        startup_delay_ms: None,
        // Environment inheritance (defaults - not persisted in DB yet)
        env_inherit: false,
        // Event hooks (defaults - not persisted in DB yet)
        hooks: oxidepm_core::Hooks::default(),
        // Process tags (defaults - not persisted in DB yet)
        tags: Vec::new(),
        // Maximum uptime (defaults - not persisted in DB yet)
        max_uptime_secs: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use tempfile::{tempdir, TempDir};

    // Return both Database and TempDir to keep the directory alive
    async fn setup_db() -> (Database, TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::new(&db_path).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn test_insert_and_get() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "test-app".to_string(),
            AppMode::Node,
            "server.js".to_string(),
            PathBuf::from("/app"),
        );

        let id = apps.insert(&spec).await.unwrap();
        assert!(id > 0);

        let retrieved = apps.get_by_id(id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.name, "test-app");
        assert_eq!(retrieved.mode, AppMode::Node);
    }

    #[tokio::test]
    async fn test_get_by_name() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "my-app".to_string(),
            AppMode::Cargo,
            "main".to_string(),
            PathBuf::from("/project"),
        );

        apps.insert(&spec).await.unwrap();

        let retrieved = apps.get_by_name("my-app").await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().mode, AppMode::Cargo);

        let not_found = apps.get_by_name("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_delete() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "to-delete".to_string(),
            AppMode::Node,
            "app.js".to_string(),
            PathBuf::from("/"),
        );

        let id = apps.insert(&spec).await.unwrap();
        assert!(apps.delete(id).await.unwrap());
        assert!(apps.get_by_id(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_all() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        for i in 0..3 {
            let spec = AppSpec::new(
                format!("app-{}", i),
                AppMode::Node,
                "app.js".to_string(),
                PathBuf::from("/"),
            );
            apps.insert(&spec).await.unwrap();
        }

        let all = apps.get_all().await.unwrap();
        assert_eq!(all.len(), 3);
    }
}
