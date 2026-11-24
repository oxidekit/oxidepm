//! Tests of the step runner and the plan checks (`step.rs`). Programs are the system's
//! `sh`, `head`, `true` and `sleep`; nothing outside a temporary directory is touched.

use super::step::*;
use oxidepm_adapter_proto::messages::{Capture, PlanResult, Step};
use oxidepm_adapter_proto::Limits;
use oxidepm_core::{AppSpec, Error, Runtime};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn rules(programs: &[&str], capture: &[&str], env_set: &[&str]) -> RuntimeRules {
    let own = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    RuntimeRules {
        programs: own(programs),
        capture_programs: own(capture),
        env_set: own(env_set),
        build_timeout_max: Duration::from_secs(60),
        limits: Limits::PROTOCOL_1,
    }
}

fn spec(dir: &Path) -> AppSpec {
    AppSpec::new(
        "step-test".to_string(),
        Runtime::CMD,
        "x".to_string(),
        dir.to_path_buf(),
    )
}

fn step(id: &str, argv: &[&str]) -> Step {
    Step {
        id: id.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env_add: BTreeMap::new(),
        timeout_secs: 20,
        capture: Capture::None,
        capture_max: None,
        optional: false,
        description: format!("test step {id}"),
    }
}

fn plan(program: &str, cwd: &Path) -> PlanResult {
    PlanResult {
        program: program.to_string(),
        args: vec![],
        cwd: cwd.to_string_lossy().to_string(),
        extra_env: BTreeMap::new(),
        binary_path: None,
    }
}

fn is_invalid<T: std::fmt::Debug>(r: Result<T, Error>) -> String {
    match r {
        Err(Error::AdapterInvalid(m)) => m,
        other => panic!("expected E_ADAPTER_INVALID, got {other:?}"),
    }
}

/// A script that records that it ran.
fn tripwire(path: &Path, ran: &Path) {
    std::fs::write(path, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s[s.rfind(')').unwrap_or(0) + 1..]
            .trim_start()
            .starts_with('Z'),
        Err(_) => false,
    }
}

async fn run_one(s: &Step, spec: &AppSpec, r: &RuntimeRules, log: &mut BuildLog) -> StepOutcome {
    let checked = validate_steps(std::slice::from_ref(s), spec, r).unwrap();
    run_step(&checked[0], log).await.unwrap()
}

#[tokio::test]
async fn step_output_over_cap_does_not_grow_memory() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["head"], &["head"], &[]);
    let log_path = dir.path().join("build.log");
    let mut log = BuildLog::create_capped(&log_path, 1024 * 1024).unwrap();

    // 40 MB of output: only a tail, a capped file and a capped capture are kept
    let mut s = step("flood", &["head", "-c", "40000000", "/dev/zero"]);
    s.capture = Capture::Stdout;
    s.capture_max = Some(2000);
    let out = run_one(&s, &spec(dir.path()), &r, &mut log).await;
    assert!(out.success(), "{out:?}");
    assert_eq!(out.output_bytes, 40_000_000);
    assert!(
        out.tail.len() <= STEP_TAIL_BYTES,
        "tail is {} bytes",
        out.tail.len()
    );
    assert!(
        out.tail.len() >= STEP_TAIL_BYTES - 4,
        "the tail keeps the last 64 KiB"
    );
    assert_eq!(out.stdout.as_ref().unwrap().len(), 2000);
    assert!(out.stderr.is_none());
    let size = std::fs::metadata(&log_path).unwrap().len();
    assert!(size <= 1024 * 1024 + 256, "log is {size} bytes");
    assert!(std::fs::read_to_string(&log_path)
        .unwrap()
        .contains("build log cut here"));
    assert_eq!(log.bytes_written(), 1024 * 1024);

    // the real cap is 10 MiB
    assert_eq!(BUILD_LOG_BYTES, 10 * 1024 * 1024);
    assert_eq!(STEP_TAIL_BYTES, 64 * 1024);
}

