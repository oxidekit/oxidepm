//! What the database keeps of an `AppSpec`.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use oxidepm_core::{AppSpec, HealthCheck, Hooks, RestartMode, RestartPolicy, Runtime};
use oxidepm_db::{AppRecord, AppsRepository, Database};
use sqlx::Row;
use tempfile::tempdir;

/// A spec with every field moved off its default.
fn full_spec(name: &str) -> AppSpec {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sh".to_string(),
        PathBuf::from("/srv/app"),
    );
    spec.args = vec!["-c".to_string(), "exec sleep 300".to_string()];
    spec.env = HashMap::from([
        ("A".to_string(), "1".to_string()),
        ("B".to_string(), "two words".to_string()),
    ]);
    spec.restart_policy = RestartPolicy {
        auto_restart: false,
        max_restarts: 7,
        restart_delay_ms: 1234,
        crash_window_secs: 99,
        max_restart_delay_ms: 4000,
        stable_uptime_secs: 17,
    };
    spec.kill_timeout_ms = 4321;
    spec.created_at = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    spec.instances = 3;
    spec.port = Some(4000);
    spec.health_check = Some(HealthCheck {
        http_url: Some("http://localhost:4000/health".to_string()),
        tcp: None,
        script: None,
        expected_status: vec![200, 204],
        interval_secs: 10,
        timeout_secs: 3,
        retries: 2,
        start_period_secs: 7,
        action: oxidepm_core::HealthAction::Restart,
    });
    spec.max_memory_mb = Some(512);
    spec.startup_delay_ms = Some(250);
    spec.env_inherit = true;
    spec.hooks = Hooks {
        on_start: Some("echo start".to_string()),
        on_stop: Some("echo stop".to_string()),
        on_restart: None,
        on_crash: Some("echo crash".to_string()),
        on_error: None,
    };
    spec.tags = vec!["frontend".to_string(), "prod".to_string()];
    spec.max_uptime_secs = Some(3600);
    spec.restart_mode = RestartMode::Always;
    spec.depends_on = vec!["db".to_string()];
    spec.depends_timeout_secs = 17;
    spec.depends_fail_open = true;
    spec.restart_cron = Some("0 4 * * *".to_string());
    spec.log_max_size = Some(1_000_000);
    spec.log_max_files = Some(4);
    spec.log_compress = true;
    spec
}

/// Insert a top-level app that should be running.
async fn insert(apps: &AppsRepository, spec: &AppSpec) -> u32 {
    apps.insert_with(spec, None, oxidepm_core::Desired::Running)
        .await
        .unwrap()
}

/// The spec of a row that reads back.
fn spec_of(record: AppRecord) -> AppSpec {
    match record {
        AppRecord::Ok { spec, .. } => spec,
        other => panic!("the row does not read back: {other:?}"),
    }
}

#[tokio::test]
async fn appspec_roundtrips_through_db() {
    let dir = tempdir().unwrap();
    let db = Database::new(&dir.path().join("rt.db")).await.unwrap();
    let apps = db.apps();

    let spec = full_spec("web");
    let id = insert(&apps, &spec).await;
    let back = spec_of(apps.record_by_id(id).await.unwrap().expect("row exists"));

    let mut want = serde_json::to_value(&spec).unwrap();
    want["id"] = serde_json::json!(id);
    assert_eq!(serde_json::to_value(&back).unwrap(), want);
    assert_eq!(
        back.created_at, spec.created_at,
        "created_at survives exactly"
    );
    assert_eq!(back.tags, vec!["frontend", "prod"]);
    assert_eq!(back.hooks.on_start.as_deref(), Some("echo start"));
    assert_eq!(back.depends_on, vec!["db"]);
    assert_eq!(back.depends_timeout_secs, 17);
    assert!(back.depends_fail_open);
    assert_eq!(back.health_check.as_ref().unwrap().retries, 2);
    assert_eq!(
        back.health_check.as_ref().unwrap().http_url.as_deref(),
        Some("http://localhost:4000/health")
    );

    // the same through the other two readers (by name, and the listing)
    let by_name = spec_of(apps.record_by_name("web").await.unwrap().unwrap());
    assert_eq!(serde_json::to_value(&by_name).unwrap(), want);
    let all = apps.records().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        serde_json::to_value(spec_of(all.into_iter().next().unwrap())).unwrap(),
        want
    );
    db.close().await;
}

#[tokio::test]
async fn tcp_health_check_roundtrips_through_db() {
    let dir = tempdir().unwrap();
    let db = Database::new(&dir.path().join("tcp.db")).await.unwrap();
    let apps = db.apps();

    let mut spec = full_spec("db-probe");
    spec.health_check = Some(HealthCheck {
        http_url: None,
        tcp: Some("localhost:4000".to_string()),
        script: None,
        expected_status: vec![200],
        interval_secs: 7,
        timeout_secs: 2,
        retries: 4,
        start_period_secs: 5,
        action: oxidepm_core::HealthAction::Notify,
    });
    let id = insert(&apps, &spec).await;
    let back = spec_of(apps.record_by_id(id).await.unwrap().expect("row exists"));

    let check = back
        .health_check
        .as_ref()
        .expect("the health check survives");
    assert_eq!(check.tcp.as_deref(), Some("localhost:4000"));
    assert_eq!(check.http_url, None);
    assert_eq!(check.script, None);
    assert_eq!(
        (check.interval_secs, check.timeout_secs, check.retries),
        (7, 2, 4)
    );
    db.close().await;
}

#[tokio::test]
async fn created_at_is_rfc3339() {
    let dir = tempdir().unwrap();
    let db = Database::new(&dir.path().join("ts.db")).await.unwrap();
    insert(&db.apps(), &full_spec("web")).await;

    let row = sqlx::query("SELECT created_at, updated_at FROM apps")
        .fetch_one(db.pool())
        .await
        .unwrap();
    for col in ["created_at", "updated_at"] {
        let text: String = row.get(col);
        chrono::DateTime::parse_from_rfc3339(&text)
            .unwrap_or_else(|e| panic!("{col} {text:?} is not RFC 3339: {e}"));
    }
    let created: String = row.get("created_at");
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&created)
            .unwrap()
            .with_timezone(&Utc),
        Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
    );
    db.close().await;
}

#[tokio::test]
async fn database_new_works_for_path_with_hash_question_percent_space() {
    let dir = tempdir().unwrap();
    let odd = dir.path().join("a #b ?c %d").join("st ate#?%.db");
    let db = Database::new(&odd).await.expect("odd path opens");
    insert(&db.apps(), &full_spec("web")).await;
    assert!(odd.exists());
    db.close().await;
}
