//! Apps repository - CRUD operations for applications

use chrono::{DateTime, SecondsFormat, Utc};
use oxidepm_core::{AppSpec, Desired, Error, Result};
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::Row;

/// One stored app.
///
/// A row whose stored spec cannot be read (a removed field after a downgrade, a hand
/// edit) is returned as `Broken` instead of failing the whole listing, so one bad row
/// never hides the others.
// Records are short-lived (one per row read); boxing the spec would only complicate the API.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum AppRecord {
    Ok {
        spec: AppSpec,
        parent_id: Option<u32>,
        desired: Desired,
    },
    Broken {
        id: u32,
        name: String,
        mode: String,
        parent_id: Option<u32>,
        desired: Desired,
        error: String,
        raw: String,
    },
}

impl AppRecord {
    pub fn id(&self) -> u32 {
        match self {
            AppRecord::Ok { spec, .. } => spec.id,
            AppRecord::Broken { id, .. } => *id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            AppRecord::Ok { spec, .. } => &spec.name,
            AppRecord::Broken { name, .. } => name,
        }
    }
}

const COLUMNS: &str =
    "id, name, mode, parent_id, desired_state, spec_json, created_at, protected, ai_control";

/// Repository for app operations
pub struct AppsRepository {
    pool: SqlitePool,
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::DbError(e.to_string())
}

