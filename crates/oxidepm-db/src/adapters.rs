//! Adapter repository: the rows of `adapters.db` (installed adapters, index sequence numbers, the
//! plan cache). Strings that hold JSON are stored as given; this layer never interprets them.

use oxidepm_core::{Error, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqliteRow};
use sqlx::{Connection, Row};
use std::path::Path;

/// One installed adapter. `kind` is `service` or `tool` (the table refuses anything else);
/// `prev_*` hold the single version an update can be rolled back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterRow {
    pub name: String,
    pub kind: String,
    pub version: String,
    pub exe_sha256: String,
    pub manifest_json: String,
    /// `index`, `bundled` or `pinned:<url or path>`.
    pub source: String,
    /// `signed:<key id>` or `pinned`.
    pub trust: String,
    pub enabled: bool,
    pub grant_json: Option<String>,
    pub grant_fingerprint: Option<String>,
    pub enforced_json: Option<String>,
    pub prev_version: Option<String>,
    pub prev_exe_sha256: Option<String>,
    pub prev_manifest_json: Option<String>,
    pub prev_grant_json: Option<String>,
    pub state_note: Option<String>,
    pub installed_at: String,
    pub updated_at: String,
}

/// The last launch plan an adapter produced for an app (a cache, keyed by app name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub app_name: String,
    pub spec_digest: String,
    pub adapter: String,
    pub exe_sha256: String,
    pub had_steps: bool,
    pub plan_json: String,
    pub created_at: String,
}

const ADAPTER_COLUMNS: &str = "name, kind, version, exe_sha256, manifest_json, source, trust, enabled, \
     grant_json, grant_fingerprint, enforced_json, prev_version, prev_exe_sha256, prev_manifest_json, \
     prev_grant_json, state_note, installed_at, updated_at";

const PLAN_COLUMNS: &str =
    "app_name, spec_digest, adapter, exe_sha256, had_steps, plan_json, created_at";

/// Repository over `adapters.db`.
pub struct AdapterRepository {
    pool: SqlitePool,
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::DbError(e.to_string())
}

fn row_from(r: &SqliteRow) -> Result<AdapterRow> {
    Ok(AdapterRow {
        name: r.try_get("name").map_err(db_err)?,
        kind: r.try_get("kind").map_err(db_err)?,
        version: r.try_get("version").map_err(db_err)?,
        exe_sha256: r.try_get("exe_sha256").map_err(db_err)?,
        manifest_json: r.try_get("manifest_json").map_err(db_err)?,
        source: r.try_get("source").map_err(db_err)?,
        trust: r.try_get("trust").map_err(db_err)?,
        enabled: r.try_get::<i64, _>("enabled").map_err(db_err)? != 0,
        grant_json: r.try_get("grant_json").map_err(db_err)?,
        grant_fingerprint: r.try_get("grant_fingerprint").map_err(db_err)?,
        enforced_json: r.try_get("enforced_json").map_err(db_err)?,
        prev_version: r.try_get("prev_version").map_err(db_err)?,
        prev_exe_sha256: r.try_get("prev_exe_sha256").map_err(db_err)?,
        prev_manifest_json: r.try_get("prev_manifest_json").map_err(db_err)?,
        prev_grant_json: r.try_get("prev_grant_json").map_err(db_err)?,
        state_note: r.try_get("state_note").map_err(db_err)?,
        installed_at: r.try_get("installed_at").map_err(db_err)?,
        updated_at: r.try_get("updated_at").map_err(db_err)?,
    })
}

fn plan_from(r: &SqliteRow) -> Result<PlanRow> {
    Ok(PlanRow {
        app_name: r.try_get("app_name").map_err(db_err)?,
        spec_digest: r.try_get("spec_digest").map_err(db_err)?,
        adapter: r.try_get("adapter").map_err(db_err)?,
        exe_sha256: r.try_get("exe_sha256").map_err(db_err)?,
        had_steps: r.try_get::<i64, _>("had_steps").map_err(db_err)? != 0,
        plan_json: r.try_get("plan_json").map_err(db_err)?,
        created_at: r.try_get("created_at").map_err(db_err)?,
    })
}

