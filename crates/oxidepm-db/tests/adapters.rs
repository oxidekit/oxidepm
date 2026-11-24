//! `adapters.db`: its own file, its own migrator, and no effect on `state.db`.

use std::os::unix::fs::PermissionsExt;

use oxidepm_core::{Error, Paths};
use oxidepm_db::migrate::user_version;
use oxidepm_db::schema::{ADAPTERS_MIGRATIONS, MIGRATIONS};
use oxidepm_db::{AdapterRow, Database, PlanRow};
use sqlx::Row;
use tempfile::tempdir;

fn row(name: &str, version: &str) -> AdapterRow {
    AdapterRow {
        name: name.to_string(),
        kind: "service".to_string(),
        version: version.to_string(),
        exe_sha256: "a".repeat(64),
        manifest_json: format!("{{\"name\":\"{name}\"}}"),
        source: "index".to_string(),
        trust: "signed:key1".to_string(),
        enabled: false,
        grant_json: None,
        grant_fingerprint: None,
        enforced_json: None,
        prev_version: None,
        prev_exe_sha256: None,
        prev_manifest_json: None,
        prev_grant_json: None,
        state_note: None,
        installed_at: "2026-11-01T10:00:00Z".to_string(),
        updated_at: "2026-11-01T10:00:00Z".to_string(),
    }
}

async fn tables(db: &Database) -> Vec<String> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    rows.iter().map(|r| r.get::<String, _>(0)).collect()
}

async fn columns(db: &Database, table: &str) -> Vec<String> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(db.pool())
        .await
        .unwrap();
    rows.iter().map(|r| r.get::<String, _>("name")).collect()
}

#[tokio::test]
async fn adapters_db_migration1_creates_tables() {
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));
    let db = Database::open_adapters(&paths.adapters_db()).await.unwrap();

    assert_eq!(tables(&db).await, vec!["adapter_meta", "adapters", "plans"]);
    assert_eq!(user_version(db.pool()).await.unwrap(), 1);
    assert_eq!(ADAPTERS_MIGRATIONS.len(), 1);
    assert_eq!(
        columns(&db, "adapters").await,
        vec![
            "name",
            "kind",
            "version",
            "exe_sha256",
            "manifest_json",
            "source",
            "trust",
            "enabled",
            "grant_json",
            "grant_fingerprint",
            "enforced_json",
            "prev_version",
            "prev_exe_sha256",
            "prev_manifest_json",
            "prev_grant_json",
            "state_note",
            "installed_at",
            "updated_at"
        ]
    );
    assert_eq!(columns(&db, "adapter_meta").await, vec!["key", "value"]);
    assert_eq!(
        columns(&db, "plans").await,
        vec![
            "app_name",
            "spec_digest",
            "adapter",
            "exe_sha256",
            "had_steps",
            "plan_json",
            "created_at"
        ]
    );

    // The file is private, like state.db.
    let mode = std::fs::metadata(paths.adapters_db())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    db.close().await;

    // Opening again is a no-op: same version, no backup copy of a fresh file.
    let db = Database::open_adapters(&paths.adapters_db()).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 1);
    db.close().await;
}

#[tokio::test]
async fn adapters_db_ignores_the_legacy_state_file_check() {
    // `oxidepm.db` next to a missing `state.db` stops the daemon (a home of an older release).
    // That rule belongs to state.db only: it must not block a new adapters.db.
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));
    std::fs::create_dir_all(paths.home()).unwrap();
    std::fs::write(paths.legacy_db(), b"old").unwrap();
    let db = Database::open_adapters(&paths.adapters_db()).await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn adapters_db_from_a_newer_build_is_refused_and_named_as_such() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("adapters.db");
    {
        let db = Database::open_adapters(&path).await.unwrap();
        sqlx::query("PRAGMA user_version = 9")
            .execute(db.pool())
            .await
            .unwrap();
        db.close().await;
    }
    let err = match Database::open_adapters(&path).await {
        Ok(_) => panic!("must refuse"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::StateIncompatible(_)), "{err:?}");
    assert!(err.to_string().contains("newer OxidePM"), "{err}");
}