/// Map a failed write: a UNIQUE violation on `name` is "app exists", the rest is a DB error.
fn write_err(e: sqlx::Error, name: &str) -> Error {
    if let sqlx::Error::Database(d) = &e {
        if d.is_unique_violation() {
            return Error::AppAlreadyExists(name.to_string());
        }
    }
    db_err(e)
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// `spec_json` of a spec: the id is stored as 0, the `id` column is authoritative.
fn spec_json(spec: &AppSpec) -> Result<String> {
    let mut copy = spec.clone();
    copy.id = 0;
    Ok(serde_json::to_string(&copy)?)
}

impl AppsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new app and return its id. A name that is taken is `AppAlreadyExists`.
    pub async fn insert_with(
        &self,
        spec: &AppSpec,
        parent_id: Option<u32>,
        desired: Desired,
    ) -> Result<u32> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        insert_row(&mut conn, spec, parent_id, desired).await
    }

    /// Insert a group (a cluster parent and its instances) in one transaction: either
    /// every row lands or none does. Every row starts `stopped`; the first successful
    /// launch of the group and of each instance flips its own, so a crash before that
    /// never resurrects a cluster that never ran. A name that is taken is
    /// `AppAlreadyExists` naming it.
    pub async fn insert_group(
        &self,
        parent: &AppSpec,
        children: &[AppSpec],
    ) -> Result<(u32, Vec<u32>)> {
        self.insert_group_with_desired(parent, children, Desired::Stopped)
            .await
    }

    /// Import a cluster atomically, with an explicit desired state for its parent.
    /// Child rows stay stopped until their own starts succeed.
    pub async fn insert_group_with_desired(
        &self,
        parent: &AppSpec,
        children: &[AppSpec],
        desired: Desired,
    ) -> Result<(u32, Vec<u32>)> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let parent_id = insert_row(&mut tx, parent, None, desired).await?;
        let mut ids = Vec::with_capacity(children.len());
        for child in children {
            ids.push(insert_row(&mut tx, child, Some(parent_id), Desired::Stopped).await?);
        }
        tx.commit().await.map_err(db_err)?;
        Ok((parent_id, ids))
    }

    /// Replace the stored spec. `created_at`, the parent and the desired state are kept.
    pub async fn update_spec(&self, id: u32, spec: &AppSpec) -> Result<()> {
        let result = sqlx::query(
            "UPDATE apps SET name = ?, mode = ?, spec_json = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&spec.name)
        .bind(spec.mode.as_str())
        .bind(spec_json(spec)?)
        .bind(rfc3339(Utc::now()))
        .bind(id as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| write_err(e, &spec.name))?;
        if result.rows_affected() == 0 {
            return Err(Error::AppNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Set the policy markers of `ids` in one transaction: a `None` leaves that marker as it is.
    /// Returns how many rows were changed. An id that has no row is `AppNotFound` and nothing
    /// is changed. (`update_spec` never writes the markers; only creation and this do.)
    pub async fn set_markers(
        &self,
        ids: &[u32],
        protected: Option<bool>,
        ai_control: Option<bool>,
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let now = rfc3339(Utc::now());
        let mut changed = 0;
        for id in ids {
            let result = sqlx::query(
                "UPDATE apps SET protected = COALESCE(?, protected), ai_control = COALESCE(?, ai_control), \
                 updated_at = ? WHERE id = ?",
            )
            .bind(protected.map(i64::from))
            .bind(ai_control.map(i64::from))
            .bind(&now)
            .bind(*id as i64)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            if result.rows_affected() == 0 {
                return Err(Error::AppNotFound(id.to_string()));
            }
            changed += 1;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(changed)
    }

    /// Record what the operator last asked for.
    pub async fn set_desired(&self, id: u32, desired: Desired) -> Result<()> {
        let result = sqlx::query("UPDATE apps SET desired_state = ?, updated_at = ? WHERE id = ?")
            .bind(desired.as_str())
            .bind(rfc3339(Utc::now()))
            .bind(id as i64)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(Error::AppNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Stored app by id, if any.
    pub async fn record_by_id(&self, id: u32) -> Result<Option<AppRecord>> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM apps WHERE id = ?"))
            .bind(id as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(read_row).transpose()
    }

    /// Stored app by name, if any.
    pub async fn record_by_name(&self, name: &str) -> Result<Option<AppRecord>> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM apps WHERE name = ?"))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(read_row).transpose()
    }

    /// Every stored app, oldest first. Unreadable rows come back as `Broken`.
    pub async fn records(&self) -> Result<Vec<AppRecord>> {
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM apps ORDER BY id"))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.iter().map(read_row).collect()
    }

    /// Delete an app; its children go with it (`ON DELETE CASCADE`).
    pub async fn delete(&self, id: u32) -> Result<bool> {
        let result = sqlx::query("DELETE FROM apps WHERE id = ?")
            .bind(id as i64)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(result.rows_affected() > 0)
    }
}

async fn insert_row(
    conn: &mut sqlx::SqliteConnection,
    spec: &AppSpec,
    parent_id: Option<u32>,
    desired: Desired,
) -> Result<u32> {
    let now = rfc3339(Utc::now());
    let result = sqlx::query(
        "INSERT INTO apps (name, mode, parent_id, desired_state, spec_json, created_at, updated_at, \
         protected, ai_control) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&spec.name)
    .bind(spec.mode.as_str())
    .bind(parent_id.map(|p| p as i64))
    .bind(desired.as_str())
    .bind(spec_json(spec)?)
    .bind(rfc3339(spec.created_at))
    .bind(&now)
    .bind(i64::from(spec.protected))
    .bind(i64::from(spec.ai_control))
    .execute(conn)
    .await
    .map_err(|e| write_err(e, &spec.name))?;
    Ok(result.last_insert_rowid() as u32)
}

fn required_column<T>(row: &SqliteRow, column: &str) -> std::result::Result<T, String>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get::<Option<T>, _>(column)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("column {column} is NULL"))
}

fn read_row(row: &SqliteRow) -> Result<AppRecord> {
    let id = required_column::<u32>(row, "id").map_err(db_err)?;
    let name = required_column::<String>(row, "name").map_err(db_err)?;
    let mode = required_column::<String>(row, "mode");
    let parent_id = row
        .try_get::<Option<u32>, _>("parent_id")
        .map_err(|e| e.to_string());
    let desired = required_column::<String>(row, "desired_state");
    let raw = required_column::<String>(row, "spec_json");

    let parsed = (|| {
        parent_id.as_ref().map_err(Clone::clone)?;
        desired.as_ref().map_err(Clone::clone)?;
        let created_at = required_column::<String>(row, "created_at")?;
        let protected = required_column::<i64>(row, "protected")? != 0;
        let ai_control = required_column::<i64>(row, "ai_control")? != 0;
        parse_spec(
            id,
            &name,
            mode.as_ref().map_err(Clone::clone)?,
            raw.as_ref().map_err(Clone::clone)?,
            &created_at,
            (protected, ai_control),
        )
    })();

    let parent_id = parent_id.unwrap_or_default();
    let desired = desired
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or_default();
    match parsed {
        Ok(spec) => Ok(AppRecord::Ok {
            spec,
            parent_id,
            desired,
        }),
        Err(error) => Ok(AppRecord::Broken {
            id,
            name,
            mode: mode.unwrap_or_default(),
            parent_id,
            desired,
            error,
            raw: raw.unwrap_or_default(),
        }),
    }
}

/// Parse `spec_json` and check it agrees with the columns. The `id` column, the `created_at`
/// column and the two marker columns (`protected`, `ai_control`) are authoritative.
fn parse_spec(
    id: u32,
    name: &str,
    mode: &str,
    raw: &str,
    created_at: &str,
    (protected, ai_control): (bool, bool),
) -> std::result::Result<AppSpec, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let mut spec = AppSpec::from_json_value(value).map_err(|e| e.to_string())?;
    if spec.name != name {
        return Err(format!(
            "name column '{}' disagrees with the stored spec '{}'",
            name, spec.name
        ));
    }
    if spec.mode.as_str() != mode {
        return Err(format!(
            "mode column '{}' disagrees with the stored spec '{}'",
            mode, spec.mode
        ));
    }
    spec.id = id;
    spec.protected = protected;
    spec.ai_control = ai_control;
    spec.created_at = DateTime::parse_from_rfc3339(created_at)
        .map_err(|e| format!("created_at {:?} is not RFC 3339: {}", created_at, e))?
        .with_timezone(&Utc);
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use oxidepm_core::Runtime;
    use std::path::PathBuf;
    use tempfile::{tempdir, TempDir};

    // Return both Database and TempDir to keep the directory alive
    async fn setup_db() -> (Database, TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::new(&db_path).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn a_malformed_column_marks_only_its_own_app_broken() {
        let (db, _dir) = setup_db().await;
        for name in ["bad", "good"] {
            let spec = AppSpec::new(name.into(), Runtime::CMD, "true".into(), PathBuf::from("/"));
            db.apps()
                .insert_with(&spec, None, Desired::Stopped)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE apps SET protected = 'invalid' WHERE name = 'bad'")
            .execute(db.pool())
            .await
            .unwrap();

        let records = db.apps().records().await.unwrap();
        assert!(
            matches!(&records[..], [AppRecord::Broken { name, error, .. }, AppRecord::Ok { spec, .. }]
            if name == "bad" && error.contains("protected") && spec.name == "good")
        );
    }

    #[tokio::test]
    async fn test_insert_and_get() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "test-app".to_string(),
            Runtime::from_static("node"),
            "server.js".to_string(),
            PathBuf::from("/app"),
        );

        let id = apps
            .insert_with(&spec, None, Desired::Running)
            .await
            .unwrap();
        assert!(id > 0);

        let retrieved = apps.record_by_id(id).await.unwrap();
        assert!(retrieved.is_some());
        match retrieved.unwrap() {
            AppRecord::Ok { spec, .. } => {
                assert_eq!(spec.name, "test-app");
                assert_eq!(spec.mode, Runtime::from_static("node"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn test_record_by_name() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "my-app".to_string(),
            Runtime::from_static("cargo"),
            "main".to_string(),
            PathBuf::from("/project"),
        );

        apps.insert_with(&spec, None, Desired::Running)
            .await
            .unwrap();

        let retrieved = apps.record_by_name("my-app").await.unwrap();
        match retrieved.unwrap() {
            AppRecord::Ok { spec, .. } => assert_eq!(spec.mode, Runtime::from_static("cargo")),
            other => panic!("{other:?}"),
        }

        let not_found = apps.record_by_name("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_delete() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        let spec = AppSpec::new(
            "to-delete".to_string(),
            Runtime::from_static("node"),
            "app.js".to_string(),
            PathBuf::from("/"),
        );

        let id = apps
            .insert_with(&spec, None, Desired::Running)
            .await
            .unwrap();
        assert!(apps.delete(id).await.unwrap());
        assert!(apps.record_by_id(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_records() {
        let (db, _dir) = setup_db().await;
        let apps = db.apps();

        for i in 0..3 {
            let spec = AppSpec::new(
                format!("app-{}", i),
                Runtime::from_static("node"),
                "app.js".to_string(),
                PathBuf::from("/"),
            );
            apps.insert_with(&spec, None, Desired::Running)
                .await
                .unwrap();
        }

        let all = apps.records().await.unwrap();
        assert_eq!(all.len(), 3);
    }
}
