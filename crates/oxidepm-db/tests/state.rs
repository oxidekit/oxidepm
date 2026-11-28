//! Schema v1: files, versions, migrations, groups, broken rows.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use oxidepm_core::{AppSpec, Desired, Error, Paths, Runtime};
use oxidepm_db::migrate::{backup_path, user_version};
use oxidepm_db::{AppRecord, AppsRepository, Database, Migration};
use sqlx::Row;
use tempfile::tempdir;

fn spec(name: &str) -> AppSpec {
    AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        PathBuf::from("/tmp"),
    )
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

async fn pragma_text(db: &Database, name: &str) -> String {
    let row = sqlx::query(&format!("PRAGMA {name}"))
        .fetch_one(db.pool())
        .await
        .unwrap();
    match row.try_get::<String, _>(0) {
        Ok(s) => s,
        Err(_) => row.get::<i64, _>(0).to_string(),
    }
}

/// A new top-level app that should be running.
async fn insert(apps: &AppsRepository, spec: &AppSpec) -> oxidepm_core::Result<u32> {
    apps.insert_with(spec, None, Desired::Running).await
}

#[tokio::test]
async fn db_file_is_0600_and_home_0700() {
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));
    paths.ensure_dirs().unwrap();
    let db = Database::new(&paths.db()).await.unwrap();
    insert(&db.apps(), &spec("web")).await.unwrap();

    assert_eq!(mode(paths.home()), 0o700);
    assert_eq!(mode(&paths.db()), 0o600);
    let mut sidecars = 0;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{}", paths.db().display(), suffix));
        if side.exists() {
            sidecars += 1;
            assert_eq!(mode(&side), 0o600, "{}", side.display());
        }
    }
    assert!(
        sidecars > 0,
        "WAL mode keeps sidecar files while the database is open"
    );
    db.close().await;
}

#[tokio::test]
async fn database_creates_a_missing_parent_private() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("fresh").join("state.db");
    let db = Database::new(&path).await.unwrap();
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    db.close().await;
}

#[tokio::test]
async fn loose_existing_db_file_is_tightened() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let db = Database::new(&path).await.unwrap();
    assert_eq!(mode(&path), 0o600);
    db.close().await;
}

#[tokio::test]
async fn wal_and_busy_timeout_pragmas_set() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    assert_eq!(pragma_text(&db, "journal_mode").await, "wal");
    assert_eq!(pragma_text(&db, "busy_timeout").await, "5000");
    assert_eq!(pragma_text(&db, "foreign_keys").await, "1");
    assert_eq!(pragma_text(&db, "synchronous").await, "1", "NORMAL");
    assert_eq!(pragma_text(&db, "user_version").await, "2");
    db.close().await;
}

#[tokio::test]
async fn legacy_oxidepm_db_without_state_db_is_refused_with_path_and_remedy() {
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));
    paths.ensure_dirs().unwrap();
    std::fs::write(paths.legacy_db(), b"old schema").unwrap();

    let err = match Database::new(&paths.db()).await {
        Ok(_) => panic!("must refuse"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::StateIncompatible(_)), "{err:?}");
    let text = err.to_string();
    let legacy = paths.legacy_db().display().to_string();
    assert!(text.contains(&legacy), "names the file: {text}");
    assert!(
        text.contains(&format!("mv {legacy} {legacy}.old")),
        "gives the remedy: {text}"
    );
    assert!(!paths.db().exists(), "a refusal leaves no state.db behind");
    assert_eq!(
        std::fs::read(paths.legacy_db()).unwrap(),
        b"old schema",
        "legacy file untouched"
    );
}

#[tokio::test]
async fn legacy_file_is_ignored_once_state_db_exists() {
    let tmp = tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("home"));
    paths.ensure_dirs().unwrap();
    Database::new(&paths.db()).await.unwrap().close().await;
    std::fs::write(paths.legacy_db(), b"old schema").unwrap();

    let db = Database::new(&paths.db()).await.expect("state.db wins");
    db.close().await;
    assert_eq!(std::fs::read(paths.legacy_db()).unwrap(), b"old schema");
}