#[tokio::test]
async fn state_db_is_untouched_by_adapters() {
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));

    let state = Database::new(&paths.db()).await.unwrap();
    let before_tables = tables(&state).await;
    let before_version = user_version(state.pool()).await.unwrap();
    assert_eq!(before_tables, vec!["apps"]);
    assert_eq!(before_version, MIGRATIONS.last().unwrap().version);

    // Use adapters.db in full while state.db is open.
    let adapters = Database::open_adapters(&paths.adapters_db()).await.unwrap();
    adapters
        .adapters()
        .insert(&row("watch", "1.0.0"))
        .await
        .unwrap();
    adapters
        .adapters()
        .set_meta("last_sequence:k1", "7")
        .await
        .unwrap();
    adapters.close().await;

    assert_eq!(tables(&state).await, before_tables);
    assert_eq!(user_version(state.pool()).await.unwrap(), before_version);
    assert_eq!(
        columns(&state, "apps")
            .await
            .iter()
            .filter(|c| c.contains("adapter"))
            .count(),
        0
    );
    state.close().await;

    // And the other way round: the adapter file has no app tables, and a home that never
    // imported anything has no adapters.db at all (it is created on first use only).
    let again = Database::open_adapters(&paths.adapters_db()).await.unwrap();
    assert!(!tables(&again).await.contains(&"apps".to_string()));
    again.close().await;
    let fresh = Paths::new(tmp.path().join("fresh"));
    let s = Database::new(&fresh.db()).await.unwrap();
    s.close().await;
    assert!(!fresh.adapters_db().exists());
}

#[tokio::test]
async fn adapter_row_roundtrip() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();

    assert!(repo.get("notify").await.unwrap().is_none());
    assert!(repo.list().await.unwrap().is_empty());

    let mut full = row("notify", "2.3.1");
    full.kind = "tool".to_string();
    full.source = "pinned:https://example.org/a.tar.gz".to_string();
    full.trust = "pinned".to_string();
    full.enabled = true;
    full.grant_json = Some("{\"host\":{\"level\":\"read\"}}".to_string());
    full.grant_fingerprint = Some("0123456789abcdef".to_string());
    full.enforced_json = Some("{\"landlock\":true}".to_string());
    full.prev_version = Some("2.3.0".to_string());
    full.prev_exe_sha256 = Some("b".repeat(64));
    full.prev_manifest_json = Some("{\"name\":\"notify\",\"old\":true}".to_string());
    full.prev_grant_json = Some("{}".to_string());
    full.state_note = Some("revoked".to_string());
    repo.insert(&full).await.unwrap();
    repo.insert(&row("check", "0.1.0")).await.unwrap();

    assert_eq!(repo.get("notify").await.unwrap(), Some(full.clone()));
    // Listing is ordered by name.
    let names: Vec<String> = repo
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, vec!["check", "notify"]);

    // Update replaces every column except the name and the install time.
    let mut changed = full.clone();
    changed.enabled = false;
    changed.grant_json = None;
    changed.grant_fingerprint = None;
    changed.state_note = None;
    changed.version = "2.4.0".to_string();
    changed.installed_at = "1999-01-01T00:00:00Z".to_string();
    changed.updated_at = "2026-11-02T10:00:00Z".to_string();
    repo.update(&changed).await.unwrap();
    let back = repo.get("notify").await.unwrap().unwrap();
    assert_eq!(back.version, "2.4.0");
    assert!(!back.enabled && back.grant_json.is_none() && back.state_note.is_none());
    assert_eq!(back.updated_at, "2026-11-02T10:00:00Z");
    assert_eq!(
        back.installed_at, full.installed_at,
        "the install time is kept"
    );

    // Updating a name that is not there is an error, not an insert.
    let err = repo.update(&row("ghost", "1.0.0")).await.unwrap_err();
    assert!(matches!(err, Error::AdapterMissing(_)), "{err:?}");
    assert!(repo.get("ghost").await.unwrap().is_none());

    assert!(repo.delete("notify").await.unwrap());
    assert!(!repo.delete("notify").await.unwrap());
    assert!(repo.get("notify").await.unwrap().is_none());
    assert!(repo.get("check").await.unwrap().is_some());
}

