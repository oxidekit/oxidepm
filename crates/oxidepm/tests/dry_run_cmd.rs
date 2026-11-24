//! `--dry-run` in the CLI: it is refused where it cannot be honoured, the commands that change
//! something send it to the daemon and show the plan, and the commands that run in the CLI
//! (startup, backup, restore, update) rehearse themselves. No test here starts a real daemon.

mod common;

use common::{MockDaemon, TestHome};
use flate2::write::GzEncoder;
use flate2::Compression;
use oxidepm_core::{ErrorCode, Selector};
use oxidepm_ipc::{DryRunPlan, PlannedItem, Request, Response};
use std::path::Path;

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Everything under `dir` (relative paths), to see that a command made nothing.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            out.push(path.strip_prefix(root).unwrap().display().to_string());
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn plan(op: &str, items: Vec<PlannedItem>) -> Response {
    Response::DryRun {
        plan: DryRunPlan::allowed(op, items, vec!["a warning".to_string()]),
    }
}

// ---- refusals -----------------------------------------------------------------------------------

#[test]
fn cli_refuses_dry_run_on_read_commands() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |_| {
        Some(Response::error(ErrorCode::Internal, "must not be called"))
    });
    let before = tree(home.path());
    for args in [
        vec!["status"],
        vec!["status", "--daemon"],
        vec!["show", "web"],
        vec!["logs", "web"],
        vec!["audit"],
        vec!["ping"],
        vec!["describe", "web"],
        vec!["env", "list", "web"],
        vec!["top"],
        vec!["completions", "bash"],
        vec!["man"],
    ] {
        let out = home.cmd().arg("--dry-run").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.contains("only reads") && text.contains("--dry-run"),
            "{args:?}: {text}"
        );
        assert!(
            text.contains(&format!("'oxidepm {}'", args[0])),
            "{args:?}: {text}"
        );
        // the flag may stand after the command as well
        let out = home.cmd().args(&args).arg("--dry-run").output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} (flag last): {}",
            stderr(&out)
        );
    }
    assert_eq!(daemon.request_count(), 0, "nothing may be sent");
    assert_eq!(tree(home.path()), before, "nothing may be made");
}

#[test]
fn cli_refuses_dry_run_on_commands_without_support() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |_| {
        Some(Response::error(ErrorCode::Internal, "must not be called"))
    });
    let before = tree(home.path());
    for (args, name) in [
        (vec!["env", "set", "web", "A=b"], "env"),
        (vec!["env", "unset", "web", "A"], "env"),
        (vec!["start", "/bin/sleep", "--check"], "start --check"),
    ] {
        let out = home.cmd().arg("--dry-run").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.contains(&format!("'oxidepm {name}' has no --dry-run yet")),
            "{args:?}: {text}"
        );
        assert!(text.contains("Nothing was run"), "{text}");
    }
    assert_eq!(daemon.request_count(), 0);
    assert_eq!(
        tree(home.path()),
        before,
        "nothing may be made (no clone, no config)"
    );
}

// ---- the daemon's plan ----------------------------------------------------------------------------

/// Every command that changes something sends `dry_run` in the envelope and shows the plan.
#[test]
fn dry_run_is_sent_by_every_write_command_and_the_plan_is_shown() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["stop", "web"], "stop"),
        (vec!["restart", "web"], "restart"),
        (vec!["reload", "web"], "restart"),
        (vec!["delete", "web"], "delete"),
        (vec!["flush", "web"], "flush"),
        (vec!["save"], "save"),
        (vec!["save", "--force"], "save"),
        (vec!["resurrect"], "resurrect"),
        (vec!["kill"], "kill"),
        (vec!["policy", "set", "web", "--protected"], "set_policy"),
        (vec!["start", "-n", "web", "/bin/sleep"], "start"),
    ];
    for (args, kind) in cases {
        let home = TestHome::new();
        let daemon = MockDaemon::start(&home, move |request| {
            Some(plan(
                request.name(),
                vec![PlannedItem::new(Some(1), "web", "stop")
                    .order(0)
                    .detail("running")],
            ))
        });
        let out = home.cmd().arg("--dry-run").args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {} {}",
            stdout(&out),
            stderr(&out)
        );
        let text = stdout(&out);
        assert!(
            text.contains("Nothing was changed")
                && text.contains("[step 0]")
                && text.contains("a warning"),
            "{args:?}: {text}"
        );
        let envelopes = daemon.envelopes.lock().unwrap();
        assert_eq!(envelopes.len(), 1, "{args:?}");
        assert!(
            envelopes[0].dry_run,
            "{args:?}: the envelope carries dry_run"
        );
        assert_eq!(envelopes[0].request.name(), kind, "{args:?}");
        // without the flag the envelope does not
        drop(envelopes);
        let out = home.cmd().args(&args).output().unwrap();
        let _ = out;
        let envelopes = daemon.envelopes.lock().unwrap();
        assert!(!envelopes[1].dry_run, "{args:?}: no flag, no dry run");
    }
}

