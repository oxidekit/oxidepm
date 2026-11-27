//! Exercise the Linux launcher, its self-test and its enforcement record using a fixture adapter.
//!
//! Tests marked [os] depend on the kernel; they probe first and print why they skip.
//! The launcher enforces only on Linux, so the suite is compiled only there.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use common::fixture::{launcher_bin, pid_alive, wait_state, wait_until, Rig};
use oxidepm_adapter_proto::Kind;
use oxidepmd::adapters::entry::{EntryState, FailReason};
use serde_json::{json, Value};

/// A script that records `what` once the process starts.
fn on_start(actions: Value) -> Value {
    json!([{ "on": "start", "do": actions }])
}

/// A script that replies to `initialize` and then does `actions` (what needs the state directory,
/// or must not delay the handshake).
fn after_initialize(actions: Value) -> Value {
    let mut list = vec![json!("reply")];
    match actions {
        Value::Array(many) => list.extend(many),
        one => list.push(one),
    }
    json!([{ "on": "initialize", "do": list }])
}

/// The first report record of kind `what`, waiting for it.
async fn record(rig: &Rig, what: &str) -> Value {
    assert!(
        wait_until(20, || rig.report().iter().any(|r| r["what"] == what)).await,
        "no '{what}' record: {:?} / {:?}",
        rig.report(),
        rig.entry.view()
    );
    rig.report()
        .into_iter()
        .find(|r| r["what"] == what)
        .unwrap()
}

/// A service in a sandbox with the default permissions and `perms` changed.
fn sandboxed(name: &str, script: Value, perms: Value) -> Rig {
    Rig::sandboxed(name, Kind::Service, script, perms, |cfg| {
        cfg.monitor_interval = Duration::from_millis(100);
    })
}

// ---- 25a: the baseline ---------------------------------------------------------------------

#[tokio::test]
async fn adapter_environment_has_no_daemon_environment_and_no_home_or_socket() {
    let rig = sandboxed("env", on_start(json!("echo_env")), json!({}));
    rig.entry.start();
    let env = record(&rig, "env").await["env"].clone();
    let mut keys: Vec<&str> = env
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "FIXTURE_REPORT",
            "FIXTURE_SCRIPT",
            "HOME",
            "LANG",
            "OXIDEPM_ADAPTER_PROTOCOLS",
            "PATH"
        ],
        "{env}"
    );
    let state = rig.home.join("adapters").join("state").join("env");
    assert_eq!(
        env["HOME"],
        state.display().to_string(),
        "HOME is the adapter's state directory, not the user's"
    );
    for (k, v) in env.as_object().unwrap() {
        assert!(!v.as_str().unwrap().contains("OXIDEPM_"), "{k}={v}");
    }
}

#[tokio::test]
async fn no_new_privs_is_set() {
    let rig = sandboxed("nnp", on_start(json!("echo_nnp")), json!({}));
    rig.entry.start();
    assert_eq!(record(&rig, "nnp").await["no_new_privs"], 1);
}

#[tokio::test]
async fn rlimits_are_applied() {
    let rig = sandboxed(
        "limits",
        on_start(json!("echo_limits")),
        json!({ "memory_mb": 96 }),
    );
    rig.entry.start();
    let r = record(&rig, "limits").await;
    let limits = &r["limits"];
    assert_eq!(limits["nofile"], json!({ "soft": 256, "hard": 256 }), "{r}");
    assert_eq!(
        limits["fsize"],
        json!({ "soft": 64 << 20, "hard": 64 << 20 }),
        "{r}"
    );
    assert_eq!(limits["core"], json!({ "soft": 0, "hard": 0 }), "{r}");
    assert_eq!(
        limits["data"],
        json!({ "soft": 96u64 << 20, "hard": 96u64 << 20 }),
        "{r}"
    );
}

#[tokio::test]
async fn oom_score_adj_is_set_and_data_limit_applies() {
    // the 80 MiB the script asks for do not fit under a 64 MiB data limit
    let rig = sandboxed(
        "oom",
        on_start(json!(["echo_limits", { "alloc": { "mb": 80 } }])),
        json!({ "memory_mb": 64 }),
    );
    rig.entry.start();
    let r = record(&rig, "limits").await;
    assert_eq!(r["oom_score_adj"], 1000, "{r}");
    assert_eq!(r["nice"], 10, "{r}");
    let alloc = record(&rig, "alloc").await;
    assert_eq!(
        alloc["ok"], false,
        "RLIMIT_DATA should refuse 80 MiB: {alloc}"
    );
}

#[test]
fn a_sandboxed_adapter_can_run_node_under_a_bounded_data_limit() {
    use std::os::fd::AsRawFd;

    let path: Vec<_> = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .collect();
    let Some(node) = oxidepm_sandbox::find_program("node", &path) else {
        eprintln!("Skipping sandboxed Node test because node is unavailable on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let permissions = common::fixture::rig_permissions(
        &json!({
            "memory_mb": 128,
            "spawn": { "mode": "any" },
            "fs": { "read": ["any"], "write": ["$STATE"] }
        }),
        dir.path(),
    );
    let profile = oxidepm_sandbox::Profile::from_permissions(
        &permissions,
        &oxidepm_sandbox::ProfileInput {
            state_dir: dir.path(),
            daemon_home: &home,
            cwd: None,
            search_path: &[],
        },
    );
    let shell = std::fs::File::open("/bin/sh").unwrap();
    let mut cmd = std::process::Command::new(launcher_bin());
    cmd.args(["run", "--profile", &profile.to_json(), "--"])
        .args(["-c", "ulimit -d; ulimit -Hd; exec \"$@\"", "sh"])
        .arg(node)
        .args(["-e", "console.log(6 * 7)"])
        .env_clear()
        .env("HOME", dir.path())
        .current_dir(dir.path());
    common::fixture::inherit_descriptors(&mut cmd, vec![(shell.as_raw_fd(), 3)]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "655360\n655360\n42\n"
    );
}

#[tokio::test]
async fn adapter_inherits_only_std_descriptors() {
    let rig = sandboxed("fds", on_start(json!("echo_fds")), json!({}));
    rig.entry.start();
    let fds = record(&rig, "fds").await["fds"].clone();
    // the fixture keeps its own report file open; nothing else may be there
    let report = rig.report_path.display().to_string();
    let others: Vec<u64> = fds
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["target"] != report.as_str())
        .map(|f| f["fd"].as_u64().unwrap())
        .collect();
    assert_eq!(others, [0, 1, 2], "{fds}");
    assert!(
        fds.as_array().unwrap().iter().all(|f| !f["target"]
            .as_str()
            .unwrap()
            .contains("oxidepm-fixture-adapter")),
        "the adapter must not hold its own executable: {fds}"
    );
}