#[tokio::test]
async fn build_log_is_truncated_at_each_build() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &[], &[]);
    let path = dir.path().join("b.log");
    for word in ["first", "second"] {
        let mut log = BuildLog::create(&path).unwrap();
        let s = step("say", &["sh", "-c", &format!("echo {word}")]);
        assert!(run_one(&s, &spec(dir.path()), &r, &mut log).await.success());
    }
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("second") && !text.contains("first"), "{text}");
}

#[tokio::test]
async fn step_timeout_kills_group() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &[], &[]);
    let pidfile = dir.path().join("grandchild.pid");
    let script = format!("sleep 600 & echo $! > '{}'; wait", pidfile.display());
    let mut s = step("slow", &["sh", "-c", &script]);
    s.timeout_secs = 1;
    let started = std::time::Instant::now();
    let out = run_one(&s, &spec(dir.path()), &r, &mut BuildLog::none()).await;
    assert!(out.timed_out && !out.success(), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "the build's child survived the timeout");
}

#[tokio::test]
async fn dropped_step_kills_group() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &[], &[]);
    let pidfile = dir.path().join("child.pid");
    let script = format!("sleep 600 & echo $! > '{}'; wait", pidfile.display());
    let s = step("long", &["sh", "-c", &script]);
    let checked = validate_steps(std::slice::from_ref(&s), &spec(dir.path()), &r).unwrap();
    let mut log = BuildLog::none();
    // a Stop during the build drops the future
    let _ = tokio::time::timeout(Duration::from_millis(700), run_step(&checked[0], &mut log)).await;
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "the build's child survived the drop");
}

#[test]
fn step_running_env_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    // the manifest lists a toolchain program only: `env`, `sh` and `curl` are not in it
    let r = rules(&["true"], &["true"], &[]);
    for argv in [
        &["env"][..],
        &["/usr/bin/env"],
        &["sh", "-c", "env"],
        &["curl", "http://x"],
    ] {
        let m = is_invalid(validate_steps(&[step("e", argv)], &spec(dir.path()), &r));
        assert!(m.contains("not one the adapter's manifest lists"), "{m}");
    }
    // listing `env` for running does not let its output go back to the adapter
    let r2 = rules(&["env"], &[], &[]);
    let mut s = step("e", &["env"]);
    s.capture = Capture::Both;
    let m = is_invalid(validate_steps(&[s], &spec(dir.path()), &r2));
    assert!(m.contains("may not be captured"), "{m}");
}

#[test]
fn step_program_outside_the_manifest_list_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let evil = dir.path().join("evil");
    tripwire(&evil, &ran);
    let r = rules(&["true"], &[], &[]);
    // an absolute path to a program that is not listed
    let m = is_invalid(validate_steps(
        &[step("a", &[evil.to_str().unwrap()])],
        &spec(dir.path()),
        &r,
    ));
    assert!(m.contains("'evil'"), "{m}");
    // a relative path, even with a listed name
    let m = is_invalid(validate_steps(
        &[step("a", &["./true"])],
        &spec(dir.path()),
        &r,
    ));
    assert!(m.contains("relative"), "{m}");
    assert!(!ran.exists(), "validation must not run anything");
    // an empty argv, a NUL, too many steps, a repeated id
    is_invalid(validate_steps(&[step("a", &[])], &spec(dir.path()), &r));
    is_invalid(validate_steps(
        &[step("a", &["true", "x\0y"])],
        &spec(dir.path()),
        &r,
    ));
    let many: Vec<Step> = (0..17).map(|i| step(&format!("s{i}"), &["true"])).collect();
    is_invalid(validate_steps(&many, &spec(dir.path()), &r));
    is_invalid(validate_steps(
        &[step("a", &["true"]), step("a", &["true"])],
        &spec(dir.path()),
        &r,
    ));
}