#[tokio::test]
async fn newer_user_version_is_refused() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = Database::new(&path).await.unwrap();
    sqlx::query("PRAGMA user_version = 99")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let err = match Database::new(&path).await {
        Ok(_) => panic!("must refuse"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::StateIncompatible(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains(&path.display().to_string()) && text.contains("newer OxidePM"),
        "{text}"
    );
}

#[tokio::test]
async fn unversioned_database_with_tables_is_refused() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let db = Database::new(&path).await.unwrap();
        sqlx::query("PRAGMA user_version = 0")
            .execute(db.pool())
            .await
            .unwrap();
        db.close().await;
    }
    let err = match Database::new(&path).await {
        Ok(_) => panic!("must refuse"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::StateIncompatible(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("not a state.db written by this OxidePM"),
        "{err}"
    );
}

const M1: Migration = Migration {
    version: 1,
    up: "CREATE TABLE apps (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);",
};
const M2: Migration = Migration {
    version: 2,
    up: "ALTER TABLE apps ADD COLUMN extra TEXT NOT NULL DEFAULT 'dflt';",
};
const M2_BAD: Migration = Migration {
    version: 2,
    up: "ALTER TABLE apps ADD COLUMN half_done TEXT; INSERT INTO no_such_table VALUES (1);",
};

async fn names(db: &Database) -> Vec<String> {
    sqlx::query("SELECT name FROM apps ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>(0))
        .collect()
}

#[tokio::test]
async fn migrator_applies_n_minus_1_to_n_preserving_rows() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = Database::open(&path, &[M1]).await.unwrap();
    sqlx::query("INSERT INTO apps (name) VALUES ('a'), ('b')")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 1);
    db.close().await;
    assert!(!backup_path(&path, 2).exists());

    let db = Database::open(&path, &[M1, M2]).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 2);
    assert_eq!(names(&db).await, vec!["a", "b"]);
    let extra: String = sqlx::query("SELECT extra FROM apps WHERE name = 'a'")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get(0);
    assert_eq!(extra, "dflt");
    let backup = backup_path(&path, 2);
    assert!(
        backup.exists(),
        "an existing database is copied before it is migrated"
    );
    assert_eq!(mode(&backup), 0o600);
    db.close().await;

    // already current: nothing runs again
    let db = Database::open(&path, &[M1, M2]).await.unwrap();
    assert_eq!(names(&db).await, vec!["a", "b"]);
    db.close().await;
}

#[tokio::test]
async fn fresh_database_gets_every_migration_and_no_backup() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = Database::open(&path, &[M1, M2]).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 2);
    db.close().await;
    assert!(!backup_path(&path, 1).exists() && !backup_path(&path, 2).exists());
}

#[tokio::test]
async fn failed_migration_leaves_user_version_and_rows() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = Database::open(&path, &[M1]).await.unwrap();
    sqlx::query("INSERT INTO apps (name) VALUES ('a'), ('b')")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let err = match Database::open(&path, &[M1, M2_BAD]).await {
        Ok(_) => panic!("the broken migration must fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("v2"), "{err}");

    // rolled back as a whole: still v1, same rows, the half-applied column is gone
    let db = Database::open(&path, &[M1]).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 1);
    assert_eq!(names(&db).await, vec!["a", "b"]);
    let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('apps')")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(cols, vec!["id", "name"]);
    assert!(
        backup_path(&path, 2).exists(),
        "the pre-migration copy exists"
    );
    db.close().await;
}

