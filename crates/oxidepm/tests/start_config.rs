//! `oxidepm start <config file>` behaviour.

mod common;

use common::{MockDaemon, TestHome};
use oxidepm_core::ErrorCode;
use oxidepm_ipc::{ItemOutcome, ItemResult, Request, Response};

const CONFIG: &str = r#"
[[apps]]
name = "good"
mode = "cmd"
script = "sleep"
args = ["1"]

[[apps]]
name = "bad"
mode = "cmd"
script = "sleep"
args = ["1"]
"#;

fn write_config(home: &TestHome) -> std::path::PathBuf {
    let path = home.path().join("apps.toml");
    std::fs::write(&path, CONFIG).unwrap();
    path
}

/// The whole file goes to the daemon as one batch (an app may be listed before its
/// dependencies); the answer has one item per app.
#[test]
fn config_start_exits_nonzero_and_reports_each_app() {
    let home = TestHome::new();
    let cfg = write_config(&home);
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::StartBatch { specs } => {
            let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["good", "bad"], "the apps in file order");
            Some(Response::StartedBatch {
                items: vec![
                    ItemResult::changed(1, "good"),
                    ItemResult {
                        id: 0,
                        name: "bad".into(),
                        result: ItemOutcome::Err {
                            code: ErrorCode::Internal,
                            message: "boom for bad".into(),
                        },
                    },
                ],
            })
        }
        _ => None,
    });

    let out = home.cmd().arg("start").arg(&cfg).output().unwrap();
    assert!(
        !out.status.success(),
        "a failed app must give a non-zero exit"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("Started good"), "stdout: {stdout}");
    assert!(stderr.contains("bad"), "stderr: {stderr}");
    assert!(stderr.contains("boom for bad"), "stderr: {stderr}");
    assert_eq!(
        daemon.request_count(),
        1,
        "one batch request carries every app"
    );
}

#[test]
fn config_start_succeeds_when_all_apps_start() {
    let home = TestHome::new();
    let cfg = write_config(&home);
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::StartBatch { specs } => Some(Response::StartedBatch {
            items: specs
                .iter()
                .enumerate()
                .map(|(i, s)| ItemResult::changed(i as u32 + 1, s.name.clone()))
                .collect(),
        }),
        _ => None,
    });
    let out = home.cmd().arg("start").arg(&cfg).output().unwrap();
    assert!(out.status.success());
}

#[test]
fn relative_config_paths_send_the_absolute_config_directory() {
    let home = TestHome::new();
    let project = home.path().join("project");
    let nested = project.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    for dir in [&project, &nested] {
        std::fs::write(dir.join("apps.toml"), CONFIG).unwrap();
    }
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::StartBatch { specs } => Some(Response::StartedBatch {
            items: specs
                .iter()
                .enumerate()
                .map(|(i, spec)| ItemResult::changed(i as u32 + 1, spec.name.clone()))
                .collect(),
        }),
        _ => None,
    });

    for (target, dir) in [
        ("apps.toml", &project),
        ("./apps.toml", &project),
        ("nested/apps.toml", &nested),
    ] {
        home.cmd()
            .current_dir(&project)
            .args(["start", target])
            .assert()
            .success();
        let requests = daemon.requests.lock().unwrap();
        let Some(Request::StartBatch { specs }) = requests.last() else {
            panic!("expected a batch start request for {target}");
        };
        let expected = dir.canonicalize().unwrap();
        for spec in specs {
            assert_eq!(spec.cwd.as_os_str(), expected.as_os_str(), "{target}");
        }
    }
    assert_eq!(daemon.request_count(), 3);
}

#[test]
fn config_start_rejects_flags_that_cannot_apply() {
    let home = TestHome::new();
    let cfg = write_config(&home);
    let daemon = MockDaemon::start(&home, |_| {
        Some(Response::error(ErrorCode::Internal, "must not be called"))
    });

    for flag_args in [
        vec!["--name", "x"],
        vec!["--env", "A=1"],
        vec!["--max-restarts", "3"],
        vec!["--port", "3000"],
        vec!["--check"],
        vec!["-i", "2"],
    ] {
        let out = home
            .cmd()
            .arg("start")
            .arg(&cfg)
            .args(&flag_args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{flag_args:?} must be rejected");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(flag_args[0]),
            "error should name {}: {stderr}",
            flag_args[0]
        );
    }
    assert_eq!(daemon.request_count(), 0, "nothing may be started");
}

#[test]
fn start_with_missing_target_does_not_check_cwd_and_start_check_requires_adapter() {
    let home = TestHome::new();
    std::fs::write(home.path().join("package.json"), "{").unwrap();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::Start { spec } => Some(Response::Started {
            id: 1,
            name: spec.name.clone(),
            status: oxidepm_core::AppStatus::Running,
            child_ids: Vec::new(),
        }),
        _ => None,
    });

    for target in ["/bin/sleep", "sleep"] {
        let out = home.cmd().args(["start", target]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let missing = TestHome::new();
    let out = missing
        .cmd()
        .args(["start", "--check", "/bin/sleep"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("oxidepm adapter import check"), "{stderr}");
    assert!(!missing.socket().exists(), "no daemon was started");
    assert_eq!(
        daemon.request_count(),
        2,
        "only the unchecked starts reached the daemon"
    );
}

#[test]
fn start_rejects_removed_preflight_flags_with_adapter_guidance() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |_| {
        Some(Response::error(ErrorCode::Internal, "must not be called"))
    });

    for (flag, guidance) in [
        ("--setup", "oxidepm check <target> --fix"),
        ("--no-check", "Checks now run only with --check"),
    ] {
        let out = home
            .cmd()
            .args(["start", "/bin/sleep", flag])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(flag) && stderr.contains("check adapter"),
            "{stderr}"
        );
        assert!(stderr.contains(guidance), "{stderr}");
    }
    assert_eq!(daemon.request_count(), 0);
}

#[test]
fn config_rejects_legacy_preflight_keys_with_adapter_guidance() {
    use oxidepm_core::ConfigFile;

    for (key, guidance) in [
        ("setup", "oxidepm check <target> --fix"),
        ("no_check", "Checks now run only with --check"),
    ] {
        for content in [
            format!("[[apps]]\nname = \"web\"\n{key} = false"),
            format!("apps:\n  - name: web\n    {key}: false"),
            format!(r#"{{"apps":[{{"name":"web","{key}":false}}]}}"#),
        ] {
            let error = match content.as_bytes()[0] {
                b'[' => ConfigFile::from_toml(&content),
                b'{' => ConfigFile::from_json(&content),
                _ => ConfigFile::from_yaml(&content),
            }
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("app 'web'") && error.contains(key),
                "{error}"
            );
            assert!(
                error.contains("check adapter") && error.contains(guidance),
                "{error}"
            );
        }
    }
}
