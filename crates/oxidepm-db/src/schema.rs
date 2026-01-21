//! Database schema for OxidePM

/// SQLite schema initialization
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS apps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    mode TEXT NOT NULL,
    command TEXT NOT NULL,
    args TEXT NOT NULL DEFAULT '[]',
    cwd TEXT NOT NULL,
    env TEXT NOT NULL DEFAULT '{}',
    watch INTEGER NOT NULL DEFAULT 0,
    ignore_patterns TEXT NOT NULL DEFAULT '[]',
    auto_restart INTEGER NOT NULL DEFAULT 1,
    max_restarts INTEGER NOT NULL DEFAULT 15,
    restart_delay_ms INTEGER NOT NULL DEFAULT 500,
    crash_window_secs INTEGER NOT NULL DEFAULT 60,
    kill_timeout_ms INTEGER NOT NULL DEFAULT 3000,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL,
    pid INTEGER,
    status TEXT NOT NULL,
    restarts INTEGER NOT NULL DEFAULT 0,
    start_time TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    stop_time TEXT,
    exit_code INTEGER,
    FOREIGN KEY (app_id) REFERENCES apps(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_runs_app_id ON runs(app_id);
CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);

CREATE TABLE IF NOT EXISTS metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL,
    timestamp TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    cpu_percent REAL,
    memory_bytes INTEGER,
    FOREIGN KEY (app_id) REFERENCES apps(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_metrics_app_id ON metrics(app_id);
CREATE INDEX IF NOT EXISTS idx_metrics_timestamp ON metrics(timestamp);
"#;