#[test]
fn the_launcher_closes_descriptors_it_was_handed_by_mistake() {
    // every descriptor Rust opens is close-on-exec, so the daemon cannot leak one by accident; this
    // hands the launcher a descriptor that is not, as a bug in a future caller might
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let io = dir.path().join("io");
    std::fs::create_dir_all(&io).unwrap();
    let (script, report) = (io.join("script.json"), io.join("report.jsonl"));
    std::fs::write(
        &script,
        json!([{ "on": "start", "do": "echo_fds" }]).to_string(),
    )
    .unwrap();
    let permissions = common::fixture::rig_permissions(&json!({}), &io);
    let profile = oxidepm_sandbox::Profile::from_permissions(
        &permissions,
        &oxidepm_sandbox::ProfileInput {
            state_dir: &io,
            daemon_home: dir.path(),
            cwd: None,
            search_path: &[],
        },
    );
    let exe = std::fs::File::open(common::fixture::fixture_bin()).unwrap();
    let leaked = std::fs::File::open("/etc/passwd").unwrap();
    let (exe_fd, leaked_fd) = (exe.as_raw_fd(), leaked.as_raw_fd());
    let mut cmd = std::process::Command::new(launcher_bin());
    cmd.arg("run")
        .arg("--profile")
        .arg(profile.to_json())
        .env_clear()
        .env("OXIDEPM_ADAPTER_PROTOCOLS", "1")
        .env("FIXTURE_SCRIPT", &script)
        .env("FIXTURE_REPORT", &report)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    common::fixture::inherit_descriptors(&mut cmd, vec![(exe_fd, 3), (leaked_fd, 9)]);
    // the fixture sees EOF on stdin at once and ends; what matters is that it ran at all
    let out = cmd.output().unwrap();
    assert_ne!(
        out.status.code(),
        Some(126),
        "the launcher started the adapter: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = oxidepm_adapter_fixture::report::read_report(&report);
    let fds = records
        .iter()
        .find(|r| r["what"] == "fds")
        .unwrap_or_else(|| {
            panic!(
                "the fixture reported its descriptors: {records:?} / {:?} / {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
        });
    let numbers: Vec<u64> = fds["fds"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["target"] != report.display().to_string().as_str())
        .map(|f| f["fd"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, [0, 1, 2], "descriptor 9 must be gone: {fds}");
}

#[test]
fn the_launcher_refuses_to_run_without_the_executable_on_descriptor_3() {
    // nothing on descriptor 3 (stdio only): not an adapter to start, and nothing else may be mistaken for one
    let dir = tempfile::tempdir().unwrap();
    let permissions = common::fixture::rig_permissions(&json!({}), dir.path());
    let profile = oxidepm_sandbox::Profile::from_permissions(
        &permissions,
        &oxidepm_sandbox::ProfileInput {
            state_dir: dir.path(),
            daemon_home: dir.path(),
            cwd: None,
            search_path: &[],
        },
    );
    let out = std::process::Command::new(launcher_bin())
        .arg("run")
        .arg("--profile")
        .arg(profile.to_json())
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("descriptor 3"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn the_adapter_runs_as_the_launchers_process_and_the_group_leader() {
    // the launcher replaces itself: the pid the host knows is the adapter's
    let rig = sandboxed("same-pid", on_start(json!("echo_status")), json!({}));
    rig.entry.start();
    let status = record(&rig, "status").await;
    let host_pid = loop {
        if let Some(pid) = rig.entry.view().pid {
            break pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(status["pid"], host_pid, "{status}");
    assert_eq!(
        status["status_pid"],
        host_pid.to_string(),
        "/proc/self is the adapter's own entry: {status}"
    );
    assert_eq!(
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(host_pid as i32)))
            .unwrap()
            .as_raw(),
        host_pid as i32
    );
}

#[tokio::test]
async fn rss_over_limit_kills_the_adapter() {
    // three children of 40 MiB each: every process is under its own 64 MiB data limit, the tree is not
    let rig = sandboxed(
        "fat",
        after_initialize(json!({ "alloc_children": { "count": 3, "mb": 40, "hold_ms": 20000 } })),
        json!({ "memory_mb": 64, "spawn": { "mode": "any" } }),
    );
    rig.entry.start();
    assert!(
        wait_until(30, || rig.entry.view().crashes >= 1).await,
        "{:?}",
        rig.entry.view()
    );
    let view = rig.entry.view();
    assert!(
        view.note.as_deref().is_some_and(|n| n.contains("memory")),
        "{view:?}"
    );
    // killed with its children: nothing of the first life is left
    let first = rig.start_pids()[0];
    assert!(wait_until(10, || !pid_alive(first)).await);
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn fork_storm_is_killed_by_the_thread_cap() {
    // spawn mode `any`: nothing but the sampler stands between the adapter and its forks
    let rig = sandboxed(
        "storm",
        after_initialize(json!({ "fork_bomb": { "max": 200, "hold_ms": 20000 } })),
        json!({ "spawn": { "mode": "any" }, "memory_mb": 256 }),
    );
    rig.entry.start();
    assert!(
        wait_until(30, || rig.entry.view().crashes >= 1).await,
        "{:?}",
        rig.entry.view()
    );
    assert!(
        rig.entry
            .view()
            .note
            .as_deref()
            .is_some_and(|n| n.contains("threads")),
        "{:?}",
        rig.entry.view()
    );
    let first = rig.start_pids()[0];
    assert!(wait_until(10, || !pid_alive(first)).await);
    rig.entry.stop(false).await;
}

#[tokio::test]
async fn state_dir_over_quota_disables_adapter() {
    let rig = Rig::sandboxed(
        "greedy",
        Kind::Service,
        after_initialize(json!({ "fill_state_dir": { "mb": 4 } })),
        json!({}),
        |cfg| {
            cfg.monitor_interval = Duration::from_millis(100);
            cfg.quota_interval = Duration::from_millis(200);
            cfg.state_quota_bytes = 2 * 1024 * 1024;
        },
    );
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 30, |s| *s == EntryState::Disabled).await,
        "{:?}",
        rig.entry.view()
    );
    let view = rig.entry.view();
    assert!(
        view.note
            .as_deref()
            .is_some_and(|n| n.contains("state directory")),
        "{view:?}"
    );
    assert_eq!(view.crashes, 0, "a quota is not a crash");
    let state = rig.home.join("adapters").join("state").join("greedy");
    assert!(state.join("fill-0.bin").exists(), "the data is kept");
    assert!(wait_until(10, || rig.entry.view().pid.is_none_or(|p| !pid_alive(p))).await);
}

#[tokio::test]
async fn five_hundred_concurrent_launcher_spawns_do_not_deadlock() {
    use oxidepm_runtime::{spawn_adapter, AdapterLaunch, SandboxLaunch};

    let dir = tempfile::tempdir().unwrap();
    let exe = std::fs::File::open(common::fixture::fixture_bin()).unwrap();
    let launcher = launcher_bin();
    let profile = oxidepm_sandbox::Profile::from_permissions(
        &common::fixture::rig_permissions(
            &json!({ "fs": { "read": [], "write": [] } }),
            dir.path(),
        ),
        &oxidepm_sandbox::ProfileInput {
            state_dir: dir.path(),
            daemon_home: dir.path(),
            cwd: None,
            search_path: &[],
        },
    )
    .to_json();
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        json!([{ "on": "start", "do": "exit_clean" }]).to_string(),
    )
    .unwrap();
    let env = vec![
        ("FIXTURE_SCRIPT".to_string(), script.display().to_string()),
        (
            "FIXTURE_REPORT".to_string(),
            dir.path().join("report.jsonl").display().to_string(),
        ),
    ];

    let fds_before = std::fs::read_dir("/proc/self/fd").unwrap().count();
    let started = std::time::Instant::now();
    let mut waits = Vec::new();
    for _ in 0..500 {
        let mut child = spawn_adapter(&AdapterLaunch {
            exe: &exe,
            arg0: "storm",
            args: &[],
            env: &env,
            cwd: dir.path(),
            sandbox: Some(SandboxLaunch {
                launcher: &launcher,
                profile_json: &profile,
                allow_unenforced: true,
            }),
            terminal: None,
        })
        .expect("spawn through the launcher");
        // stdin closed: the fixture ends at EOF, whatever the script says
        drop(child.stdin.take());
        waits.push(tokio::spawn(async move {
            let status = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;
            child.disarm();
            status.is_ok()
        }));
    }
    for w in waits {
        assert!(w.await.unwrap(), "a launcher did not finish: deadlock");
    }
    eprintln!("500 launcher spawns took {:?}", started.elapsed());
    drop(exe);
    assert!(
        wait_until(10, || std::fs::read_dir("/proc/self/fd").unwrap().count()
            <= fds_before + 1)
        .await,
        "descriptors leaked"
    );
}

#[tokio::test]
async fn launcher_failure_marks_failed_sandbox() {
    // a profile the launcher cannot read: it exits 126 with one line and the adapter never starts
    let rig = Rig::with_raw_profile("broken", Kind::Service, json!([]), "{not json");
    rig.entry.start();
    assert!(
        wait_state(&rig.entry, 20, |s| matches!(
            s,
            EntryState::Failed(FailReason::Sandbox)
        ))
        .await,
        "{:?}",
        rig.entry.view()
    );
    let note = rig.entry.view().note.unwrap();
    assert!(note.contains("the profile is not valid"), "{note}");
    assert_eq!(rig.starts(), 0, "the adapter never ran");
}

// ---- 25b: Landlock and seccomp ---------------------------------------------------------------

/// Skip (printing why) unless the kernel offers what the test is about.
macro_rules! need {
    ($caps:ident => $cond:expr, $what:expr) => {
        let $caps = oxidepm_sandbox::probe();
        if !$cond {
            eprintln!(
                "skipping {}: {} ({})",
                module_path!(),
                $what,
                $caps.summary()
            );
            return;
        }
        eprintln!("probe says: {}", $caps.summary());
    };
}

/// A true program that exists on this machine.
fn true_program() -> &'static str {
    ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .expect("a `true` program")
}

#[tokio::test]
async fn net_none_blocks_inet_sockets_but_not_unix() {
    need!(caps => caps.seccomp, "no seccomp");
    let rig = sandboxed("nonet", json!([]), json!({}));
    // a listener in the adapter's writable directory: the AF_UNIX connect is allowed there
    let io = rig.dir.path().join("io");
    std::fs::create_dir_all(&io).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(io.join("svc.sock")).unwrap();
    let script = after_initialize(
        json!(["inet_socket", "unix_socket", { "unix_connect": { "path": io.join("svc.sock").display().to_string() } }]),
    );
    rig.set_script(script);
    rig.entry.start();
    let inet = record(&rig, "inet_socket").await;
    assert_eq!(
        (inet["ok"].clone(), inet["os_error"].clone()),
        (json!(false), json!(libc_eperm())),
        "{inet}"
    );
    assert_eq!(record(&rig, "unix_socket").await["ok"], true);
    assert_eq!(
        record(&rig, "unix_connect").await["ok"],
        true,
        "AF_UNIX stays open"
    );
    drop(listener);
}

fn libc_eperm() -> i32 {
    1
}

fn libc_eacces() -> i32 {
    13
}

#[tokio::test]
async fn io_uring_setup_is_denied() {
    need!(caps => caps.seccomp, "no seccomp");
    let rig = sandboxed(
        "uring",
        after_initialize(json!("io_uring_setup")),
        json!({}),
    );
    rig.entry.start();
    let r = record(&rig, "io_uring_setup").await;
    assert_eq!(
        (r["ok"].clone(), r["os_error"].clone()),
        (json!(false), json!(libc_eperm())),
        "{r}"
    );
}

#[tokio::test]
async fn fs_write_outside_state_is_denied() {
    need!(caps => caps.landlock_abi.is_some(), "no Landlock");
    let rig = sandboxed("writer", json!([]), json!({}));
    let outside = rig.dir.path().join("outside.txt");
    let inside = rig.home.join("adapters/state/writer/inside.txt");
    rig.set_script(after_initialize(json!([
        { "touch": { "path": outside.display().to_string() } },
        { "touch": { "path": inside.display().to_string() } },
    ])));
    rig.entry.start();
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .filter(|r| r["what"] == "touch")
            .count()
            == 2)
        .await,
        "{:?}",
        rig.report()
    );
    let touches: Vec<Value> = rig
        .report()
        .into_iter()
        .filter(|r| r["what"] == "touch")
        .collect();
    let outcome = |path: &std::path::Path| {
        touches
            .iter()
            .find(|t| t["path"] == path.display().to_string().as_str())
            .cloned()
            .unwrap()
    };
    let refused = outcome(&outside);
    assert_eq!(
        (refused["ok"].clone(), refused["os_error"].clone()),
        (json!(false), json!(libc_eacces())),
        "{refused}"
    );
    assert_eq!(
        outcome(&inside)["ok"],
        true,
        "the state directory is writable"
    );
    assert!(!outside.exists() && inside.exists());
}