#[tokio::test]
async fn step_env_is_the_apps_env_plus_the_adapters_but_a_capturing_step_has_no_app_env() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &["sh"], &["BUILD_MODE"]);
    let mut sp = spec(dir.path());
    sp.env.insert("APP_SECRET".to_string(), "s3".to_string());

    let out_file = dir.path().join("seen");
    let script = format!(
        "printf '%s|%s' \"$APP_SECRET\" \"$BUILD_MODE\" > '{}'",
        out_file.display()
    );
    let mut s = step("env", &["sh", "-c", &script]);
    s.env_add
        .insert("BUILD_MODE".to_string(), "release".to_string());
    assert!(run_one(&s, &sp, &r, &mut BuildLog::none()).await.success());
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "s3|release");

    // the same step with its output captured runs without the app's own env
    let mut c = step(
        "cap",
        &["sh", "-c", "printf '%s|%s' \"$APP_SECRET\" \"$BUILD_MODE\""],
    );
    c.env_add
        .insert("BUILD_MODE".to_string(), "release".to_string());
    c.capture = Capture::Stdout;
    let out = run_one(&c, &sp, &r, &mut BuildLog::none()).await;
    assert_eq!(out.stdout.as_deref(), Some("|release"));
}

#[tokio::test]
async fn extra_env_cannot_override_spec_env() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh", "node"], &[], &["FEATURE"]);
    let mut sp = spec(dir.path());
    sp.env.insert("FEATURE".to_string(), "user".to_string());

    // a step: the app's value stays
    let out_file = dir.path().join("seen");
    let mut s = step(
        "e",
        &[
            "sh",
            "-c",
            &format!("printf %s \"$FEATURE\" > '{}'", out_file.display()),
        ],
    );
    s.env_add
        .insert("FEATURE".to_string(), "adapter".to_string());
    let checked = validate_steps(std::slice::from_ref(&s), &sp, &r).unwrap();
    assert_eq!(checked[0].dropped_env, vec!["FEATURE".to_string()]);
    assert!(run_step(&checked[0], &mut BuildLog::none())
        .await
        .unwrap()
        .success());
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "user");

    // a plan: the key is not in the launch's extra_env
    let mut p = plan("sh", dir.path());
    p.extra_env
        .insert("FEATURE".to_string(), "adapter".to_string());
    let checked = validate_plan(&p, &sp, &r).unwrap();
    assert!(checked.launch.extra_env.is_empty());
    assert_eq!(checked.dropped_env, vec!["FEATURE".to_string()]);
}

#[test]
fn extra_env_outside_env_set_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &[], &["ALLOWED"]);
    let mut s = step("e", &["sh"]);
    s.env_add.insert("OTHER".to_string(), "1".to_string());
    let m = is_invalid(validate_steps(&[s], &spec(dir.path()), &r));
    assert!(m.contains("env_set"), "{m}");
    let mut p = plan("sh", dir.path());
    p.extra_env.insert("OTHER".to_string(), "1".to_string());
    let m = is_invalid(validate_plan(&p, &spec(dir.path()), &r));
    assert!(m.contains("env_set"), "{m}");
    // the listed key passes
    let mut p = plan("sh", dir.path());
    p.extra_env.insert("ALLOWED".to_string(), "1".to_string());
    let ok = validate_plan(&p, &spec(dir.path()), &r).unwrap();
    assert_eq!(
        ok.launch.extra_env,
        vec![("ALLOWED".to_string(), "1".to_string())]
    );
}