impl AdapterRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Read the installed bundled versions without creating or migrating the database.
    pub async fn bundled_versions_at(path: &Path) -> Result<Vec<(String, String)>> {
        if !path.try_exists()? {
            return Ok(Vec::new());
        }
        let options = SqliteConnectOptions::new().filename(path).read_only(true);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(db_err)?;
        sqlx::query_as("SELECT name, version FROM adapters WHERE source = 'bundled' ORDER BY name")
            .fetch_all(&mut connection)
            .await
            .map_err(db_err)
    }

    /// Insert a new adapter. A name that is taken, or a `kind` other than `service` or `tool`,
    /// is `AdapterInvalid`; nothing is written then.
    pub async fn insert(&self, a: &AdapterRow) -> Result<()> {
        let result = sqlx::query(&format!(
            "INSERT INTO adapters ({ADAPTER_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&a.name)
        .bind(&a.kind)
        .bind(&a.version)
        .bind(&a.exe_sha256)
        .bind(&a.manifest_json)
        .bind(&a.source)
        .bind(&a.trust)
        .bind(a.enabled as i64)
        .bind(&a.grant_json)
        .bind(&a.grant_fingerprint)
        .bind(&a.enforced_json)
        .bind(&a.prev_version)
        .bind(&a.prev_exe_sha256)
        .bind(&a.prev_manifest_json)
        .bind(&a.prev_grant_json)
        .bind(&a.state_note)
        .bind(&a.installed_at)
        .bind(&a.updated_at)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Err(Error::AdapterInvalid(
                format!("adapter '{}' is already installed", a.name),
            )),
            Err(sqlx::Error::Database(d)) if d.is_check_violation() => {
                Err(Error::AdapterInvalid(format!(
                    "adapter '{}': kind must be 'service' or 'tool', not '{}'",
                    a.name, a.kind
                )))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    /// Replace every column of the row of `a.name` except the name and `installed_at`. A name
    /// that is not installed is `AdapterMissing` (an update never inserts).
    pub async fn update(&self, a: &AdapterRow) -> Result<()> {
        let result = sqlx::query(
            "UPDATE adapters SET kind = ?, version = ?, exe_sha256 = ?, manifest_json = ?, source = ?, \
             trust = ?, enabled = ?, grant_json = ?, grant_fingerprint = ?, enforced_json = ?, \
             prev_version = ?, prev_exe_sha256 = ?, prev_manifest_json = ?, prev_grant_json = ?, \
             state_note = ?, updated_at = ? WHERE name = ?",
        )
        .bind(&a.kind)
        .bind(&a.version)
        .bind(&a.exe_sha256)
        .bind(&a.manifest_json)
        .bind(&a.source)
        .bind(&a.trust)
        .bind(a.enabled as i64)
        .bind(&a.grant_json)
        .bind(&a.grant_fingerprint)
        .bind(&a.enforced_json)
        .bind(&a.prev_version)
        .bind(&a.prev_exe_sha256)
        .bind(&a.prev_manifest_json)
        .bind(&a.prev_grant_json)
        .bind(&a.state_note)
        .bind(&a.updated_at)
        .bind(&a.name)
        .execute(&self.pool)
        .await;
        match result {
            Ok(r) if r.rows_affected() == 0 => Err(Error::AdapterMissing(format!(
                "adapter '{}' is not installed",
                a.name
            ))),
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(d)) if d.is_check_violation() => {
                Err(Error::AdapterInvalid(format!(
                    "adapter '{}': kind must be 'service' or 'tool', not '{}'",
                    a.name, a.kind
                )))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    /// One adapter by name.
    pub async fn get(&self, name: &str) -> Result<Option<AdapterRow>> {
        let row = sqlx::query(&format!(
            "SELECT {ADAPTER_COLUMNS} FROM adapters WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.as_ref().map(row_from).transpose()
    }

    /// Every adapter, ordered by name.
    pub async fn list(&self) -> Result<Vec<AdapterRow>> {
        let rows = sqlx::query(&format!(
            "SELECT {ADAPTER_COLUMNS} FROM adapters ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_from).collect()
    }

    /// Remove an adapter; `true` if there was one.
    pub async fn delete(&self, name: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM adapters WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(r.rows_affected() > 0)
    }

    /// Change only the note of a row (the reason an adapter failed), and only while the row is at
    /// `version`: a note about a version that has been replaced since is not written. A row that
    /// is gone, or a concurrent change of any other column, is not affected.
    pub async fn set_note(&self, name: &str, version: &str, note: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE adapters SET state_note = ? WHERE name = ? AND version = ?")
            .bind(note)
            .bind(name)
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Mark a row disabled with the reason, touching no other column, while it is at `version`.
    pub async fn disable_with_note(&self, name: &str, version: &str, note: &str) -> Result<()> {
        sqlx::query(
            "UPDATE adapters SET enabled = 0, state_note = ? WHERE name = ? AND version = ?",
        )
        .bind(note)
        .bind(name)
        .bind(version)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// A value of the small key/value table.
    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM adapter_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|r| r.try_get::<String, _>(0).map_err(db_err))
            .transpose()
    }

    /// Every entry of the key/value table whose key starts with `prefix`, by key.
    pub async fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT key, value FROM adapter_meta WHERE substr(key, 1, ?) = ? ORDER BY key",
        )
        .bind(prefix.chars().count() as i64)
        .bind(prefix)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>(0).map_err(db_err)?,
                    r.try_get::<String, _>(1).map_err(db_err)?,
                ))
            })
            .collect()
    }

    /// Set (insert or replace) a value of the key/value table.
    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO adapter_meta (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Store the plan of an app, replacing the one it had.
    pub async fn put_plan(&self, p: &PlanRow) -> Result<()> {
        sqlx::query(&format!(
            "INSERT OR REPLACE INTO plans ({PLAN_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&p.app_name)
        .bind(&p.spec_digest)
        .bind(&p.adapter)
        .bind(&p.exe_sha256)
        .bind(p.had_steps as i64)
        .bind(&p.plan_json)
        .bind(&p.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// The cached plan of an app.
    pub async fn get_plan(&self, app_name: &str) -> Result<Option<PlanRow>> {
        let row = sqlx::query(&format!(
            "SELECT {PLAN_COLUMNS} FROM plans WHERE app_name = ?"
        ))
        .bind(app_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.as_ref().map(plan_from).transpose()
    }

    /// Drop every cached plan that `adapter` made; the number of rows removed.
    pub async fn delete_plans_of(&self, adapter: &str) -> Result<u64> {
        let r = sqlx::query("DELETE FROM plans WHERE adapter = ?")
            .bind(adapter)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(r.rows_affected())
    }

    /// Drop the cached plan of an app; `true` if there was one.
    pub async fn delete_plan(&self, app_name: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM plans WHERE app_name = ?")
            .bind(app_name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(r.rows_affected() > 0)
    }
}
