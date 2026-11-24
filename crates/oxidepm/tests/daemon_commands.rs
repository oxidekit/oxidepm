//! Daemon-facing CLI behaviour: auto-start rules, log routing, env commands, top.

mod common;

use common::{MockDaemon, TestHome};
use oxidepm_core::{AppInfo, AppSpec, RunState, Runtime};
use oxidepm_ipc::{Request, Response};
use std::path::PathBuf;
use std::time::Duration;

fn app_info(name: &str, env: &[(&str, &str)]) -> AppInfo {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        PathBuf::from("/tmp"),
    );
    for (k, v) in env {
        spec.env.insert(k.to_string(), v.to_string());
    }
    AppInfo::new(spec, RunState::new(1))
}

// ---- ping / kill / status must not start a daemon ----

#[test]
fn ping_without_daemon_fails_and_starts_nothing() {
    let home = TestHome::new();
    let out = home.cmd().arg("ping").output().unwrap();
    assert!(!out.status.success(), "ping must fail when no daemon runs");
    let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(text.contains("not running"), "stderr: {text}");
    assert!(!home.socket().exists(), "ping must not spawn a daemon");
}

#[test]
fn status_without_daemon_fails_and_starts_nothing() {
    let home = TestHome::new();
    let out = home.cmd().arg("status").output().unwrap();
    assert!(!out.status.success());
    assert!(!home.socket().exists(), "status must not spawn a daemon");
}

#[test]
fn kill_without_daemon_says_not_running_and_starts_nothing() {
    let home = TestHome::new();
    let out = home.cmd().arg("kill").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(out.status.success(), "nothing to kill is not an error");
    assert!(stdout.contains("not running"), "stdout: {stdout}");
    assert!(!home.socket().exists(), "kill must not spawn a daemon");
}

#[test]
fn kill_fails_when_daemon_stays_up_without_answering() {
    let home = TestHome::new();
    // Accepts the connection, never answers and keeps listening.
    let daemon = MockDaemon::start(&home, |_| None);
    let out = home.cmd().arg("kill").output().unwrap();
    assert!(
        !out.status.success(),
        "kill must not claim success while the daemon is still up; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(matches!(
        daemon.requests.lock().unwrap().as_slice(),
        [Request::Kill]
    ));
}

#[test]
fn kill_reports_daemon_answer() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::Kill => Some(Response::ok("Daemon stopped")),
        _ => None,
    });
    let out = home.cmd().arg("kill").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Daemon stopped"));
    assert_eq!(daemon.request_count(), 1);
}

// ---- tracing output goes to stderr ----

#[test]
fn verbose_logs_do_not_pollute_json_stdout() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Status => Some(Response::Status { apps: vec![] }),
        _ => None,
    });
    let out = home
        .cmd()
        .args(["-vv", "--json", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document ({e}): {stdout}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Sent request"),
        "debug logs should be on stderr: {stderr}"
    );
}

// ---- env set/unset must not pretend, secrets stay masked ----

#[test]
fn env_set_is_not_supported_and_never_prints_the_value() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Show { .. } => Some(Response::Show {
            app: app_info("web", &[]),
        }),
        _ => None,
    });
    let out = home
        .cmd()
        .args(["env", "set", "web", "API_TOKEN=s3cretvalue"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "env set must fail until supported");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("not supported"), "output: {all}");
    assert!(!all.contains("s3cretvalue"), "secret leaked: {all}");
    assert!(!all.contains('✓'), "must not print a success mark: {all}");
}

#[test]
fn env_unset_is_not_supported() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Show { .. } => Some(Response::Show {
            app: app_info("web", &[("FOO", "bar")]),
        }),
        _ => None,
    });
    let out = home
        .cmd()
        .args(["env", "unset", "web", "FOO"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not supported"), "stderr: {stderr}");
}

// Redaction is the daemon's job (it sends markers instead of values); the CLI prints what it
// is given and does not second-guess key names.

#[test]
fn env_list_prints_what_the_daemon_returns() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Show { .. } => Some(Response::Show {
            app: app_info(
                "web",
                &[("MY_TOKEN", "<redacted>"), ("PLAIN", "<redacted>")],
            ),
        }),
        _ => None,
    });
    let out = home.cmd().args(["env", "list", "web"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MY_TOKEN=<redacted>"), "stdout: {stdout}");
    assert!(stdout.contains("PLAIN=<redacted>"), "stdout: {stdout}");
}

#[test]
fn describe_prints_what_the_daemon_returns() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Describe { .. } => Some(Response::Described {
            name: "web".into(),
            command: "sleep".into(),
            args: vec![],
            cwd: "/tmp".into(),
            env: [("DB_PASSWORD".to_string(), "<redacted>".to_string())]
                .into_iter()
                .collect(),
            runtime: "cmd".into(),
        }),
        _ => None,
    });
    let out = home.cmd().args(["describe", "web"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("DB_PASSWORD=<redacted>"),
        "stdout: {stdout}"
    );
}

// `show --reveal` is the only way to ask the daemon for real env values: it sets the flag in
// the request envelope and nothing else changes.