#[test]
fn stored_plan_with_ld_preload_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    // even a manifest that lists the key (the proto crate refuses it, but a stored plan was
    // checked by an older core) does not get it through
    let r = rules(
        &["sh"],
        &[],
        &[
            "LD_PRELOAD",
            "PATH",
            "NODE_OPTIONS",
            "ld_library_path",
            "GIT_SSH_COMMAND",
        ],
    );
    for key in [
        "LD_PRELOAD",
        "PATH",
        "NODE_OPTIONS",
        "ld_library_path",
        "GIT_SSH_COMMAND",
        "DYLD_INSERT_LIBRARIES",
    ] {
        let mut p = plan("sh", dir.path());
        p.extra_env
            .insert(key.to_string(), "/tmp/evil.so".to_string());
        let m = is_invalid(validate_plan(&p, &spec(dir.path()), &r));
        assert!(
            m.contains("may never be set") || m.contains("env_set"),
            "{key}: {m}"
        );
    }
    // the same through a JSON round trip, as a stored plan comes back
    let json = r#"{"program":"sh","args":[],"cwd":"/","extra_env":{"LD_PRELOAD":"x.so"}}"#;
    let p: PlanResult = serde_json::from_str(json).unwrap();
    is_invalid(validate_plan(&p, &spec(dir.path()), &r));
}

#[test]
fn plan_with_relative_program_or_nul_or_huge_args_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let sp = spec(dir.path());
    let r = rules(&["sh"], &[], &[]);
    let good = plan("sh", dir.path());
    let ok = validate_plan(&good, &sp, &r).unwrap();
    assert!(ok.launch.program.is_absolute());
    assert_eq!(ok.launch.cwd, dir.path());

    let mut p = good.clone();
    p.program = "./sh".to_string();
    assert!(is_invalid(validate_plan(&p, &sp, &r)).contains("relative"));
    p.program = "bin/sh".to_string();
    is_invalid(validate_plan(&p, &sp, &r));

    let mut p = good.clone();
    p.program = "s\0h".to_string();
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.args = vec!["a\0b".to_string()];
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.cwd = format!("{}\0x", dir.path().display());
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.extra_env.insert("A".to_string(), "x\0y".to_string());
    is_invalid(validate_plan(&p, &sp, &r));

    let mut p = good.clone();
    p.args = vec!["x".to_string(); 4097];
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.args = vec!["y".repeat(1024); 129];
    is_invalid(validate_plan(&p, &sp, &r));

    // a relative or missing cwd, a program that is not listed
    let mut p = good.clone();
    p.cwd = "relative/dir".to_string();
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.cwd = dir.path().join("nope").to_string_lossy().to_string();
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good.clone();
    p.program = "env".to_string();
    is_invalid(validate_plan(&p, &sp, &r));
    let mut p = good;
    p.binary_path = Some("relative/bin".to_string());
    is_invalid(validate_plan(&p, &sp, &r));
}

#[test]
fn plan_whose_program_is_not_installed_is_a_start_failure_not_an_invalid_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["definitely-not-installed-xyz"], &[], &[]);
    let p = plan("definitely-not-installed-xyz", dir.path());
    match validate_plan(&p, &spec(dir.path()), &r) {
        Err(Error::ProcessStartFailed(m)) => assert!(m.contains("not found in PATH"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn step_timeout_is_capped_by_the_core() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["sh"], &[], &[]);
    let mut s = step("t", &["sh"]);
    s.timeout_secs = 86_400;
    let checked = validate_steps(&[s], &spec(dir.path()), &r).unwrap();
    assert_eq!(checked[0].timeout, Duration::from_secs(60));
}

/// A plan that runs `binary` as the program the build made.
fn built_plan(binary: &Path, cwd: &Path) -> PlanResult {
    let mut p = plan(&binary.to_string_lossy(), cwd);
    p.binary_path = Some(binary.to_string_lossy().to_string());
    p
}

#[test]
fn plan_may_run_the_binary_its_build_made() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("demo");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // the manifest lists a builder and not the binary
    let r = rules(&["cargo"], &[], &[]);
    let ok = validate_plan(&built_plan(&bin, dir.path()), &spec(dir.path()), &r).unwrap();
    assert_eq!(ok.launch.program, bin);
    assert_eq!(ok.binary_path, Some(bin.clone()));
}