#[tokio::test]
async fn insert_group_is_transactional() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();

    let err = apps
        .insert_group(
            &spec("cluster"),
            &[spec("cluster-0"), spec("cluster-1"), spec("cluster-0")],
        )
        .await
        .expect_err("second child with the same name violates UNIQUE");
    assert!(
        matches!(err, Error::AppAlreadyExists(ref n) if n == "cluster-0"),
        "{err:?}"
    );
    assert!(
        apps.records().await.unwrap().is_empty(),
        "nothing of the group survived"
    );

    let (parent, kids) = apps
        .insert_group(&spec("cluster"), &[spec("cluster-0"), spec("cluster-1")])
        .await
        .unwrap();
    assert_eq!(kids.len(), 2);
    match apps.record_by_id(kids[1]).await.unwrap().unwrap() {
        AppRecord::Ok {
            parent_id, desired, ..
        } => {
            assert_eq!(parent_id, Some(parent));
            assert_eq!(
                desired,
                oxidepm_core::Desired::Stopped,
                "the first launch flips it"
            );
        }
        other => panic!("{other:?}"),
    }
    match apps.record_by_id(parent).await.unwrap().unwrap() {
        AppRecord::Ok {
            parent_id, desired, ..
        } => {
            assert_eq!((parent_id, desired), (None, oxidepm_core::Desired::Stopped));
        }
        other => panic!("{other:?}"),
    }
    db.close().await;
}

#[tokio::test]
async fn delete_parent_cascades_children() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    let (parent, _) = apps
        .insert_group(&spec("cluster"), &[spec("cluster-0"), spec("cluster-1")])
        .await
        .unwrap();
    let other = insert(&apps, &spec("other")).await.unwrap();

    assert!(apps.delete(parent).await.unwrap());
    let left: Vec<String> = apps
        .records()
        .await
        .unwrap()
        .iter()
        .map(|r| r.name().to_string())
        .collect();
    assert_eq!(left, vec!["other"]);
    assert!(apps.record_by_id(other).await.unwrap().is_some());
    assert!(
        !apps.delete(parent).await.unwrap(),
        "second delete finds nothing"
    );
    db.close().await;
}

#[tokio::test]
async fn unique_name_violation_maps_to_app_exists() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    insert(&apps, &spec("web")).await.unwrap();
    let err = insert(&apps, &spec("web"))
        .await
        .expect_err("duplicate name");
    assert!(
        matches!(err, Error::AppAlreadyExists(ref n) if n == "web"),
        "{err:?}"
    );
    db.close().await;
}

#[tokio::test]
async fn unparsable_spec_json_row_is_returned_as_broken() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    let good = insert(&apps, &spec("good")).await.unwrap();
    let bad = insert(&apps, &spec("bad")).await.unwrap();
    let removed_field = insert(&apps, &spec("old")).await.unwrap();
    sqlx::query("UPDATE apps SET spec_json = '{not json' WHERE id = ?")
        .bind(bad as i64)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE apps SET spec_json = json_set(spec_json, '$.unknown_options', json('{\"x\":1}')) WHERE id = ?",
    )
    .bind(removed_field as i64)
    .execute(db.pool())
    .await
    .unwrap();

    let all = apps.records().await.unwrap();
    assert_eq!(all.len(), 3, "one bad row does not hide the others");
    assert!(matches!(&all[0], AppRecord::Ok { spec, .. } if spec.id == good));
    match &all[1] {
        AppRecord::Broken {
            id,
            name,
            mode,
            error,
            raw,
            desired,
            ..
        } => {
            assert_eq!((*id, name.as_str(), mode.as_str()), (bad, "bad", "cmd"));
            assert_eq!(raw, "{not json");
            assert!(!error.is_empty());
            assert_eq!(*desired, Desired::Running);
        }
        other => panic!("{other:?}"),
    }
    match &all[2] {
        AppRecord::Broken { error, .. } => assert!(error.contains("unknown_options"), "{error}"),
        other => panic!("{other:?}"),
    }

    // it can still be deleted
    assert!(apps.delete(bad).await.unwrap());
    db.close().await;
}