#[tokio::test]
async fn adapter_name_unique_and_kind_checked() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();

    repo.insert(&row("watch", "1.0.0")).await.unwrap();
    let err = repo.insert(&row("watch", "2.0.0")).await.unwrap_err();
    assert!(matches!(err, Error::AdapterInvalid(_)), "{err:?}");
    assert!(err.to_string().contains("watch"), "{err}");
    assert_eq!(
        repo.get("watch").await.unwrap().unwrap().version,
        "1.0.0",
        "the first row is kept"
    );

    for bad in ["", "runtime", "Service", "service ", "ui"] {
        let mut r = row("other", "1.0.0");
        r.kind = bad.to_string();
        let err = repo.insert(&r).await.unwrap_err();
        assert!(matches!(err, Error::AdapterInvalid(_)), "{bad:?}: {err:?}");
    }
    assert!(repo.get("other").await.unwrap().is_none());

    // The database enforces it too, not only the repository: a direct write is refused.
    let direct = sqlx::query(
        "INSERT INTO adapters (name, kind, version, exe_sha256, manifest_json, source, trust, installed_at, updated_at) \
         VALUES ('x','daemon','1','s','{}','index','pinned','t','t')",
    )
    .execute(db.pool())
    .await;
    assert!(direct.is_err());
    // `enabled` defaults to 0.
    sqlx::query(
        "INSERT INTO adapters (name, kind, version, exe_sha256, manifest_json, source, trust, installed_at, updated_at) \
         VALUES ('y','tool','1','s','{}','index','pinned','t','t')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert!(!repo.get("y").await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn prev_columns_hold_one_rollback_step() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();

    let v1 = row("deploy", "1.0.0");
    repo.insert(&v1).await.unwrap();

    // Update to 2.0.0: the row moves what was current into the prev_* columns.
    let mut v2 = repo.get("deploy").await.unwrap().unwrap();
    v2.prev_version = Some(v2.version.clone());
    v2.prev_exe_sha256 = Some(v2.exe_sha256.clone());
    v2.prev_manifest_json = Some(v2.manifest_json.clone());
    v2.prev_grant_json = v2.grant_json.clone();
    v2.version = "2.0.0".to_string();
    v2.exe_sha256 = "c".repeat(64);
    v2.manifest_json = "{\"name\":\"deploy\",\"v\":2}".to_string();
    repo.update(&v2).await.unwrap();
    let got = repo.get("deploy").await.unwrap().unwrap();
    assert_eq!(got.prev_version.as_deref(), Some("1.0.0"));
    assert_eq!(got.prev_exe_sha256, Some("a".repeat(64)));
    assert_eq!(got.prev_manifest_json, Some(v1.manifest_json.clone()));

    // Update to 3.0.0: the step is overwritten, there is no second step to go back to.
    let mut v3 = got.clone();
    v3.prev_version = Some(v3.version.clone());
    v3.prev_exe_sha256 = Some(v3.exe_sha256.clone());
    v3.prev_manifest_json = Some(v3.manifest_json.clone());
    v3.version = "3.0.0".to_string();
    v3.exe_sha256 = "d".repeat(64);
    repo.update(&v3).await.unwrap();
    let got = repo.get("deploy").await.unwrap().unwrap();
    assert_eq!(got.prev_version.as_deref(), Some("2.0.0"));
    assert_eq!(got.prev_exe_sha256, Some("c".repeat(64)));
    let all = serde_json::to_string(&got.prev_manifest_json).unwrap();
    assert!(
        !all.contains("1.0.0") && all.contains("\\\"v\\\":2"),
        "{all}"
    );
    let cols = columns(&db, "adapters").await;
    assert_eq!(
        cols.iter().filter(|c| c.starts_with("prev_")).count(),
        4,
        "{cols:?}"
    );

    // After a rollback the prev_* columns are cleared again.
    let mut back = got.clone();
    back.version = back.prev_version.take().unwrap();
    back.exe_sha256 = back.prev_exe_sha256.take().unwrap();
    back.manifest_json = back.prev_manifest_json.take().unwrap();
    back.prev_grant_json = None;
    repo.update(&back).await.unwrap();
    let got = repo.get("deploy").await.unwrap().unwrap();
    assert_eq!(got.version, "2.0.0");
    assert!(
        got.prev_version.is_none()
            && got.prev_exe_sha256.is_none()
            && got.prev_manifest_json.is_none()
    );
}