#[tokio::test]
async fn spawn_none_blocks_exec() {
    need!(caps => caps.landlock_abi.is_some() && caps.seccomp, "needs Landlock and seccomp");
    // a Landlock refusal needs an exec that does not fork first (fork is denied by seccomp)
    let rig = sandboxed(
        "noexec",
        after_initialize(
            json!([{ "execve": { "path": true_program() } }, { "exec": { "path": true_program() } }]),
        ),
        json!({}),
    );
    rig.entry.start();
    let refused = record(&rig, "execve").await;
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .any(|r| r["what"] == "execve" && r["state"] == "refused"))
        .await,
        "{:?}",
        rig.report()
    );
    let refused = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "execve" && r["state"] == "refused")
        .unwrap_or(refused);
    assert_eq!(
        refused["os_error"],
        libc_eacces(),
        "EACCES from Landlock: {refused}"
    );
    let forked = record(&rig, "exec").await;
    assert_eq!(forked["ok"], false, "{forked}");
    // (it answered `initialize` before it tried anything, so it becomes ready, whatever the timing)
    assert!(
        wait_state(&rig.entry, 20, |s| *s == EntryState::Ready).await,
        "the adapter itself is untouched: {:?}",
        rig.entry.view()
    );
}

#[tokio::test]
async fn proc_self_is_the_adapters_own_entry() {
    need!(caps => caps.landlock_abi.is_some(), "no Landlock");
    let me = std::process::id();
    let rig = sandboxed(
        "procself",
        after_initialize(
            json!(["echo_status", { "read_file": { "path": format!("/proc/{me}/status") } }, { "read_file": { "path": "/proc/self/status" } }]),
        ),
        json!({}),
    );
    rig.entry.start();
    let status = record(&rig, "status").await;
    assert_eq!(
        status["status_pid"],
        status["pid"].as_u64().unwrap().to_string(),
        "{status}"
    );
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .filter(|r| r["what"] == "read_file")
            .count()
            == 2)
        .await,
        "{:?}",
        rig.report()
    );
    let reads: Vec<Value> = rig
        .report()
        .into_iter()
        .filter(|r| r["what"] == "read_file")
        .collect();
    let other = reads
        .iter()
        .find(|r| r["path"] == format!("/proc/{me}/status").as_str())
        .unwrap();
    assert_eq!(
        (other["ok"].clone(), other["os_error"].clone()),
        (json!(false), json!(libc_eacces())),
        "another process's entry stays closed: {other}"
    );
    let own = reads
        .iter()
        .find(|r| r["path"] == "/proc/self/status")
        .unwrap();
    assert_eq!(own["ok"], true);
}

