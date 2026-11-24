//! Database schema for OxidePM (schema v1 and later migrations).

use crate::migrate::Migration;

/// Schema v1: one row per app. `name`, `mode`, `parent_id` and `desired_state` are
/// columns for lookup and constraints; everything else lives in `spec_json`, so a new
/// spec field never needs a column migration.
const V1: &str = r#"
CREATE TABLE apps (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL UNIQUE,
    mode          TEXT    NOT NULL,
    parent_id     INTEGER REFERENCES apps(id) ON DELETE CASCADE,
    desired_state TEXT    NOT NULL DEFAULT 'running' CHECK (desired_state IN ('running','stopped')),
    spec_json     TEXT    NOT NULL,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL
);
CREATE INDEX idx_apps_parent ON apps(parent_id);
"#;

/// Schema v2: the two policy markers of an app. They are columns (and not only keys of
/// `spec_json`) because the columns are the one source of truth for them: `SetPolicy` changes
/// them without touching the spec, and the load puts them back into the spec it returns.
const V2: &str = r#"
ALTER TABLE apps ADD COLUMN protected  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE apps ADD COLUMN ai_control INTEGER NOT NULL DEFAULT 0;
"#;

/// Every migration, oldest first. Versions are consecutive from 1.
pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 1, up: V1 },
    Migration { version: 2, up: V2 },
];

/// `adapters.db`, migration 1: the installed adapters (with one rollback step in the `prev_*`
/// columns), a small key/value table (`last_sequence:<key id>`, the last index sequence seen under
/// that key) and the cache of the last launch plan an adapter produced for an app.
const ADAPTERS_V1: &str = r#"
CREATE TABLE adapters (
    name              TEXT PRIMARY KEY,
    kind              TEXT NOT NULL CHECK (kind IN ('service','tool')),
    version           TEXT NOT NULL,
    exe_sha256        TEXT NOT NULL,
    manifest_json     TEXT NOT NULL,
    source            TEXT NOT NULL,
    trust             TEXT NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 0,
    grant_json        TEXT,
    grant_fingerprint TEXT,
    enforced_json     TEXT,
    prev_version      TEXT,
    prev_exe_sha256   TEXT,
    prev_manifest_json TEXT,
    prev_grant_json   TEXT,
    state_note        TEXT,
    installed_at      TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);
CREATE TABLE adapter_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE plans (
    app_name    TEXT PRIMARY KEY,
    spec_digest TEXT NOT NULL,
    adapter     TEXT NOT NULL,
    exe_sha256  TEXT NOT NULL,
    had_steps   INTEGER NOT NULL,
    plan_json   TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
"#;

/// Every migration of `adapters.db`, oldest first. It has its own version counter, independent of
/// `state.db`'s.
pub const ADAPTERS_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    up: ADAPTERS_V1,
}];