#[tokio::test]
async fn plan_row_roundtrip() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();

    let plan = PlanRow {
        app_name: "web".to_string(),
        spec_digest: "f".repeat(64),
        adapter: "runtime-node".to_string(),
        exe_sha256: "a".repeat(64),
        had_steps: true,
        plan_json: "{\"program\":\"/usr/bin/node\",\"args\":[\"server.js\"]}".to_string(),
        created_at: "2026-11-01T10:00:00Z".to_string(),
    };
    assert!(repo.get_plan("web").await.unwrap().is_none());
    repo.put_plan(&plan).await.unwrap();
    assert_eq!(repo.get_plan("web").await.unwrap(), Some(plan.clone()));

    // One plan per app name: a second put replaces the first.
    let mut newer = plan.clone();
    newer.spec_digest = "e".repeat(64);
    newer.had_steps = false;
    newer.created_at = "2026-11-02T10:00:00Z".to_string();
    repo.put_plan(&newer).await.unwrap();
    assert_eq!(repo.get_plan("web").await.unwrap(), Some(newer.clone()));
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM plans")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);

    assert!(repo.delete_plan("web").await.unwrap());
    assert!(!repo.delete_plan("web").await.unwrap());
    assert!(repo.get_plan("web").await.unwrap().is_none());
}

#[tokio::test]
async fn adapter_meta_roundtrip() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();
    assert!(repo.get_meta("last_sequence:k1").await.unwrap().is_none());
    repo.set_meta("last_sequence:k1", "41").await.unwrap();
    repo.set_meta("last_sequence:k1", "42").await.unwrap();
    repo.set_meta("last_sequence:k2", "5").await.unwrap();
    assert_eq!(
        repo.get_meta("last_sequence:k1").await.unwrap().as_deref(),
        Some("42")
    );
    assert_eq!(
        repo.get_meta("last_sequence:k2").await.unwrap().as_deref(),
        Some("5")
    );
}

#[tokio::test]
async fn meta_entries_are_listed_by_prefix() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();
    repo.set_meta("last_sequence:k1", "42").await.unwrap();
    repo.set_meta("last_sequence:k2", "5").await.unwrap();
    repo.set_meta("other:k3", "9").await.unwrap();
    assert_eq!(
        repo.meta_with_prefix("last_sequence:").await.unwrap(),
        vec![
            ("last_sequence:k1".to_string(), "42".to_string()),
            ("last_sequence:k2".to_string(), "5".to_string())
        ]
    );
    assert!(repo.meta_with_prefix("nothing:").await.unwrap().is_empty());
    // a prefix is a prefix, not a pattern
    assert!(repo.meta_with_prefix("last_%").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_note_or_a_disable_changes_nothing_else_of_the_row() {
    let tmp = tempdir().unwrap();
    let db = Database::open_adapters(&tmp.path().join("adapters.db"))
        .await
        .unwrap();
    let repo = db.adapters();
    let mut a = row("watch", "1.0.0");
    a.enabled = true;
    a.grant_json = Some("{}".to_string());
    repo.insert(&a).await.unwrap();

    repo.set_note("watch", "1.0.0", Some("failed: crash_loop"))
        .await
        .unwrap();
    let got = repo.get("watch").await.unwrap().unwrap();
    assert_eq!(got.state_note.as_deref(), Some("failed: crash_loop"));
    assert!(
        got.enabled && got.grant_json.is_some(),
        "only the note changed"
    );

    repo.disable_with_note("watch", "1.0.0", "disabled: state over quota")
        .await
        .unwrap();
    let got = repo.get("watch").await.unwrap().unwrap();
    assert!(!got.enabled);
    assert_eq!(
        got.state_note.as_deref(),
        Some("disabled: state over quota")
    );
    assert!(got.grant_json.is_some(), "the grant is not touched");

    // a note about a version that is not the row's any more is not written
    repo.set_note("watch", "0.9.0", Some("about an older version"))
        .await
        .unwrap();
    repo.disable_with_note("watch", "0.9.0", "about an older version")
        .await
        .unwrap();
    assert_eq!(
        repo.get("watch")
            .await
            .unwrap()
            .unwrap()
            .state_note
            .as_deref(),
        Some("disabled: state over quota")
    );

    // a row that is gone is not made
    repo.set_note("gone", "1.0.0", Some("x")).await.unwrap();
    assert!(repo.get("gone").await.unwrap().is_none());
}