#[test]
fn show_sends_reveal_only_when_asked() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::Show { .. } => Some(Response::Show {
            app: app_info("web", &[("FOO", "bar")]),
        }),
        _ => None,
    });

    let plain = home.cmd().args(["show", "web"]).output().unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("FOO=bar"));
    let revealed = home
        .cmd()
        .args(["show", "--reveal", "web"])
        .output()
        .unwrap();
    assert!(
        revealed.status.success(),
        "{}",
        String::from_utf8_lossy(&revealed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&revealed.stdout).contains("FOO=bar"),
        "show --reveal prints what the daemon revealed: {}",
        String::from_utf8_lossy(&revealed.stdout)
    );

    let envelopes = daemon.envelopes.lock().unwrap();
    assert_eq!(envelopes.len(), 2);
    assert!(!envelopes[0].reveal, "a plain show must not ask for values");
    assert!(envelopes[1].reveal, "show --reveal must set the flag");
}

// ---- top honours --json ----

#[test]
fn top_rejects_json_mode() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Status => Some(Response::Status {
            apps: vec![app_info("web", &[])],
        }),
        _ => None,
    });
    let out = home
        .cmd()
        .timeout(Duration::from_secs(5))
        .args(["--json", "top"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--json"), "stderr: {stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("oxidepm top"),
        "top must not print its table in json mode"
    );
}

// ---- the startup generator must match the real CLI surface ----

#[test]
fn cli_has_no_daemon_subcommand() {
    let home = TestHome::new();
    let out = home.cmd().arg("daemon").output().unwrap();
    assert!(!out.status.success(), "`oxidepm daemon` must not exist");
}

// ---- who is asking: OXIDEPM_SOCKET and OXIDEPM_ACTOR ----

/// `OXIDEPM_SOCKET` names the socket the CLI talks to (instead of `{home}/daemon.sock`) and
/// `OXIDEPM_ACTOR` is sent as the actor claim of every request; with neither set the claim is
/// absent.
#[test]
fn cli_uses_oxidepm_socket_and_oxidepm_actor() {
    let home = TestHome::new();
    let elsewhere = home.path().join("elsewhere.sock");
    let daemon = MockDaemon::start_at(&elsewhere, |r| match r {
        Request::Ping => Some(Response::Pong),
        _ => None,
    });

    let out = home
        .cmd()
        .env("OXIDEPM_SOCKET", &elsewhere)
        .env("OXIDEPM_ACTOR", "ai:example-agent")
        .arg("ping")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !home.socket().exists(),
        "nothing was started or created at the default socket"
    );
    {
        let envelopes = daemon.envelopes.lock().unwrap();
        assert_eq!(envelopes.len(), 1, "the request went to OXIDEPM_SOCKET");
        assert_eq!(envelopes[0].actor.as_deref(), Some("ai:example-agent"));
    }

    // an empty OXIDEPM_ACTOR is no claim
    let out = home
        .cmd()
        .env("OXIDEPM_SOCKET", &elsewhere)
        .env("OXIDEPM_ACTOR", "")
        .arg("ping")
        .output()
        .unwrap();
    assert!(out.status.success());
    // and neither variable set: no claim, the default socket
    let default = MockDaemon::start(&home, |_| Some(Response::Pong));
    let out = home
        .cmd()
        .env_remove("OXIDEPM_SOCKET")
        .env_remove("OXIDEPM_ACTOR")
        .arg("ping")
        .output()
        .unwrap();
    assert!(out.status.success());
    let envelopes = daemon.envelopes.lock().unwrap();
    assert_eq!(envelopes.len(), 2);
    assert_eq!(
        envelopes[1].actor, None,
        "an empty OXIDEPM_ACTOR is no claim"
    );
    assert_eq!(default.envelopes.lock().unwrap()[0].actor, None);
}

// ---- policy markers: `policy set` and what `status --more` shows ----

fn marked(name: &str, protected: bool, ai_control: bool) -> AppInfo {
    let mut info = app_info(name, &[]);
    info.spec.protected = protected;
    info.spec.ai_control = ai_control;
    info
}

#[test]
fn status_shows_markers() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::Status => Some(Response::Status {
            apps: vec![
                marked("plain", false, false),
                marked("vault", true, false),
                marked("open", false, true),
                marked("both", true, true),
            ],
        }),
        _ => None,
    });
    let out = home.cmd().args(["status", "--more"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("policy"), "a policy column: {stdout}");
    let row = |name: &str| {
        stdout
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name}: {stdout}"))
            .to_string()
    };
    assert!(
        !row("plain").contains("protected") && !row("plain").contains("ai-control"),
        "{}",
        row("plain")
    );
    assert!(
        row("vault").contains("protected") && !row("vault").contains("ai-control"),
        "{}",
        row("vault")
    );
    assert!(
        row("open").contains("ai-control") && !row("open").contains("protected"),
        "{}",
        row("open")
    );
    assert!(
        row("both").contains("protected") && row("both").contains("ai-control"),
        "{}",
        row("both")
    );

    // the JSON form of the extended status carries both booleans
    let out = home
        .cmd()
        .args(["--json", "status", "--more"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let find = |name: &str| {
        json.as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            find("vault")["protected"].as_bool(),
            find("vault")["ai_control"].as_bool()
        ),
        (Some(true), Some(false))
    );
    assert_eq!(
        (
            find("open")["protected"].as_bool(),
            find("open")["ai_control"].as_bool()
        ),
        (Some(false), Some(true))
    );
}