#[tokio::test]
async fn fs_read_any_excludes_the_daemon_home() {
    need!(caps => caps.landlock_abi.is_some(), "no Landlock");
    // the state directory is not in fs.write: reading it works only because `any` gives it back
    let rig = sandboxed(
        "reader",
        json!([]),
        json!({ "fs": { "read": ["any"], "write": ["$IO"] } }),
    );
    let secret = rig.home.join("state.db");
    std::fs::write(&secret, b"the app list").unwrap();
    let state = rig.home.join("adapters/state/reader");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("own.txt"), b"mine").unwrap();
    let elsewhere = rig.dir.path().join("elsewhere.txt");
    std::fs::write(&elsewhere, b"public").unwrap();
    rig.set_script(after_initialize(json!([
        { "read_file": { "path": secret.display().to_string() } },
        { "read_file": { "path": elsewhere.display().to_string() } },
        { "read_file": { "path": state.join("own.txt").display().to_string() } },
        { "list_dir": { "path": rig.home.display().to_string() } },
        { "read_file": { "path": "/etc/hostname" } },
    ])));
    rig.entry.start();
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .filter(|r| r["what"] == "read_file")
            .count()
            == 4
            && rig.report().iter().any(|r| r["what"] == "list_dir"))
        .await,
        "{:?} {:?} {:?}",
        rig.report(),
        rig.entry.view(),
        rig.entry.stderr_tail()
    );
    let report = rig.report();
    let outcome = |what: &str, path: &std::path::Path| {
        report
            .iter()
            .find(|r| r["what"] == what && r["path"] == path.display().to_string().as_str())
            .cloned()
            .unwrap()
    };
    let denied = outcome("read_file", &secret);
    assert_eq!(
        (denied["ok"].clone(), denied["os_error"].clone()),
        (json!(false), json!(libc_eacces())),
        "reading state.db fails: {denied}"
    );
    assert_eq!(
        outcome("read_file", &elsewhere)["ok"],
        true,
        "reading a file elsewhere works"
    );
    assert_eq!(
        outcome("read_file", &state.join("own.txt"))["ok"],
        true,
        "the adapter's own state stays readable"
    );
    assert_eq!(
        outcome("list_dir", &rig.home)["ok"],
        false,
        "the home cannot even be listed"
    );
}

#[tokio::test]
async fn spawned_children_inherit_the_landlock_domain() {
    need!(caps => caps.landlock_abi.is_some(), "no Landlock");
    let rig = sandboxed(
        "kids",
        json!([]),
        json!({ "spawn": { "mode": "any" }, "fs": { "read": ["$STATE"], "write": ["$STATE", "$IO"] } }),
    );
    let secret = rig.home.join("state.db");
    std::fs::write(&secret, b"the app list").unwrap();
    rig.set_script(after_initialize(json!([
        { "exec_shell": { "cmd": "echo hello" } },
        { "exec_shell": { "cmd": format!("cat {}", secret.display()) } },
    ])));
    rig.entry.start();
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .filter(|r| r["what"] == "exec_shell")
            .count()
            == 2)
        .await,
        "{:?}",
        rig.report()
    );
    let shells: Vec<Value> = rig
        .report()
        .into_iter()
        .filter(|r| r["what"] == "exec_shell")
        .collect();
    let hello = shells.iter().find(|r| r["cmd"] == "echo hello").unwrap();
    assert_eq!(
        hello["stdout"], "hello\n",
        "an allowed program runs: {hello}"
    );
    let cat = shells
        .iter()
        .find(|r| r["cmd"].as_str().unwrap().starts_with("cat "))
        .unwrap();
    assert!(
        cat["stderr"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "the child cannot read the daemon home either: {cat}"
    );
    assert_ne!(cat["status"], 0);
}