#[test]
fn plan_cannot_name_a_system_program_as_the_binary_it_built() {
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipping: the system shell may be owned by the test user (root)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["cargo"], &[], &[]);
    // Resolve the shell so the ownership check is reached even when /bin/sh is a symlink.
    let sh = std::fs::canonicalize("/bin/sh").expect("resolve the system shell");
    let msg = is_invalid(validate_plan(
        &built_plan(&sh, dir.path()),
        &spec(dir.path()),
        &r,
    ));
    assert!(msg.contains("not owned"), "{msg}");
    // naming the program without saying it was built is the program list's business as before
    let mut p = plan(&sh.to_string_lossy(), dir.path());
    p.binary_path = None;
    assert!(is_invalid(validate_plan(&p, &spec(dir.path()), &r))
        .contains("not one the adapter's manifest lists"));
    // and a binary_path that differs from the program does not open the list
    p.binary_path = Some("/somewhere/else".to_string());
    assert!(is_invalid(validate_plan(&p, &spec(dir.path()), &r))
        .contains("not one the adapter's manifest lists"));
}

#[test]
fn the_binary_a_build_made_must_be_a_regular_executable_file_that_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let r = rules(&["cargo"], &[], &[]);
    let sp = spec(dir.path());
    // missing
    let missing = dir.path().join("missing");
    assert!(
        is_invalid(validate_plan(&built_plan(&missing, dir.path()), &sp, &r)).contains("not there")
    );
    // not executable
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "x").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        is_invalid(validate_plan(&built_plan(&plain, dir.path()), &sp, &r))
            .contains("not an executable file")
    );
    // a link, even to something executable
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(which::which("sh").unwrap(), &link).unwrap();
    assert!(
        is_invalid(validate_plan(&built_plan(&link, dir.path()), &sp, &r))
            .contains("not an executable file")
    );
    // a directory
    assert!(
        is_invalid(validate_plan(&built_plan(dir.path(), dir.path()), &sp, &r))
            .contains("not an executable file")
    );
    // a relative path
    let mut p = plan("bin/demo", dir.path());
    p.binary_path = Some("bin/demo".to_string());
    assert!(is_invalid(validate_plan(&p, &sp, &r)).contains("not absolute"));
}

#[tokio::test]
async fn a_capturing_interpreter_resolves_on_the_app_path_with_a_scrubbed_environment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom-bin");
    std::fs::create_dir(&path).unwrap();
    let python = path.join("python");
    std::fs::write(
        &python,
        b"#!/bin/sh\nprintf '%s|%s' \"$PATH\" \"$APP_SECRET\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut app = spec(dir.path());
    app.env.insert("PATH".into(), "custom-bin".into());
    app.env.insert("APP_SECRET".into(), "private".into());
    let mut probe = step("interpreter", &["python"]);
    probe.capture = Capture::Stdout;
    let r = rules(&["python"], &["python"], &[]);
    let daemon_env = vec![("PATH".into(), "/usr/bin:/bin".into())];

    let checked = validate_steps_with(&[probe], &app, &r, &daemon_env).unwrap();
    assert_eq!(checked[0].program, python);
    let output = run_step(&checked[0], &mut BuildLog::none()).await.unwrap();
    assert_eq!(output.stdout.as_deref(), Some("/usr/bin:/bin|"));
}

#[test]
fn capturing_step_does_not_inherit_the_daemon_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = spec(dir.path());
    app.env_inherit = true;
    let mut request = step("metadata", &["true"]);
    request.capture = Capture::Stdout;
    let rules = rules(&["true"], &["true"], &[]);
    let daemon_env = vec![("PATH".into(), "/usr/bin:/bin".into())];

    let checked = validate_steps_with(&[request], &app, &rules, &daemon_env).unwrap();
    assert!(checked[0].env.clear);
}