#[test]
fn status_daemon_reports_the_daemon_state() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |r| match r {
        Request::DaemonStatus => Some(Response::DaemonStatus {
            audit: "degraded".into(),
            audit_lost: 4,
            agent_socket: false,
            adapters: vec![oxidepm_ipc::AdapterStatus {
                name: "check".into(),
                state: "failed".into(),
                note: Some("tampered: the executable changed".into()),
            }],
            warnings: vec!["the audit directory /x/audit is in the way".into()],
            sandbox: Some(oxidepm_ipc::SandboxStatus {
                landlock_abi: Some(10),
                landlock_unix_connect: true,
                seccomp: true,
                no_new_privs: true,
                ptrace_scope: Some(0),
                cgroup_delegated: false,
                launcher: false,
                summary: "landlock abi 10 (restricts unix connect), seccomp, ptrace_scope 0".into(),
            }),
        }),
        _ => None,
    });
    let out = home.cmd().args(["status", "--daemon"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("audit log:") && stdout.contains("degraded"),
        "{stdout}"
    );
    assert!(stdout.contains("lost: 4"), "{stdout}");
    assert!(stdout.contains("not bound"), "{stdout}");
    assert!(
        stdout.contains("adapter check: failed (tampered: the executable changed)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("warning: the audit directory /x/audit is in the way"),
        "{stdout}"
    );
    assert!(
        stdout.contains("sandbox:")
            && stdout.contains("landlock abi 10")
            && stdout.contains("launcher NOT installed"),
        "{stdout}"
    );

    let out = home
        .cmd()
        .args(["--json", "status", "--daemon"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (
            json["audit"].as_str(),
            json["audit_lost"].as_u64(),
            json["agent_socket"].as_bool()
        ),
        (Some("degraded"), Some(4), Some(false))
    );
    assert_eq!(
        json["warnings"][0],
        "the audit directory /x/audit is in the way"
    );
    assert_eq!(
        (
            json["sandbox"]["landlock_abi"].as_u64(),
            json["sandbox"]["launcher"].as_bool()
        ),
        (Some(10), Some(false))
    );

    // the two kinds of status do not mix
    let out = home
        .cmd()
        .args(["status", "--daemon", "--more"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn policy_set_sends_the_request_and_reports_the_markers() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::SetPolicy { .. } => Some(Response::PolicySet {
            count: 1,
            items: vec![oxidepm_ipc::ItemResult {
                id: 3,
                name: "web".into(),
                result: oxidepm_ipc::ItemOutcome::Ok {
                    changed: true,
                    detail: Some("protected=true ai_control=false".into()),
                },
            }],
        }),
        _ => None,
    });
    let cases: [(&[&str], Option<bool>, Option<bool>); 5] = [
        (&["--protected"], Some(true), None),
        (&["--protected=true"], Some(true), None),
        (&["--ai-control=false"], None, Some(false)),
        (
            &["--protected=false", "--ai-control"],
            Some(false),
            Some(true),
        ),
        (
            &["--ai-control=true", "--protected=true"],
            Some(true),
            Some(true),
        ),
    ];
    for (flags, protected, ai_control) in cases {
        let out = home
            .cmd()
            .args(["policy", "set", "web"])
            .args(flags)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{flags:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("web: protected=true ai_control=false"),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let requests = daemon.requests.lock().unwrap();
        match requests.last().unwrap() {
            Request::SetPolicy {
                selector,
                protected: p,
                ai_control: a,
            } => {
                assert_eq!(selector.to_string(), "web");
                assert_eq!((*p, *a), (protected, ai_control), "{flags:?}");
            }
            other => panic!("{other:?}"),
        }
    }
    // `all` and a tag are selectors for a person
    let out = home
        .cmd()
        .args(["policy", "set", "@prod", "--protected"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let requests = daemon.requests.lock().unwrap();
    assert!(
        matches!(requests.last().unwrap(), Request::SetPolicy { selector, .. } if selector.to_string() == "@prod")
    );
}

#[test]
fn policy_set_needs_a_marker_and_reports_a_refusal() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |r| match r {
        Request::SetPolicy { .. } => Some(Response::error(
            oxidepm_core::ErrorCode::PolicyDenied,
            "ai:test may not make the request 'set_policy'",
        )),
        _ => None,
    });
    // neither marker: refused by the argument parser, nothing is sent
    let out = home.cmd().args(["policy", "set", "web"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(daemon.requests.lock().unwrap().is_empty());
    // a refusal from the daemon is an error with its message
    let out = home
        .cmd()
        .env("OXIDEPM_ACTOR", "ai:test")
        .args(["policy", "set", "web", "--protected=false"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("may not make the request"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