#[tokio::test]
async fn spawn_none_denies_fork_and_an_adapter_cannot_signal_the_daemon() {
    need!(caps => caps.landlock_abi.is_some_and(|abi| abi >= 6) && caps.seccomp, "needs Landlock ABI 6 (signal scope) and seccomp");
    // the "daemon" here is this test process: outside the adapter's Landlock domain
    let me = std::process::id() as i32;
    let rig = sandboxed(
        "nofork",
        after_initialize(
            json!([{ "fork_bomb": { "max": 5 } }, { "signal": { "pid": me, "signal": 0 } }]),
        ),
        json!({}),
    );
    rig.entry.start();
    let bomb = record(&rig, "fork_bomb").await;
    assert_eq!(
        (bomb["forks"].clone(), bomb["os_error"].clone()),
        (json!(0), json!(libc_eperm())),
        "fork is denied by seccomp: {bomb}"
    );
    let signal = record(&rig, "signal").await;
    assert_eq!(
        (signal["ok"].clone(), signal["os_error"].clone()),
        (json!(false), json!(libc_eperm())),
        "no signals out of the domain: {signal}"
    );
}

#[tokio::test]
async fn tcp_bind_is_limited_to_the_listed_ports() {
    need!(caps => caps.landlock_abi.is_some_and(|abi| abi >= 4), "Landlock ABI 4 (TCP bind) is not available");
    let free = || {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (allowed, other) = (free(), free());
    let rig = sandboxed(
        "binder",
        after_initialize(json!([
            { "tcp_bind": { "addr": format!("127.0.0.1:{allowed}") } },
            { "tcp_bind": { "addr": format!("127.0.0.1:{other}") } },
        ])),
        json!({ "network": { "mode": "outbound", "listen": [format!("127.0.0.1:{allowed}")] } }),
    );
    rig.entry.start();
    assert!(
        wait_until(20, || rig
            .report()
            .iter()
            .filter(|r| r["what"] == "tcp_bind")
            .count()
            == 2)
        .await,
        "{:?}",
        rig.report()
    );
    let binds: Vec<Value> = rig
        .report()
        .into_iter()
        .filter(|r| r["what"] == "tcp_bind")
        .collect();
    let by = |port: u16| {
        binds
            .iter()
            .find(|b| b["addr"] == format!("127.0.0.1:{port}").as_str())
            .unwrap()
            .clone()
    };
    assert_eq!(by(allowed)["ok"], true);
    assert_eq!(
        (by(other)["ok"].clone(), by(other)["os_error"].clone()),
        (json!(false), json!(libc_eacces()))
    );
}

/// Run the launcher by hand around `/usr/bin/true`, with Landlock made unavailable to it by a
/// seccomp filter of the test's own (the launcher inherits it and cannot remove it). Returns the
/// exit status and what it wrote to stderr.
fn launcher_without_landlock(require: Value, allow_unenforced: bool) -> (Option<i32>, String) {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let dir = tempfile::tempdir().unwrap();
    let permissions =
        common::fixture::rig_permissions(&json!({ "requires_enforcement": require }), dir.path());
    let profile = oxidepm_sandbox::Profile::from_permissions(
        &permissions,
        &oxidepm_sandbox::ProfileInput {
            state_dir: dir.path(),
            daemon_home: dir.path(),
            cwd: None,
            search_path: &[],
        },
    );
    let arch = if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        TargetArch::x86_64
    };
    let mut rules = std::collections::BTreeMap::new();
    rules.insert(libc::SYS_landlock_create_ruleset, Vec::new());
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .unwrap()
    .try_into()
    .unwrap();
    let exe = std::fs::File::open(true_program()).unwrap();
    let fd = exe.as_raw_fd();
    let mut cmd = std::process::Command::new(launcher_bin());
    cmd.arg("run").arg("--profile").arg(profile.to_json());
    if allow_unenforced {
        cmd.arg("--allow-unenforced");
    }
    cmd.env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    common::fixture::inherit_descriptors(&mut cmd, vec![(fd, 3)]);
    // SAFETY: the closure calls only the seccomp installation between fork and exec; the filter was built before the fork.
    unsafe {
        cmd.pre_exec(move || {
            seccompiler::apply_filter(&filter).map_err(|e| std::io::Error::other(e.to_string()))
        });
    }
    let out = cmd.output().expect("run the launcher");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_launcher_refuses_a_required_item_the_kernel_cannot_enforce() {
    need!(caps => caps.seccomp, "the test needs seccomp to take Landlock away");
    let (code, stderr) = launcher_without_landlock(json!(["fs_write", "spawn"]), false);
    assert_eq!(code, Some(126), "{stderr}");
    assert!(
        stderr.contains("fs_write")
            && stderr.contains("spawn")
            && stderr.contains("--allow-unenforced"),
        "{stderr}"
    );
    // an item that needs no Landlock is not in the way
    let (code, stderr) = launcher_without_landlock(json!(["network"]), false);
    assert_eq!(code, Some(0), "{stderr}");
}

#[test]
fn allow_unenforced_lets_the_launcher_start_the_adapter_anyway() {
    need!(caps => caps.seccomp, "the test needs seccomp to take Landlock away");
    let (code, stderr) = launcher_without_landlock(json!(["fs_write", "fs_read", "spawn"]), true);
    assert_eq!(code, Some(0), "{stderr}");
}

// ---- 25c: the probe, the self-test and the enforcement record --------------------------------

use oxidepm_adapter_proto::permissions::Enforce;
use oxidepm_sandbox::report::attempt;
use oxidepm_sandbox::{EnforcedRecord, Outcome, Profile, ProfileInput, SelfTestReport};
use oxidepmd::adapters::sandbox::{selftest, SelftestEnv};

/// The profile of `perms` for a directory layout like a rig's.
fn profile_in(dir: &std::path::Path, perms: &Value) -> Profile {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let state = home.join("adapters/state/probe");
    let permissions = common::fixture::rig_permissions(perms, &dir.join("io"));
    Profile::from_permissions(
        &permissions,
        &ProfileInput {
            state_dir: &state,
            daemon_home: &home,
            cwd: None,
            search_path: &[],
        },
    )
}

/// Run the self-test under `perms`.
async fn run_selftest(
    dir: &std::path::Path,
    perms: &Value,
    scratch_base: Option<std::path::PathBuf>,
) -> SelfTestReport {
    let profile = profile_in(dir, perms);
    let home_file = dir.join("home/state.db");
    std::fs::write(&home_file, b"the app list").unwrap();
    let env = SelftestEnv {
        daemon_socket: dir.join("home/oxidepm.sock"),
        home_file,
        scratch_base,
    };
    let report = selftest(&launcher_bin(), &profile, &env, false)
        .await
        .expect("the self-test runs");
    eprintln!("self-test report: {:?}", report.attempts);
    report
}

fn is_refused(report: &SelfTestReport, name: &str) -> bool {
    report.outcome(name).is_some_and(|o| o.is_refused())
}

#[tokio::test]
async fn selftest_names_a_socket_path_that_is_too_long() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("é".repeat(60));
    std::fs::create_dir(&base).unwrap();
    let profile = profile_in(dir.path(), &json!({}));
    let env = SelftestEnv {
        daemon_socket: dir.path().join("home/daemon.sock"),
        home_file: dir.path().join("home/adapters.db"),
        scratch_base: Some(base.clone()),
    };
    let error = selftest(&launcher_bin(), &profile, &env, false)
        .await
        .unwrap_err();
    assert!(
        error.contains("set TMPDIR to a shorter directory"),
        "{error}"
    );
    let path = error.split('\'').nth(1).expect("the socket path is quoted");
    assert!(std::path::Path::new(path).starts_with(&base), "{error}");
    assert!(path.ends_with("/probe.sock"), "{error}");
    assert!(
        error.contains(&format!("{} bytes (limit 107 bytes)", path.len())),
        "{error}"
    );
    assert!(base.read_dir().unwrap().next().is_none());
}

