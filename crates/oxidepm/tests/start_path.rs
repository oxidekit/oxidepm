//! Working directories and commands sent by `oxidepm start`.

mod common;

use common::{MockDaemon, TestHome};
use oxidepm_core::{AppStatus, ErrorCode};
use oxidepm_ipc::{AdapterRequest, Request, Response};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn admit_start(request: &Request) -> Option<Response> {
    let Request::Start { spec } = request else {
        return None;
    };
    Some(match spec.validate() {
        Ok(()) => Response::Started {
            id: 1,
            name: spec.name.clone(),
            status: AppStatus::Running,
            child_ids: Vec::new(),
        },
        Err(error) => Response::error(ErrorCode::InvalidSpec, error.to_string()),
    })
}

fn assert_start(
    home: &TestHome,
    daemon: &MockDaemon,
    args: &[&str],
    cwd: &Path,
    mode: &str,
    command: &str,
) {
    home.cmd().arg("start").args(args).assert().success();
    let requests = daemon.requests.lock().unwrap();
    let Some(Request::Start { spec }) = requests.last() else {
        panic!("expected a start request for {args:?}");
    };
    assert_eq!(spec.cwd, cwd, "{args:?}");
    assert_eq!(spec.mode.as_str(), mode, "{args:?}");
    assert_eq!(spec.command, command, "{args:?}");
}

#[test]
fn start_bare_python_file_uses_the_current_directory() {
    let home = TestHome::new();
    std::fs::write(home.path().join("app.py"), "print('hello')\n").unwrap();
    let daemon = MockDaemon::start(&home, admit_start);
    assert_start(
        &home,
        &daemon,
        &["app.py", "--name", "hello"],
        &home.path().canonicalize().unwrap(),
        "python",
        "app.py",
    );
}

#[test]
fn start_bare_node_file_uses_the_current_directory() {
    let home = TestHome::new();
    std::fs::write(home.path().join("app.js"), "console.log('hello');\n").unwrap();
    let daemon = MockDaemon::start(&home, admit_start);
    assert_start(
        &home,
        &daemon,
        &["app.js"],
        &home.path().canonicalize().unwrap(),
        "node",
        "app.js",
    );
}

#[test]
fn start_bare_command_distinguishes_a_local_file_from_a_path_lookup() {
    let home = TestHome::new();
    let script = home.path().join("run-app");
    std::fs::write(&script, "#!/bin/sh\necho hello\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = MockDaemon::start(&home, admit_start);
    let cwd = home.path().canonicalize().unwrap();
    assert_start(&home, &daemon, &["run-app"], &cwd, "cmd", "./run-app");
    assert_start(&home, &daemon, &["sleep"], &cwd, "cmd", "sleep");
}

#[test]
fn start_relative_paths_use_the_target_directory() {
    let home = TestHome::new();
    let cwd = home.path().canonicalize().unwrap();
    let nested = cwd.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(cwd.join("package.json"), "{}").unwrap();
    std::fs::write(nested.join("Cargo.toml"), "").unwrap();
    std::fs::write(nested.join("app.go"), "").unwrap();
    std::fs::write(cwd.join("app.py"), "").unwrap();
    let daemon = MockDaemon::start(&home, admit_start);

    for (target, dir, mode, command) in [
        ("./app.py", &cwd, "python", "app.py"),
        ("nested/app.go", &nested, "go", "app.go"),
        (".", &cwd, "npm", "start"),
        ("nested", &nested, "cargo", "."),
        ("missing/app.py", &cwd.join("missing"), "python", "app.py"),
    ] {
        assert_start(&home, &daemon, &[target], dir, mode, command);
    }
}

#[test]
fn start_check_then_start_a_bare_file() {
    let home = TestHome::new();
    std::fs::write(home.path().join("app.py"), "").unwrap();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::Adapter(request) if matches!(request.as_ref(), AdapterRequest::Invoke(_)) => {
            Some(Response::InvokeDone {
                exit_code: 0,
                result: None,
            })
        }
        _ => admit_start(r),
    });
    let cwd = home.path().canonicalize().unwrap();
    assert_start(
        &home,
        &daemon,
        &["--check", "app.py"],
        &cwd,
        "python",
        "app.py",
    );
    let requests = daemon.requests.lock().unwrap();
    let [Request::Adapter(request), Request::Start { .. }] = requests.as_slice() else {
        panic!("expected a preflight request followed by a start request");
    };
    let AdapterRequest::Invoke(invoke) = request.as_ref() else {
        panic!("expected an adapter invocation");
    };
    assert_eq!(invoke.verb, "check");
    assert_eq!(invoke.argv, ["--", "app.py"]);
    assert_eq!(Path::new(&invoke.cwd), cwd);
}