#[tokio::test]
async fn row_whose_columns_disagree_with_the_spec_is_broken() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    let id = insert(&apps, &spec("web")).await.unwrap();
    sqlx::query("UPDATE apps SET name = 'renamed' WHERE id = ?")
        .bind(id as i64)
        .execute(db.pool())
        .await
        .unwrap();
    match apps.record_by_id(id).await.unwrap().unwrap() {
        AppRecord::Broken { error, .. } => assert!(error.contains("name column"), "{error}"),
        other => panic!("{other:?}"),
    }
    db.close().await;
}

#[tokio::test]
async fn update_spec_and_set_desired_keep_created_at_and_report_missing_rows() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    let mut s = spec("web");
    s.created_at = "2020-01-01T00:00:00Z".parse().unwrap();
    let id = apps.insert_with(&s, None, Desired::Running).await.unwrap();

    let mut changed = spec("web");
    changed.command = "other".to_string();
    changed.tags = vec!["t".to_string()];
    apps.update_spec(id, &changed).await.unwrap();
    apps.set_desired(id, Desired::Stopped).await.unwrap();

    match apps.record_by_id(id).await.unwrap().unwrap() {
        AppRecord::Ok {
            spec,
            desired,
            parent_id,
        } => {
            assert_eq!(spec.command, "other");
            assert_eq!(spec.tags, vec!["t"]);
            assert_eq!(spec.created_at, s.created_at, "created_at is set once");
            assert_eq!(desired, Desired::Stopped);
            assert_eq!(parent_id, None);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        apps.update_spec(999, &changed).await,
        Err(Error::AppNotFound(_))
    ));
    assert!(matches!(
        apps.set_desired(999, Desired::Running).await,
        Err(Error::AppNotFound(_))
    ));
    db.close().await;
}

// ---- migration 2: the policy markers (17c) -----------------------------------------------

#[tokio::test]
async fn migration2_adds_marker_columns_and_keeps_rows() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    // a database of the release before the markers: schema v1 only
    let v1 = &oxidepm_db::schema::MIGRATIONS[..1];
    let db = Database::open(&path, v1).await.unwrap();
    for name in ["a", "b"] {
        let mut s = spec(name);
        s.created_at = chrono::Utc::now();
        let json = serde_json::to_string(&s).unwrap();
        sqlx::query(
            "INSERT INTO apps (name, mode, parent_id, desired_state, spec_json, created_at, updated_at) \
             VALUES (?, 'cmd', NULL, 'running', ?, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .bind(name)
        .bind(json)
        .execute(db.pool())
        .await
        .unwrap();
    }
    assert_eq!(user_version(db.pool()).await.unwrap(), 1);
    db.close().await;

    let db = Database::new(&path).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 2);
    assert!(
        backup_path(&path, 2).exists(),
        "the v1 file is copied before it is migrated"
    );
    let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('apps')")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        cols.contains(&"protected".to_string()) && cols.contains(&"ai_control".to_string()),
        "{cols:?}"
    );
    let rows = db.apps().records().await.unwrap();
    assert_eq!(rows.len(), 2, "the rows are kept");
    for record in rows {
        match record {
            oxidepm_db::AppRecord::Ok { spec, desired, .. } => {
                assert!(
                    !spec.protected && !spec.ai_control,
                    "old rows get both markers off"
                );
                assert_eq!(desired, Desired::Running);
            }
            other => panic!("{other:?}"),
        }
    }
    db.close().await;
}