#[tokio::test]
async fn selftest_report_matches_what_the_fixture_observes() {
    need!(caps => caps.landlock_abi.is_some() && caps.seccomp, "needs Landlock and seccomp");
    let dir = tempfile::tempdir().unwrap();
    let perms = json!({});
    let report = run_selftest(dir.path(), &perms, None).await;
    assert_eq!(report.setup_error, None);

    // absolute expectations: both mechanisms exist on this kernel, so every forbidden attempt is refused
    for name in [
        attempt::WRITE_OUTSIDE,
        attempt::INET_SOCKET,
        attempt::IO_URING,
        attempt::EXEC,
        attempt::READ_OUTSIDE,
        attempt::READ_DAEMON_HOME,
    ] {
        assert!(
            is_refused(&report, name),
            "{name} should be refused: {:?}",
            report.attempts
        );
    }
    assert_eq!(
        report.enforced(false),
        vec![
            Enforce::Network,
            Enforce::FsWrite,
            Enforce::FsRead,
            Enforce::Spawn
        ]
    );

    // and the fixture, under the same profile, observes the same things
    let rig = sandboxed("observer", json!([]), perms);
    let outside = rig.dir.path().join("outside.txt");
    std::fs::write(&outside, b"x").unwrap();
    rig.set_script(after_initialize(json!([
        { "touch": { "path": rig.dir.path().join("written.txt").display().to_string() } },
        "inet_socket",
        "io_uring_setup",
        { "execve": { "path": true_program() } },
        { "read_file": { "path": outside.display().to_string() } },
        { "read_file": { "path": rig.home.join("probe-state.db").display().to_string() } },
    ])));
    std::fs::write(rig.home.join("probe-state.db"), b"x").unwrap();
    rig.entry.start();
    assert!(
        wait_until(30, || rig
            .report()
            .iter()
            .any(|r| r["what"] == "execve" && r["state"] == "refused")
            && rig
                .report()
                .iter()
                .filter(|r| r["what"] == "read_file")
                .count()
                == 2)
        .await,
        "{:?}",
        rig.report()
    );
    let report_of = rig.report();
    let refused = |what: &str| {
        report_of
            .iter()
            .filter(|r| {
                r["what"] == what
                    && r["ok"] == false
                    && matches!(r["os_error"].as_i64(), Some(1) | Some(13))
            })
            .count()
    };
    assert_eq!(refused("touch"), 1);
    assert_eq!(refused("inet_socket"), 1);
    assert_eq!(refused("io_uring_setup"), 1);
    assert_eq!(refused("read_file"), 2);
    assert!(report_of
        .iter()
        .any(|r| r["what"] == "execve" && r["state"] == "refused" && r["os_error"] == 13));

    // the record `enable` will store says the same, and survives the database
    let caps = oxidepm_sandbox::probe();
    let record = EnforcedRecord::from_report(&report, caps, false, false);
    assert_eq!(record.enforced, report.enforced(false));
    let home = dir.path().join("record-home");
    let installed = install(
        &home,
        InstallSpec {
            enforced: Some(serde_json::to_value(&record).unwrap()),
            ..InstallSpec::service("recorded", json!([]))
        },
    )
    .await;
    let _ = installed;
    let paths = oxidepm_core::Paths::new(home);
    let store = oxidepmd::adapters::store::AdapterStore::open_existing(&paths)
        .await
        .unwrap()
        .unwrap();
    let row = store.repo().get("recorded").await.unwrap().unwrap();
    store.close().await;
    assert_eq!(
        EnforcedRecord::parse(row.enforced_json.as_deref()),
        Some(record)
    );
}

#[tokio::test]
async fn requires_enforcement_check_names_what_a_weaker_machine_would_miss() {
    // the same record on a machine where the self-test refused nothing but the network
    let dir = tempfile::tempdir().unwrap();
    let report = SelfTestReport {
        setup_error: None,
        attempts: [(
            attempt::INET_SOCKET.to_string(),
            Outcome::Refused { errno: 1 },
        )]
        .into_iter()
        .collect(),
    };
    let _ = dir;
    let record = EnforcedRecord::from_report(
        &report,
        oxidepm_sandbox::SandboxCaps::default(),
        false,
        false,
    );
    assert_eq!(record.check(&[Enforce::Network]), Ok(()));
    assert_eq!(
        record.check(&[Enforce::Network, Enforce::FsWrite]),
        Err(vec![Enforce::FsWrite])
    );
}

#[tokio::test]
async fn af_unix_probe_reports_reachable_services() {
    need!(caps => caps.landlock_abi.is_some(), "no Landlock");
    let caps = oxidepm_sandbox::probe();
    // 1. a profile with no write path covering the probe's socket: where Landlock can restrict
    //    connecting to pathname sockets (ABI 9) it is refused, elsewhere it is reachable and the
    //    record says so
    let dir = tempfile::tempdir().unwrap();
    let report = run_selftest(dir.path(), &json!({}), None).await;
    let record = EnforcedRecord::from_report(&report, caps.clone(), false, false);
    if caps.landlock_unix_connect {
        assert!(
            is_refused(&report, attempt::PATHNAME_SOCKET),
            "{:?}",
            report.attempts
        );
        assert!(!record
            .unix_reachable
            .contains(&"pathname_socket".to_string()));
    } else {
        assert_eq!(
            report.outcome(attempt::PATHNAME_SOCKET),
            Some(Outcome::Allowed),
            "{:?}",
            report.attempts
        );
        assert!(record.unix_note().is_some_and(
            |n| n.starts_with("AF_UNIX services reachable") && n.contains("pathname_socket")
        ));
    }

    // 2. a listener in a directory the profile lets the adapter write to is reachable on every
    //    kernel, and `info` would say so
    let dir = tempfile::tempdir().unwrap();
    let probe_dir = dir.path().join("probe");
    std::fs::create_dir_all(&probe_dir).unwrap();
    let perms = json!({ "fs": { "read": ["$STATE", probe_dir.display().to_string()], "write": ["$STATE", probe_dir.display().to_string()] } });
    let report = run_selftest(dir.path(), &perms, Some(probe_dir)).await;
    assert_eq!(
        report.outcome(attempt::PATHNAME_SOCKET),
        Some(Outcome::Allowed),
        "{:?}",
        report.attempts
    );
    let record = EnforcedRecord::from_report(&report, caps, false, false);
    let note = record.unix_note().expect("a reachable service is reported");
    assert!(
        note.starts_with("AF_UNIX services reachable") && note.contains("pathname_socket"),
        "{note}"
    );
}