#[test]
fn cli_dry_run_json_is_the_plan() {
    let home = TestHome::new();
    let wanted = DryRunPlan::allowed(
        "stop",
        vec![
            PlannedItem::new(Some(2), "web", "stop")
                .order(0)
                .detail("status running"),
            PlannedItem::new(Some(1), "db", "stop").order(1),
        ],
        vec![],
    );
    let answer = wanted.clone();
    let _daemon = MockDaemon::start(&home, move |_| {
        Some(Response::DryRun {
            plan: answer.clone(),
        })
    });
    let out = home
        .cmd()
        .args(["--json", "--dry-run", "stop", "all"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let got: DryRunPlan =
        serde_json::from_str(&stdout(&out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(&out)));
    assert_eq!(got, wanted);

    // a plan the policy refuses is shown (as JSON too) and the command fails like the real one
    let denied = DryRunPlan::denied("stop", ErrorCode::PolicyDenied, "ai:x may not stop db");
    let answer = denied.clone();
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, move |_| {
        Some(Response::DryRun {
            plan: answer.clone(),
        })
    });
    let out = home
        .cmd()
        .args(["--json", "--dry-run", "stop", "db"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        serde_json::from_str::<DryRunPlan>(&stdout(&out)).unwrap(),
        denied
    );
    let out = home
        .cmd()
        .args(["--dry-run", "stop", "db"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("refused (E_POLICY_DENIED)"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn control_characters_in_a_plan_do_not_reach_the_terminal() {
    let home = TestHome::new();
    let _daemon = MockDaemon::start(&home, |_| {
        Some(plan(
            "stop",
            vec![PlannedItem::new(None, "a\u{1b}[2Kb", "stop").detail("x\u{1b}[31my")],
        ))
    });
    let out = home
        .cmd()
        .args(["--dry-run", "stop", "a"])
        .output()
        .unwrap();
    assert!(!stdout(&out).contains('\u{1b}'), "{:?}", stdout(&out));
}

/// A rehearsal never starts a daemon: without one there is nothing to rehearse against.
#[test]
fn dry_run_start_and_resurrect_do_not_start_a_daemon() {
    for args in [vec!["start", "-n", "web", "/bin/sleep"], vec!["resurrect"]] {
        let home = TestHome::new();
        let out = home.cmd().arg("--dry-run").args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr(&out).to_lowercase().contains("not running"),
            "{args:?}: {}",
            stderr(&out)
        );
        assert!(!home.socket().exists(), "{args:?}: no daemon was started");
    }
}

#[test]
fn dry_run_selectors_reach_the_daemon_as_typed() {
    let home = TestHome::new();
    let daemon = MockDaemon::start(&home, |request| Some(plan(request.name(), vec![])));
    let out = home
        .cmd()
        .args(["--dry-run", "restart", "@front"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let requests = daemon.requests.lock().unwrap();
    assert!(
        matches!(&requests[0], Request::Restart { selector } if *selector == Selector::ByTag("front".into())),
        "{:?}",
        requests[0]
    );
    assert!(stdout(&out).contains("nothing to do"));
}

// ---- startup ----------------------------------------------------------------------------------------

/// The directory of the CLI binary has the daemon next to it (a `cargo test --workspace` builds
/// it; a lone `cargo test -p oxidepm` may not).
fn daemon_is_built() -> bool {
    let cli = assert_cmd::cargo::cargo_bin("oxidepm");
    cli.parent()
        .is_some_and(|dir| dir.join("oxidepmd").is_file())
}

/// `sudo`, `systemctl`, `tee` and `launchctl` that only note that they were called.
fn fake_service_tools(home: &TestHome) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = home.path().join("tool-calls.log");
    for tool in ["sudo", "systemctl", "tee", "launchctl"] {
        let script = bin.join(tool);
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"{tool} $@\" >> '{}'\n", calls.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, calls)
}

#[test]
fn startup_dry_run_prints_unit_and_writes_nothing() {
    if !daemon_is_built() {
        eprintln!("skipped: oxidepmd is not built next to the oxidepm test binary");
        return;
    }
    let home = TestHome::new();
    let (bin, calls) = fake_service_tools(&home);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let unit_path = Path::new("/etc/systemd/system/oxidepmd.service");
    let unit_before = std::fs::read(unit_path).ok();
    let before_all = tree(home.path());

    let out = home
        .cmd()
        .env("PATH", &path)
        .env("USER", "tester")
        .args(["startup", "systemd", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{} {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Dry run of 'startup'") && text.contains("Nothing was changed"),
        "{text}"
    );
    assert!(
        text.contains("[Unit]") && text.contains("User=tester") && text.contains("ExecStart="),
        "{text}"
    );
    assert!(
        text.contains("/etc/systemd/system/oxidepmd.service"),
        "{text}"
    );
    assert!(
        !calls.exists(),
        "no tool was run: {:?}",
        std::fs::read_to_string(&calls)
    );
    assert_eq!(
        std::fs::read(unit_path).ok(),
        unit_before,
        "the unit file is as it was"
    );
    assert_eq!(tree(home.path()), before_all, "nothing was made in HOME");

    // --json: the plan, with the unit as the detail of its one item
    let out = home
        .cmd()
        .env("PATH", &path)
        .env("USER", "tester")
        .args(["--json", "startup", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let got: DryRunPlan = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(got.op, "startup");
    assert!(
        ["write", "replace", "unchanged"].contains(&got.items[0].action.as_str()),
        "{got:?}"
    );
    assert!(got.items[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("ExecStart="));

    // the macOS plist the same way, and no Library directory is made
    let out = home
        .cmd()
        .env("PATH", &path)
        .env("USER", "tester")
        .args(["startup", "launchd", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("com.oxidepm.daemon"),
        "{}",
        stdout(&out)
    );
    assert!(!home.path().join("Library").exists());
    assert!(!calls.exists());
}

// ---- backup and restore --------------------------------------------------------------------------------

fn archive(path: &Path, members: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
    for (name, data) in members {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

#[test]
fn restore_dry_run_lists_members_and_extracts_nothing() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"OLD").unwrap();
    let archive_path = home.path().join("b.tar.gz");
    archive(
        &archive_path,
        &[("saved.json", b"NEWER"), ("adapters.db", b"n")],
    );
    let before = tree(home.path());

    let out = home
        .cmd()
        .args(["--dry-run", "restore"])
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{} {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("overwrite")
            && text.contains("saved.json")
            && text.contains("5 B from the archive replace 3 B"),
        "{text}"
    );
    assert!(
        text.contains("restore") && text.contains("adapters.db"),
        "{text}"
    );
    assert!(
        text.contains("needs --yes"),
        "the plan says what a real restore would need: {text}"
    );
    // nothing extracted, nothing moved, no scratch directory left behind
    assert_eq!(std::fs::read(oxhome.join("saved.json")).unwrap(), b"OLD");
    assert!(!oxhome.join("adapters.db").exists());
    assert_eq!(tree(home.path()), before);

    // --json is the plan
    let out = home
        .cmd()
        .args(["--json", "--dry-run", "restore", "--yes"])
        .arg(&archive_path)
        .output()
        .unwrap();
    let got: DryRunPlan = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(got.op, "restore");
    assert_eq!(got.items.len(), 2);
    assert!(got.warnings.iter().all(|w| !w.contains("--yes")), "{got:?}");

    // an archive the real restore refuses is refused by the rehearsal
    let evil = home.path().join("evil.tar.gz");
    archive(&evil, &[("saved.json", b"x"), ("other.txt", b"y")]);
    let out = home
        .cmd()
        .args(["--dry-run", "restore"])
        .arg(&evil)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Unexpected archive member"),
        "{}",
        stderr(&out)
    );
    // and a home that does not exist is not made
    let fresh = TestHome::new();
    let out = fresh
        .cmd()
        .args(["--dry-run", "restore"])
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !fresh.path().join(".oxidepm").exists(),
        "the home was not created"
    );
}

#[test]
fn backup_dry_run_lists_files_and_writes_nothing() {
    let home = TestHome::new();
    let oxhome = home.oxidepm_dir();
    std::fs::write(oxhome.join("saved.json"), b"12345").unwrap();
    let out_file = home.path().join("out.tar.gz");
    let before = tree(home.path());
    let out = home
        .cmd()
        .args(["--dry-run", "backup", "-o"])
        .arg(&out_file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{} {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("add") && text.contains("saved.json") && text.contains("5 B"),
        "{text}"
    );
    assert!(
        text.contains("write") && text.contains("out.tar.gz"),
        "{text}"
    );
    assert!(!out_file.exists());
    assert_eq!(tree(home.path()), before);
}