#[tokio::test]
async fn migrates_state_file_written_by_older_sqlite() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    // This schema v1 fixture was written with SQLite 3.44.0.
    std::fs::write(&path, include_bytes!("fixtures/state-v1.db")).unwrap();

    let db = Database::new(&path).await.unwrap();
    assert_eq!(user_version(db.pool()).await.unwrap(), 2);
    let app = db.apps().record_by_name("web").await.unwrap().unwrap();
    let AppRecord::Ok {
        spec,
        desired,
        parent_id,
    } = app
    else {
        panic!("the stored app could not be read: {app:?}");
    };
    assert_eq!(spec.id, 1);
    assert_eq!(spec.command, "sleep");
    assert_eq!(spec.args, ["300"]);
    assert_eq!(spec.cwd, Path::new("/srv/web"));
    assert_eq!(desired, Desired::Stopped);
    assert_eq!(parent_id, None);
    assert!(!spec.protected && !spec.ai_control);
    db.close().await;

    let backup = Database::open(&backup_path(&path, 2), &oxidepm_db::schema::MIGRATIONS[..1])
        .await
        .unwrap();
    assert_eq!(user_version(backup.pool()).await.unwrap(), 1);
    assert_eq!(names(&backup).await, ["web"]);
    backup.close().await;
}

#[tokio::test]
async fn markers_roundtrip() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();

    let mut a = spec("a");
    a.protected = true;
    let id_a = apps.insert_with(&a, None, Desired::Running).await.unwrap();
    let id_b = apps
        .insert_with(&spec("b"), None, Desired::Running)
        .await
        .unwrap();
    let markers = |r: oxidepm_db::AppRecord| match r {
        oxidepm_db::AppRecord::Ok { spec, .. } => (spec.protected, spec.ai_control),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        markers(apps.record_by_id(id_a).await.unwrap().unwrap()),
        (true, false)
    );
    assert_eq!(
        markers(apps.record_by_id(id_b).await.unwrap().unwrap()),
        (false, false)
    );

    // one field at a time, or both; the other is left as it was
    assert_eq!(
        apps.set_markers(&[id_a, id_b], None, Some(true))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        markers(apps.record_by_id(id_a).await.unwrap().unwrap()),
        (true, true)
    );
    assert_eq!(
        markers(apps.record_by_id(id_b).await.unwrap().unwrap()),
        (false, true)
    );
    apps.set_markers(&[id_a], Some(false), Some(false))
        .await
        .unwrap();
    assert_eq!(
        markers(apps.record_by_name("a").await.unwrap().unwrap()),
        (false, false)
    );
    assert_eq!(
        markers(apps.records().await.unwrap().into_iter().nth(1).unwrap()),
        (false, true)
    );

    // the column is what counts: a stale copy in spec_json is overridden
    let stale = apps.record_by_id(id_b).await.unwrap().unwrap();
    if let oxidepm_db::AppRecord::Ok { mut spec, .. } = stale {
        spec.protected = true;
        spec.ai_control = false;
        // update_spec writes the spec but never the markers
        apps.update_spec(id_b, &spec).await.unwrap();
    }
    assert_eq!(
        markers(apps.record_by_id(id_b).await.unwrap().unwrap()),
        (false, true)
    );

    // an unknown id changes nothing at all
    let err = apps
        .set_markers(&[id_a, 999], Some(true), None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AppNotFound(_)), "{err:?}");
    assert_eq!(
        markers(apps.record_by_id(id_a).await.unwrap().unwrap()),
        (false, false),
        "rolled back"
    );
    db.close().await;
}

#[tokio::test]
async fn set_markers_of_a_cluster_is_one_transaction() {
    let tmp = tempdir().unwrap();
    let db = Database::new(&tmp.path().join("state.db")).await.unwrap();
    let apps = db.apps();
    let parent = spec("web");
    let children: Vec<AppSpec> = (0..3).map(|i| parent.for_instance(i, None)).collect();
    let (pid, cids) = apps.insert_group(&parent, &children).await.unwrap();
    let mut all = vec![pid];
    all.extend(cids);
    assert_eq!(
        apps.set_markers(&all, Some(true), Some(true))
            .await
            .unwrap(),
        4
    );
    for record in apps.records().await.unwrap() {
        match record {
            oxidepm_db::AppRecord::Ok { spec, .. } => {
                assert!(spec.protected && spec.ai_control, "{}", spec.name)
            }
            other => panic!("{other:?}"),
        }
    }
    db.close().await;
}