#[tokio::test]
async fn selftest_under_fs_read_any_shows_the_daemon_home_closed() {
    need!(caps => caps.landlock_abi.is_some() && caps.seccomp, "needs Landlock and seccomp");
    let dir = tempfile::tempdir().unwrap();
    let report = run_selftest(
        dir.path(),
        &json!({ "fs": { "read": ["any"], "write": ["$STATE"] }, "spawn": { "mode": "any" } }),
        None,
    )
    .await;
    assert!(
        is_refused(&report, attempt::READ_DAEMON_HOME),
        "{:?}",
        report.attempts
    );
    assert_eq!(
        report.outcome(attempt::READ_OUTSIDE),
        Some(Outcome::Allowed),
        "any: everything else is readable"
    );
    assert_eq!(
        report.outcome(attempt::EXEC),
        Some(Outcome::Allowed),
        "spawn any runs programs"
    );
    let enforced = report.enforced(true);
    assert!(
        enforced.contains(&Enforce::FsRead)
            && enforced.contains(&Enforce::Network)
            && !enforced.contains(&Enforce::Spawn),
        "{enforced:?}"
    );
}

#[test]
fn the_selftest_reports_a_profile_it_cannot_apply() {
    let output = std::process::Command::new(launcher_bin())
        .args(["selftest", "--profile", "{bad", "--plan", "{}"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(String::from_utf8_lossy(&output.stderr).contains("the profile is not valid"));
    let output = std::process::Command::new(launcher_bin())
        .arg("frobnicate")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(126),
        "an unknown mode is refused"
    );
}

// ---- 25a: with a daemon ----------------------------------------------------------------------

use common::daemon::DaemonProc;
use common::fixture::{adapter_states, install, wait_adapter_state, InstallSpec};
use oxidepm_core::{AppSpec, Runtime};

/// A `cmd` app as the JSON a client sends.
fn app_json(
    workdir: &std::path::Path,
    name: &str,
    program: &str,
    args: &[&str],
    extra: Value,
) -> Value {
    let mut spec = AppSpec::new(
        name.into(),
        Runtime::CMD,
        program.into(),
        workdir.to_path_buf(),
    );
    spec.args = args.iter().map(|a| a.to_string()).collect();
    let mut value = serde_json::to_value(spec).unwrap();
    for (k, v) in extra.as_object().into_iter().flatten() {
        value[k.as_str()] = v.clone();
    }
    value
}

/// `(status, pid, restarts)` of the app `name`.
fn app_state(daemon: &DaemonProc, name: &str) -> (String, Option<u64>, u64) {
    let reply = daemon.rpc(json!({ "type": "status" }));
    let app = reply["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["spec"]["name"] == name)
        .unwrap_or_else(|| panic!("no app {name}: {reply}"))
        .clone();
    (
        app["state"]["status"].as_str().unwrap_or("").to_string(),
        app["state"]["pid"].as_u64(),
        app["state"]["restarts"].as_u64().unwrap_or(0),
    )
}

/// A copy of the daemon binary in a directory with nothing next to it.
fn lone_daemon_binary(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("lone").join("oxidepmd");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_oxidepmd"), &bin).unwrap();
    bin
}

#[tokio::test]
async fn adapter_enable_runs_the_selftest_with_a_long_tmpdir() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let home = dir.path().join("home");
    let tmpdir = dir
        .path()
        .join("t".repeat(81 - dir.path().as_os_str().len()));
    std::fs::create_dir(&tmpdir).unwrap();
    install(
        &home,
        InstallSpec {
            enabled: false,
            perms: Some(json!({
                "env_pass": [],
                "fs": { "read": ["$STATE"], "write": ["$STATE"] },
                "spawn": { "mode": "none" },
            })),
            ..InstallSpec::tool("longtmp", json!([]))
        },
    )
    .await;
    let daemon = DaemonProc::spawn_with_env(&home, &[("TMPDIR", tmpdir.to_str().unwrap())]);
    daemon.wait_for_boot();
    let reply = daemon.rpc(json!({
        "type": "adapter", "op": "enable", "name": "longtmp", "yes": true,
        "allow_unenforced": true,
    }));
    assert_eq!(reply["kind"], "enabled", "{reply}");
    assert!(tmpdir.read_dir().unwrap().next().is_none());
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

#[test]
fn enable_accounts_for_reachable_unix_services() {
    use common::adapter_kit::{IndexOpts, Kit, Pkg};

    for required in [json!(["network"]), json!([])] {
        let package = Pkg::tool("net", "0.1.0")
            .perms(json!({"requires_enforcement": required}))
            .package();
        let kit = Kit::new();
        kit.publish(&[&package], &IndexOpts::default());
        let state = oxidepm_core::Paths::new(kit.home.clone()).adapter_state("net");
        let _daemon = kit.daemon_with(&[("TMPDIR", state.to_str().unwrap())]);
        let imported = kit.run(&["adapter", "import", "net"]);
        assert_eq!(imported.code, 0, "{}", imported.all());
        let fingerprint = package.fingerprint();

        let automatic = kit.run(&["adapter", "enable", "net", "--yes"]);
        assert_eq!(automatic.code, 1, "{}", automatic.all());
        assert!(
            automatic.stderr.contains("pathname_socket"),
            "{}",
            automatic.all()
        );
        let explicit = ["adapter", "enable", "net", "--accept", fingerprint.as_str()];
        let run = kit.run(&explicit);
        if required == json!(["network"]) {
            assert_eq!(run.code, 1, "{}", run.all());
            assert!(run.stderr.contains("pathname_socket"), "{}", run.all());
            let override_run =
                kit.run(&["adapter", "enable", "net", "--yes", "--allow-unenforced"]);
            assert_eq!(override_run.code, 0, "{}", override_run.all());
        } else {
            assert_eq!(run.code, 0, "{}", run.all());
        }
    }
}

#[tokio::test]
async fn launcher_failure_marks_failed_sandbox_and_the_daemon_is_unaffected() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let fx = install(&home, InstallSpec::service("nosandbox", json!([]))).await;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bin = lone_daemon_binary(tmp.path());
    let env = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = DaemonProc::spawn_binary(&bin, &home, &env);

    // the app is started and keeps running while the adapter cannot be
    let started = daemon.rpc(json!({ "type": "start", "spec": app_json(&work, "steady", "sleep", &["305.7"], json!({})) }));
    assert_eq!(started["type"], "started", "{started}");
    assert!(
        wait_adapter_state(&daemon, "nosandbox", "failed", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    let note = adapter_states(&daemon)
        .into_iter()
        .find(|(n, ..)| n == "nosandbox")
        .unwrap()
        .2
        .unwrap();
    assert!(
        note.starts_with("sandbox:") && note.contains("oxidepm-sandbox"),
        "{note}"
    );
    assert!(fx.start_pids().is_empty(), "the adapter never ran");
    let (status, pid, _) = app_state(&daemon, "steady");
    assert_eq!(
        (status.as_str(), pid.is_some()),
        ("running", true),
        "the app is untouched"
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

#[tokio::test]
async fn allow_unenforced_runs_an_adapter_when_there_is_no_launcher() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::service("accepted", json!([]));
    spec.enforced = Some(
        json!({ "allow_unenforced": true, "enforced": [], "caps": {"landlock_abi": null, "landlock_unix_connect": false, "seccomp": false, "no_new_privs": true, "ptrace_scope": null, "cgroup_delegated": false} }),
    );
    let fx = install(&home, spec).await;
    let bin = lone_daemon_binary(tmp.path());
    let env = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = DaemonProc::spawn_binary(&bin, &home, &env);
    assert!(
        wait_adapter_state(&daemon, "accepted", "ready", 20).await,
        "{:?}",
        adapter_states(&daemon)
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

#[tokio::test]
async fn fork_bomb_adapter_is_killed_and_apps_still_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::service(
        "bomber",
        after_initialize(json!({ "fork_bomb": { "max": 300, "hold_ms": 30000 } })),
    );
    spec.perms = Some(json!({ "spawn": { "mode": "any" }, "memory_mb": 256 }));
    let fx = install(&home, spec).await;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let env = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = DaemonProc::spawn_with_env(&home, &env);

    // an app that crashes and is restarted by the supervisor, as it should be, all along
    let crashing = app_json(
        &work,
        "crasher",
        "sh",
        &["-c", "sleep 0.2; exit 1"],
        json!({ "restart_policy": { "auto_restart": true, "max_restarts": 100, "restart_delay_ms": 100, "crash_window_secs": 600, "max_restart_delay_ms": 200, "stable_uptime_secs": 60 } }),
    );
    let started = daemon.rpc(json!({ "type": "start", "spec": crashing }));
    assert_eq!(started["type"], "started", "{started}");

    // the group is killed by the cap (within a few sampler intervals) and the adapter backs off
    assert!(
        wait_until(40, || adapter_states(&daemon)
            .iter()
            .any(|(n, s, note)| n == "bomber"
                && s == "backoff"
                && note.as_deref().is_some_and(|n| n.contains("threads"))))
        .await,
        "{:?}",
        adapter_states(&daemon)
    );
    let first_pid = fx.start_pids()[0];
    assert!(
        wait_until(10, || !pid_alive(first_pid)).await,
        "the adapter is dead"
    );
    let before = app_state(&daemon, "crasher").2;
    assert!(
        wait_until(20, || app_state(&daemon, "crasher").2 > before).await,
        "the crashing app keeps being restarted"
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
}

#[cfg(feature = "slow-tests")]
#[tokio::test]
async fn state_dir_over_quota_is_recorded_and_shown() {
    // the production quota is 64 MiB and checked every 60 s: the daemon cannot be hurried, so this
    // writes past 64 MiB (a sparse-free fill) and waits for the real check
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut spec = InstallSpec::service(
        "hoarder",
        after_initialize(json!({ "fill_state_dir": { "mb": 70 } })),
    );
    spec.perms = Some(json!({ "fs": { "read": ["$STATE", "$IO"], "write": ["$STATE", "$IO"] } }));
    let fx = install(&home, spec).await;
    let env = fx.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = DaemonProc::spawn_with_env(&home, &env);
    assert!(
        wait_adapter_state(&daemon, "hoarder", "disabled", 120).await,
        "{:?}",
        adapter_states(&daemon)
    );
    let status = daemon.rpc(json!({ "type": "daemon_status" }));
    let warnings = status["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("hoarder")
                && w.as_str().unwrap().contains("state directory")),
        "{status}"
    );
    assert!(
        home.join("adapters/state/hoarder/fill-0.bin").exists(),
        "the data is kept"
    );
    let _ = daemon.rpc(json!({ "type": "kill" }));
    drop(daemon);
    // the row says disabled, so a restart does not start it again
    let paths = oxidepm_core::Paths::new(home.clone());
    let store = oxidepmd::adapters::store::AdapterStore::open_existing(&paths)
        .await
        .unwrap()
        .unwrap();
    let row = store.repo().get("hoarder").await.unwrap().unwrap();
    store.close().await;
    assert!(!row.enabled);
    assert!(row.state_note.unwrap().starts_with("disabled:"));
}

#[tokio::test]
async fn daemon_status_reports_the_sandbox_probe() {
    launcher_bin();
    let tmp = tempfile::tempdir().unwrap();
    let daemon = DaemonProc::spawn(&tmp.path().join("home"));
    let status = daemon.rpc(json!({ "type": "daemon_status" }));
    let caps = oxidepm_sandbox::probe();
    let sandbox = &status["sandbox"];
    assert_eq!(
        sandbox["landlock_abi"].as_u64().map(|v| v as u32),
        caps.landlock_abi,
        "{status}"
    );
    assert_eq!(
        (
            sandbox["seccomp"].as_bool(),
            sandbox["landlock_unix_connect"].as_bool()
        ),
        (Some(caps.seccomp), Some(caps.landlock_unix_connect))
    );
    assert_eq!(
        sandbox["ptrace_scope"].as_u64().map(|v| v as u8),
        caps.ptrace_scope
    );
    assert_eq!(
        sandbox["launcher"], true,
        "the launcher is built next to the daemon in the tests"
    );
    assert_eq!(sandbox["summary"], caps.summary());
    let _ = daemon.rpc(json!({ "type": "kill" }));
}
