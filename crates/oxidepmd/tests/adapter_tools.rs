//! Relayed verbs: `Adapter(Invoke)`, the streamed answer, cancellation, the
//! `$CWD` rules, the verb table and the clap-first fallback of the CLI.
//!
//! A tool adapter (the fixture) declares verbs; the real `oxidepmd` runs it for each call, in a
//! process of its own, and the tests read the lines the daemon writes. Tests marked [os] depend on
//! the kernel; they probe first and print why they skip.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::adapter_kit::*;
use common::daemon::DaemonProc;
use common::fixture::pid_alive;
use serde_json::{json, Value};

const NAME: &str = "tools";

/// One verb entry of a manifest.
fn verb(name: &str, tier: &str, writes: bool, dry_run: bool) -> Value {
    json!({ "name": name, "summary": format!("The {name} verb."), "tier": tier, "writes": writes, "dry_run": dry_run, "max_runtime_secs": 60 })
}

struct Rig {
    kit: Kit,
    daemon: DaemonProc,
    project: PathBuf,
    /// Processes of the adapter started before the test began (the round trip of `enable`).
    baseline: usize,
}

impl Rig {
    /// A tool adapter named `tools` with `verbs`, whose fixture runs `script`, installed and
    /// enabled in a daemon that has `env` in its environment. `perms` change the default ones.
    fn new(script: Value, verbs: Vec<Value>, perms: Value, env: &[(&str, &str)]) -> Rig {
        let kit = Kit::new();
        let io = kit.io_dir();
        let io_text = io.display().to_string();
        let mut p = json!({
            "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
            "fs": { "read": ["$STATE", io_text.clone(), "$CWD"], "write": ["$STATE", io_text, "$CWD"] }
        });
        for (k, v) in perms.as_object().unwrap() {
            p[k.as_str()] = v.clone();
        }
        let pkg = Pkg::tool(NAME, "0.1.0")
            .perms(p)
            .with("provides", json!({ "verbs": verbs }))
            .package();
        kit.publish(&[&pkg], &IndexOpts::default());
        kit.write_script(NAME, script);
        // the report is written in the adapter's state directory: a safe verb may write nothing else
        let mut script_env = kit.script_env(NAME);
        let report_path = kit
            .home
            .join("adapters")
            .join("state")
            .join(NAME)
            .join("report.jsonl");
        script_env[1].1 = report_path.display().to_string();
        let mut all: Vec<(&str, &str)> = script_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        all.extend_from_slice(env);
        let daemon = kit.daemon_with(&all);
        let imported = kit.run(&["adapter", "import", NAME]);
        assert_eq!(imported.code, 0, "{}", imported.all());
        let enabled = kit.run(&["adapter", "enable", NAME, "--accept", &pkg.fingerprint()]);
        assert_eq!(enabled.code, 0, "{}", enabled.all());
        let project = kit.dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let mut rig = Rig {
            kit,
            daemon,
            project,
            baseline: 0,
        };
        rig.baseline = rig.starts();
        rig
    }

    /// Processes of the adapter that have started.
    fn starts(&self) -> usize {
        self.report()
            .iter()
            .filter(|r| r["what"] == "start")
            .count()
    }

    /// Processes started since the test began: a refused call must leave this at zero.
    fn started_since(&self) -> usize {
        self.starts() - self.baseline
    }

    fn simple(script: Value, verbs: Vec<Value>) -> Rig {
        Rig::new(script, verbs, json!({}), &[])
    }

    /// Send an invoke on the main socket (a person) and return the stream.
    fn send(&self, verb: &str, cwd: &Path, dry_run: bool) -> UnixStream {
        send_invoke(&self.daemon.socket(), None, verb, cwd, dry_run)
    }

    fn report(&self) -> Vec<Value> {
        oxidepm_adapter_fixture::report::read_report(
            &self
                .kit
                .home
                .join("adapters")
                .join("state")
                .join(NAME)
                .join("report.jsonl"),
        )
    }

    /// The pid the fixture recorded with `echo_status`, waiting for it.
    fn recorded_pid(&self) -> u32 {
        let mut found = None;
        assert!(wait_for(20, || {
            found = self
                .report()
                .into_iter()
                .find(|r| r["what"] == "status")
                .map(|r| r["pid"].as_u64().unwrap() as u32);
            found.is_some()
        }));
        found.unwrap()
    }
}

fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send_invoke(
    socket: &Path,
    claim: Option<&str>,
    verb: &str,
    cwd: &Path,
    dry_run: bool,
) -> UnixStream {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut envelope = json!({
        "protocol": 2,
        "request_id": "inv-1",
        "dry_run": dry_run,
        "request": { "type": "adapter", "op": "invoke", "verb": verb, "argv": ["a", "b"], "cwd": cwd.display().to_string() }
    });
    if let Some(claim) = claim {
        envelope["actor"] = json!(claim);
    }
    let mut line = envelope.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream
}

/// Read lines until one without `more`; returns them all (envelopes).
fn read_all(stream: &UnixStream) -> Vec<Value> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .expect("read a line from the daemon");
        assert!(n > 0, "the daemon closed the connection after {lines:?}");
        let value: Value =
            serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{e}: {line}"));
        let more = value["more"].as_bool().unwrap_or(false);
        lines.push(value);
        if !more {
            return lines;
        }
    }
}

fn last(lines: &[Value]) -> &Value {
    &lines.last().unwrap()["response"]
}

fn texts(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["response"]["type"] == "invoke_output")
        .map(|l| l["response"]["text"].as_str().unwrap().to_string())
        .collect()
}

macro_rules! need_landlock {
    () => {
        let caps = oxidepm_sandbox::probe();
        if caps.landlock_abi.is_none() {
            eprintln!(
                "skipping {}: no Landlock ({})",
                module_path!(),
                caps.summary()
            );
            return;
        }
        eprintln!("probe says: {}", caps.summary());
    };
}

// ---- the stream ------------------------------------------------------------------------------

#[test]
fn invoke_streams_progress_then_result() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [
            { "invoke_output": { "text": "one" } },
            { "sleep": { "ms": 150 } },
            { "invoke_output": { "stream": "stderr", "text": "two" } },
            { "invoke_output": { "stream": "status", "text": "three" } },
            { "reply": { "result": { "exit_code": 3, "result": { "k": 1 } } } }
        ] }]),
        vec![verb("probe", "safe", false, false)],
    );
    let stream = rig.send("probe", &rig.project, false);
    let lines = read_all(&stream);
    assert_eq!(texts(&lines), ["one", "two", "three"], "{lines:?}");
    let streams: Vec<&str> = lines
        .iter()
        .filter(|l| l["more"] == true)
        .map(|l| l["response"]["stream"].as_str().unwrap())
        .collect();
    assert_eq!(streams, ["stdout", "stderr", "status"]);
    assert!(lines.iter().all(|l| l["request_id"] == "inv-1"));
    assert!(
        lines[..lines.len() - 1].iter().all(|l| l["more"] == true),
        "every part but the last has more"
    );
    assert!(
        lines.last().unwrap().get("more").is_none(),
        "the last line has none"
    );
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    assert_eq!(last(&lines)["exit_code"], 3);
    assert_eq!(last(&lines)["result"], json!({ "k": 1 }));

    // the tool was asked what the CLI sent, in the directory the daemon checked
    let invoke = rig
        .report()
        .into_iter()
        .find(|r| {
            r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "command.invoke"
        })
        .unwrap_or_else(|| panic!("command.invoke reached the tool: {:?}", rig.report()));
    let params = &invoke["frame"]["params"];
    assert_eq!(params["verb"], "probe");
    assert_eq!(params["argv"], json!(["a", "b"]));
    assert_eq!(params["cwd"], rig.project.display().to_string());
    assert_eq!(params["dry_run"], false);
    assert_eq!(params["tty"], false);
    assert_eq!(params["actor"]["class"], "human");

    // the tool was started for this call alone and is gone again
    let pid = invoke["pid"].as_u64().unwrap() as u32;
    assert!(
        wait_for(20, || !pid_alive(pid)),
        "the tool of a finished call is ended"
    );
    // the connection serves the next request
    let mut again = stream.try_clone().unwrap();
    again
        .write_all(b"{\"protocol\":2,\"request_id\":\"p\",\"request\":{\"type\":\"ping\"}}\n")
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("pong"), "{line}");
}

#[test]
fn client_disconnect_cancels_and_kills_the_tool() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": ["echo_status", { "invoke_output": { "text": "going" } }, "hang"] }]),
        vec![verb("slow", "safe", false, false)],
    );
    let stream = rig.send("slow", &rig.project, false);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("going"), "{line}");
    let pid = rig.recorded_pid();
    assert!(pid_alive(pid));
    drop(reader);
    drop(stream);
    assert!(
        wait_for(20, || !pid_alive(pid)),
        "the tool of a call whose client left is killed"
    );
}

#[test]
fn output_beyond_8_mib_is_truncated_with_a_marker() {
    // 9 MiB in 64 KiB frames
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "invoke_output": { "bytes": 65536, "count": 144 } }, "reply"] }]),
        vec![verb("loud", "safe", false, false)],
    );
    let lines = read_all(&rig.send("loud", &rig.project, false));
    let all = texts(&lines);
    let data: usize = all
        .iter()
        .filter(|t| t.chars().all(|c| c == 'x'))
        .map(String::len)
        .sum();
    assert!(
        (8 * 1024 * 1024 - 65536..=8 * 1024 * 1024).contains(&data),
        "relayed {data} bytes"
    );
    let marker = all.last().unwrap();
    assert!(marker.contains("truncated"), "{marker}");
    assert_eq!(
        all.iter().filter(|t| t.contains("truncated")).count(),
        1,
        "one marker"
    );
    assert_eq!(
        last(&lines)["type"],
        "invoke_done",
        "the call still ends normally"
    );
}

#[test]
fn invoke_inactivity_timeout_is_reset_by_frames() {
    use oxidepm_ipc::{AdapterRequest, CallOptions, InvokeArgs, IpcClient, Request, Response};
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "invoke_output": { "count": 4, "every_ms": 800 } }, "reply"] }]),
        vec![verb("steady", "safe", false, false)],
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client =
        IpcClient::new(rig.daemon.socket()).with_stream_inactivity(Duration::from_millis(2000));
    let request = Request::Adapter(Box::new(AdapterRequest::Invoke(Box::new(InvokeArgs {
        verb: "steady".into(),
        argv: vec![],
        cwd: rig.project.display().to_string(),
    }))));
    // 3.2 s in all, never silent for 2 s: it finishes
    let mut parts = 0;
    let started = Instant::now();
    let done = rt
        .block_on(client.send_streaming(&request, &CallOptions::default(), |_| parts += 1))
        .unwrap();
    assert_eq!(parts, 4);
    assert!(
        matches!(done, Response::InvokeDone { exit_code: 0, .. }),
        "{done:?}"
    );
    assert!(
        started.elapsed() > Duration::from_millis(2400),
        "longer than the inactivity limit: {:?}",
        started.elapsed()
    );
    // a tool that says nothing is given up on (each call starts a process that reads the script anew)
    rig.kit.write_script(
        NAME,
        json!([{ "on": "command.invoke", "do": [{ "sleep": { "ms": 6000 } }, "reply"] }]),
    );
    let silent = rt.block_on(client.send_streaming(&request, &CallOptions::default(), |_| {}));
    assert!(
        matches!(silent, Err(oxidepm_core::Error::Timeout(_))),
        "{silent:?}"
    );
}

// ---- timeouts of the daemon ------------------------------------------------------------------

#[test]
fn tool_with_no_frames_is_killed_after_the_idle_limit() {
    // the verb is allowed an hour: it is the silence that ends the call, not its runtime
    let mut quiet = verb("quiet", "safe", false, false);
    quiet["max_runtime_secs"] = json!(3600);
    let rig = Rig::new(
        json!([{ "on": "command.invoke", "do": ["echo_status", "hang"] }]),
        vec![quiet],
        json!({}),
        &[("OXIDEPM_TEST_INVOKE_IDLE_MS", "1500")],
    );
    let started = Instant::now();
    let lines = read_all(&rig.send("quiet", &rig.project, false));
    assert_eq!(last(&lines)["type"], "error", "{lines:?}");
    assert_eq!(last(&lines)["code"], "E_ADAPTER_TIMEOUT");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let pid = rig.recorded_pid();
    assert!(wait_for(20, || !pid_alive(pid)), "the tool is killed");
}

#[cfg(feature = "slow-tests")]
#[test]
fn tool_with_no_frames_for_120s_is_killed() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": ["echo_status", "hang"] }]),
        vec![verb("quiet", "safe", false, false)],
    );
    let started = Instant::now();
    let lines = read_all(&rig.send("quiet", &rig.project, false));
    assert_eq!(last(&lines)["code"], "E_ADAPTER_TIMEOUT", "{lines:?}");
    assert!(started.elapsed() >= Duration::from_secs(119));
    let pid = rig.recorded_pid();
    assert!(wait_for(20, || !pid_alive(pid)), "killed");
}

#[test]
fn relay_write_timeout_cancels_a_stuck_client() {
    // one slot: the next call can run only if the stuck one gave its slot back
    let rig = Rig::new(
        json!([{ "on": "command.invoke", "do": ["echo_status", { "invoke_output": { "bytes": 65536, "count": 120 } }, "hang"] }]),
        vec![verb("flood", "safe", false, false)],
        json!({}),
        &[
            ("OXIDEPM_TEST_RELAY_WRITE_MS", "1000"),
            ("OXIDEPM_TEST_INVOKE_SLOTS", "1"),
        ],
    );
    // a client that never reads (its receive buffer fills up)
    let mut stuck = rig.send("flood", &rig.project, false);
    let pid = rig.recorded_pid();
    assert!(
        wait_for(30, || !pid_alive(pid)),
        "the tool of a client that stopped reading is killed"
    );
    // The daemon closes the connection after the call has released its slot.
    std::io::copy(&mut stuck, &mut std::io::sink()).expect("drain the cancelled connection to EOF");
    rig.kit.write_script(NAME, json!([]));
    let lines = read_all(&rig.send("flood", &rig.project, false));
    assert_eq!(
        last(&lines)["type"],
        "invoke_done",
        "the slot is free again: {lines:?}"
    );
    drop(stuck);
}

// ---- dry run ---------------------------------------------------------------------------------

#[test]
fn dry_run_invoke_of_a_verb_without_dry_run_support_is_refused() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": ["echo_status", "reply"] }]),
        vec![
            verb("writer", "caution", true, false),
            verb("reader", "safe", false, false),
        ],
    );
    let lines = read_all(&rig.send("writer", &rig.project, true));
    assert_eq!(last(&lines)["type"], "error", "{lines:?}");
    assert_eq!(last(&lines)["code"], "E_DRY_RUN_UNSUPPORTED");
    assert_eq!(
        rig.started_since(),
        0,
        "the tool was not started: {:?}",
        rig.report()
    );
    // a verb that only reads ignores the flag and runs
    let lines = read_all(&rig.send("reader", &rig.project, true));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
}

#[test]
fn read_only_verbs_receive_dry_run_only_when_declared() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": ["reply"] }]),
        vec![
            verb("preview", "caution", false, true),
            verb("reader", "safe", false, false),
        ],
    );
    for name in ["preview", "reader"] {
        let lines = read_all(&rig.send(name, &rig.project, true));
        assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    }
    let flags: Vec<_> = rig
        .report()
        .into_iter()
        .filter(|r| {
            r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "command.invoke"
        })
        .map(|r| {
            let params = &r["frame"]["params"];
            (params["verb"].clone(), params["dry_run"].clone())
        })
        .collect();
    assert_eq!(
        flags,
        [
            (json!("preview"), json!(true)),
            (json!("reader"), json!(false))
        ]
    );
}

#[test]
fn git_start_accepts_a_configuration_without_a_suggested_spec() {
    let kit = Kit::new();
    let config = kit.dir.path().join("oxidepm.toml");
    std::fs::write(
        &config,
        "[[apps]]\nname = \"from-config\"\nbinary = \"/bin/sleep\"\nargs = [\"60\"]\n",
    )
    .unwrap();
    let pkg = Pkg::tool("git-start", "0.1.0")
        .perms(json!({
            "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
            "fs": {"read": [kit.io_dir()], "write": ["$STATE", kit.io_dir()]}
        }))
        .with(
            "provides",
            json!({"verbs": [verb("git-start", "caution", false, true)]}),
        )
        .package();
    kit.publish(&[&pkg], &IndexOpts::default());
    kit.write_script(
        "git-start",
        json!([{ "on": "command.invoke", "do": [
        { "reply": { "result": { "exit_code": 0, "result": { "config_file": config } } } }
    ] }]),
    );
    let env = kit.script_env("git-start");
    let env: Vec<_> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let _daemon = kit.daemon_with(&env);
    let imported = kit.run(&["adapter", "import", "git-start"]);
    assert_eq!(imported.code, 0, "{}", imported.all());
    let enabled = kit.run(&[
        "adapter",
        "enable",
        "git-start",
        "--accept",
        &pkg.fingerprint(),
    ]);
    assert_eq!(enabled.code, 0, "{}", enabled.all());
    let started = kit
        .cli()
        .current_dir(kit.io_dir())
        .args(["--json", "git-start"])
        .output()
        .unwrap();
    let started = Run {
        code: started.status.code().unwrap(),
        stdout: String::from_utf8(started.stdout).unwrap(),
        stderr: String::from_utf8(started.stderr).unwrap(),
    };
    assert_eq!(started.code, 0, "{}", started.all());
    let done: Value = serde_json::from_str(&started.stdout).unwrap();
    assert_eq!(done["start"]["type"], "started_batch", "{done}");
}

#[test]
fn cli_reports_when_a_read_only_verb_ignores_dry_run() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "invoke_output": { "text": "read result" } }, "reply"] }]),
        vec![verb("reader", "safe", false, false)],
    );
    let out = rig
        .kit
        .cli()
        .current_dir(&rig.project)
        .args(["--dry-run", "reader"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "read result\n");
    assert!(
        stderr.contains("has no dry run") && stderr.contains("ignoring --dry-run"),
        "{stderr}"
    );
}

#[test]
fn dry_run_initializes_with_a_read_grant() {
    let rig = Rig::new(
        json!([{ "on": "command.invoke", "do": ["reply"] }]),
        vec![verb("preview", "caution", true, true)],
        json!({ "host": { "level": "lifecycle", "scope": "all" } }),
        &[],
    );
    for dry_run in [true, false] {
        let lines = read_all(&rig.send("preview", &rig.project, dry_run));
        assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    }
    let levels: Vec<_> = rig
        .report()
        .into_iter()
        .filter(|r| r["what"] == "frame" && r["frame"]["method"] == "initialize")
        .skip(rig.baseline)
        .map(|r| r["frame"]["params"]["grant"]["level"].clone())
        .collect();
    assert_eq!(levels, ["read", "lifecycle"]);
}

#[test]
fn dry_run_invoke_runs_with_a_read_only_profile() {
    need_landlock!();
    let rig = Rig::simple(json!([]), vec![verb("writer", "caution", true, true)]);
    // the directory the verb works in is the one it may write to
    let work = rig.project.join("w");
    std::fs::create_dir_all(&work).unwrap();
    let target = work.join("out");
    rig.kit.write_script(
        NAME,
        json!([{ "on": "command.invoke", "do": [{ "touch": { "path": target.display().to_string() } }, "reply"] }]),
    );
    let lines = read_all(&rig.send("writer", &work, true));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    let touch = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "touch")
        .expect("the tool tried to write");
    assert_eq!(touch["ok"], false, "a rehearsal cannot write: {touch}");
    assert!(!target.exists());
    // the real call writes
    let lines = read_all(&rig.send("writer", &work, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    assert!(
        target.exists(),
        "the real call may write in its directory: {:?}",
        rig.report()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn tool_can_connect_only_to_the_declared_environment_socket() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let caps = oxidepm_sandbox::probe();
    if !caps.landlock_unix_connect {
        eprintln!("skipping: {}", caps.summary());
        return;
    }
    let sockets = tempfile::tempdir().unwrap();
    let agent = sockets.path().join("agent.sock");
    let other = sockets.path().join("other.sock");
    let alias = sockets.path().join("alias.sock");
    let _agent = UnixListener::bind(&agent).unwrap();
    let _other = UnixListener::bind(&other).unwrap();
    symlink(&agent, &alias).unwrap();

    for granted in [false, true] {
        let mut perms =
            json!({ "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT", "SSH_AUTH_SOCK"] });
        if granted {
            perms["connect_env_socket"] = json!(["SSH_AUTH_SOCK"]);
        }
        let rig = Rig::new(
            json!([{ "on": "command.invoke", "do": [
                { "unix_connect": { "path": alias } },
                { "unix_connect": { "path": other } },
                "reply",
            ] }]),
            vec![verb("probe-agent", "caution", false, true)],
            perms,
            &[("SSH_AUTH_SOCK", alias.to_str().unwrap())],
        );
        for dry_run in [false, true] {
            let lines = read_all(&rig.send("probe-agent", &rig.project, dry_run));
            assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
            let connects: Vec<_> = rig
                .report()
                .into_iter()
                .filter(|r| r["what"] == "unix_connect")
                .collect();
            let connects = &connects[connects.len() - 2..];
            assert_eq!(connects[0]["ok"], granted && !dry_run, "{connects:?}");
            assert_eq!(connects[1]["ok"], false, "{connects:?}");
            assert_eq!(connects[1]["os_error"], nix::errno::Errno::EACCES as i32);
        }

        let info = rig.kit.run(&["adapter", "info", NAME]);
        assert_eq!(info.code, 0, "{}", info.all());
        assert_eq!(
            info.stdout
                .contains("connects to UNIX sockets from SSH_AUTH_SOCK"),
            granted,
            "{}",
            info.all()
        );
    }
}

#[test]
fn tool_refuses_a_directory_as_an_environment_socket() {
    let kit = Kit::new();
    let pkg = Pkg::tool(NAME, "0.1.0")
        .perms(json!({
            "env_pass": ["SSH_AUTH_SOCK"],
            "connect_env_socket": ["SSH_AUTH_SOCK"],
        }))
        .package();
    kit.publish(&[&pkg], &IndexOpts::default());
    let _daemon = kit.daemon_with(&[("SSH_AUTH_SOCK", kit.dir.path().to_str().unwrap())]);
    let imported = kit.run(&["adapter", "import", NAME]);
    assert_eq!(imported.code, 0, "{}", imported.all());

    let enabled = kit.run(&["adapter", "enable", NAME, "--accept", &pkg.fingerprint()]);
    assert_ne!(enabled.code, 0, "{}", enabled.all());
    assert!(
        enabled
            .all()
            .contains("SSH_AUTH_SOCK must name a UNIX socket owned by the daemon user"),
        "{}",
        enabled.all()
    );
    assert!(
        imported
            .stdout
            .contains("connects to UNIX sockets from SSH_AUTH_SOCK"),
        "{}",
        imported.all()
    );
}

#[test]
fn tool_refuses_a_socket_inside_the_daemon_home() {
    use std::os::unix::net::UnixListener;

    let kit = Kit::new();
    let pkg = Pkg::tool(NAME, "0.1.0")
        .perms(json!({
            "env_pass": ["SSH_AUTH_SOCK"],
            "connect_env_socket": ["SSH_AUTH_SOCK"],
        }))
        .package();
    kit.publish(&[&pkg], &IndexOpts::default());
    std::fs::create_dir_all(&kit.home).unwrap();
    let socket = kit.home.join("other.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let _daemon = kit.daemon_with(&[("SSH_AUTH_SOCK", socket.to_str().unwrap())]);
    let imported = kit.run(&["adapter", "import", NAME]);
    assert_eq!(imported.code, 0, "{}", imported.all());

    let enabled = kit.run(&["adapter", "enable", NAME, "--accept", &pkg.fingerprint()]);
    assert_ne!(enabled.code, 0, "{}", enabled.all());
    assert!(
        enabled.all().contains("inside the daemon home"),
        "{}",
        enabled.all()
    );
}

#[test]
fn safe_verb_runs_with_writes_limited_to_state() {
    need_landlock!();
    let rig = Rig::simple(
        json!([]),
        vec![
            verb("look", "safe", false, false),
            verb("make", "caution", true, false),
        ],
    );
    let target = rig.project.join("out");
    rig.kit.write_script(
        NAME,
        json!([{ "on": "command.invoke", "do": [{ "touch": { "path": target.display().to_string() } }, "reply"] }]),
    );
    let lines = read_all(&rig.send("look", &rig.project, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    let touch = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "touch")
        .unwrap();
    assert_eq!(
        touch["ok"], false,
        "a safe verb may not write outside its state directory: {touch}"
    );
    assert!(!target.exists());
    // a verb that declared writes gets its working directory
    let lines = read_all(&rig.send("make", &rig.project, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    assert!(target.exists(), "{:?}", rig.report());
}

// ---- who may ---------------------------------------------------------------------------------

#[test]
fn ai_cannot_invoke_a_caution_verb() {
    let rig = Rig::simple(json!([]), vec![verb("make", "caution", true, false)]);
    let lines = read_all(&send_invoke(
        &rig.daemon.agent_socket(),
        None,
        "make",
        &rig.project,
        false,
    ));
    assert_eq!(last(&lines)["type"], "error", "{lines:?}");
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED");
    assert!(
        last(&lines)["message"]
            .as_str()
            .unwrap()
            .contains("caution"),
        "{lines:?}"
    );
    // the same through the main socket with a claim: still an ai
    let lines = read_all(&send_invoke(
        &rig.daemon.socket(),
        Some("ai:test"),
        "make",
        &rig.project,
        false,
    ));
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED", "{lines:?}");
    assert_eq!(rig.started_since(), 0, "nothing ran");
}

#[test]
fn ai_cannot_invoke_a_verb_of_an_adapter_above_read() {
    // the verb is safe, but the adapter holds the lifecycle level: it could be used on an AI's behalf
    let rig = Rig::new(
        json!([]),
        vec![verb("look", "safe", false, false)],
        json!({ "host": { "level": "lifecycle", "scope": "all" } }),
        &[],
    );
    let lines = read_all(&send_invoke(
        &rig.daemon.agent_socket(),
        None,
        "look",
        &rig.project,
        false,
    ));
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED", "{lines:?}");
    assert!(
        last(&lines)["message"]
            .as_str()
            .unwrap()
            .contains("lifecycle"),
        "{lines:?}"
    );
    // a person may
    let lines = read_all(&rig.send("look", &rig.project, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
}

#[test]
fn ai_invokes_a_safe_verb_only_in_the_directory_of_an_app_it_controls() {
    use oxidepm_core::{AppSpec, Runtime};
    let rig = Rig::simple(json!([]), vec![verb("look", "safe", false, false)]);
    let other = rig.kit.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut spec = AppSpec::new(
        "held".into(),
        Runtime::CMD,
        "sleep".into(),
        rig.project.clone(),
    );
    spec.args = vec!["305.2".into()];
    spec.ai_control = true;
    let started = rig.daemon.rpc_on(
        &rig.daemon.socket(),
        "s",
        Some("human:t"),
        json!({ "type": "start", "spec": spec }),
    );
    assert_eq!(started["response"]["type"], "started", "{started}");

    let agent = rig.daemon.agent_socket();
    let lines = read_all(&send_invoke(&agent, None, "look", &rig.project, false));
    assert_eq!(
        last(&lines)["type"],
        "invoke_done",
        "the directory of an app with ai_control: {lines:?}"
    );
    let lines = read_all(&send_invoke(&agent, None, "look", &other, false));
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED", "{lines:?}");
    let _ = rig.daemon.rpc(json!({ "type": "kill" }));
}

#[test]
fn cwd_outside_the_allowed_set_is_refused() {
    let rig = Rig::simple(json!([]), vec![verb("look", "safe", false, false)]);
    let home = rig.kit.home.clone();
    let parent = home.parent().unwrap().to_path_buf();
    for bad in [
        Path::new("/"),
        parent.as_path(),
        home.as_path(),
        &home.join("adapters"),
        Path::new("relative"),
        Path::new("/no/such/dir/here"),
    ] {
        let lines = read_all(&rig.send("look", bad, false));
        assert_eq!(last(&lines)["type"], "error", "{bad:?}: {lines:?}");
        assert_eq!(
            last(&lines)["code"],
            "E_POLICY_DENIED",
            "{bad:?}: {lines:?}"
        );
    }
    assert_eq!(
        rig.started_since(),
        0,
        "no tool was started for a refused directory"
    );
    let lines = read_all(&rig.send("look", &rig.project, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
}

#[test]
fn an_unknown_verb_and_a_disabled_adapter_are_told_apart() {
    let rig = Rig::simple(json!([]), vec![verb("look", "safe", false, false)]);
    let lines = read_all(&rig.send("nonesuch", &rig.project, false));
    assert_eq!(last(&lines)["code"], "E_ADAPTER_MISSING", "{lines:?}");
    let lines = read_all(&rig.send("deploy", &rig.project, false));
    assert!(
        last(&lines)["message"]
            .as_str()
            .unwrap()
            .contains("oxidepm adapter import deploy"),
        "{lines:?}"
    );
    let off = rig.kit.run(&["adapter", "disable", NAME]);
    assert_eq!(off.code, 0, "{}", off.all());
    let lines = read_all(&rig.send("look", &rig.project, false));
    assert_eq!(last(&lines)["code"], "E_ADAPTER_UNAVAILABLE", "{lines:?}");
}

// ---- who may, in what order, and what is written down ---------------------------------------

/// The lines of both audit files, for the records the daemon has written so far.
fn audit_records(rig: &Rig) -> Vec<Value> {
    let mut out = Vec::new();
    for file in ["audit.jsonl", "audit-security.jsonl"] {
        let text =
            std::fs::read_to_string(rig.kit.home.join("audit").join(file)).unwrap_or_default();
        out.extend(
            text.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok()),
        );
    }
    out
}

/// sha256 of the arguments, each followed by a NUL byte, as the audit log writes it.
fn args_digest(args: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for arg in args {
        hash.update(arg.as_bytes());
        hash.update([0u8]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn an_ai_that_asks_for_a_verb_that_is_not_there_is_audited_and_rate_limited_first() {
    let rig = Rig::simple(json!([]), vec![verb("look", "safe", false, false)]);
    let agent = rig.daemon.agent_socket();
    let lines = read_all(&send_invoke(&agent, None, "nonesuch", &rig.project, false));
    assert_eq!(last(&lines)["code"], "E_ADAPTER_MISSING", "{lines:?}");
    // the probe left a line: who asked for what, and that it was refused
    assert!(
        wait_for(20, || audit_records(&rig)
            .iter()
            .any(|r| r["decision"] == "deny"
                && r["selector"] == "nonesuch"
                && r["deny"]["code"] == "E_ADAPTER_MISSING")),
        "{:?}",
        audit_records(&rig)
    );
    // and a directory that is not there too
    let lines = read_all(&send_invoke(
        &agent,
        None,
        "look",
        Path::new("/no/such/dir/here"),
        false,
    ));
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED", "{lines:?}");
    assert!(
        wait_for(20, || audit_records(&rig)
            .iter()
            .any(|r| r["decision"] == "deny"
                && r["selector"] == "look"
                && r["actor"]["class"] == "ai")),
        "{:?}",
        audit_records(&rig)
    );
    // the rate limit is met before anything is looked up: an AI that keeps asking for verbs that
    // are not there is stopped by the rate limit and not told, for ever, that they are missing
    let mut limited_after = None;
    for n in 0..800 {
        let lines = read_all(&send_invoke(&agent, None, "nonesuch", &rig.project, false));
        match last(&lines)["code"].as_str() {
            Some("E_RATE_LIMITED") => {
                limited_after = Some(n);
                break;
            }
            Some("E_ADAPTER_MISSING") => {}
            other => panic!("unexpected answer {other:?}: {lines:?}"),
        }
    }
    assert!(
        limited_after.is_some(),
        "an AI probing for verbs was never rate limited"
    );
    assert_eq!(rig.started_since(), 0, "nothing ran");
}

#[test]
fn a_person_is_not_rate_limited_for_looking_up_a_verb() {
    let rig = Rig::simple(json!([]), vec![verb("look", "safe", false, false)]);
    for _ in 0..150 {
        let lines = read_all(&rig.send("nonesuch", &rig.project, false));
        assert_eq!(last(&lines)["code"], "E_ADAPTER_MISSING", "{lines:?}");
    }
}

#[test]
fn a_rehearsals_tool_cannot_spawn_a_child() {
    let caps = oxidepm_sandbox::probe();
    if caps.landlock_abi.is_none() || !caps.seccomp {
        eprintln!(
            "skipping {}: needs Landlock and seccomp ({})",
            module_path!(),
            caps.summary()
        );
        return;
    }
    eprintln!("probe says: {}", caps.summary());
    // the adapter is granted `spawn = any`, so only the rehearsal's own profile can stop it
    let rig = Rig::new(
        json!([]),
        vec![verb("writer", "caution", true, true)],
        json!({ "spawn": { "mode": "any" } }),
        &[],
    );
    let true_program = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a `true` program");
    rig.kit.write_script(
        NAME,
        json!([{ "on": "command.invoke", "do": [{ "exec": { "path": true_program } }, "reply"] }]),
    );
    let lines = read_all(&rig.send("writer", &rig.project, true));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    let tried = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "exec")
        .expect("the tool tried to start a child");
    assert_eq!(
        tried["ok"], false,
        "a rehearsal cannot start a child: {tried}"
    );
    // the same verb for real, with the same grant, starts it
    let lines = read_all(&rig.send("writer", &rig.project, false));
    assert_eq!(last(&lines)["type"], "invoke_done", "{lines:?}");
    let ran = rig
        .report()
        .into_iter()
        .rfind(|r| r["what"] == "exec")
        .unwrap();
    assert_eq!(ran["ok"], true, "the real call may: {ran}");
}

#[test]
fn the_audit_file_records_an_ai_deny_and_a_persons_invoke_with_the_arguments_hashed() {
    // `make` is a caution verb that exits with 3
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "reply": { "result": { "exit_code": 3, "result": { "ok": false } } } }] }]),
        vec![verb("make", "caution", true, false)],
    );
    let digest = args_digest(&["a", "b"]);
    // an AI is refused
    let lines = read_all(&send_invoke(
        &rig.daemon.agent_socket(),
        None,
        "make",
        &rig.project,
        false,
    ));
    assert_eq!(last(&lines)["code"], "E_POLICY_DENIED", "{lines:?}");
    // a person runs it and it fails
    let lines = read_all(&rig.send("make", &rig.project, false));
    assert_eq!(
        (
            last(&lines)["type"].as_str(),
            last(&lines)["exit_code"].as_i64()
        ),
        (Some("invoke_done"), Some(3)),
        "{lines:?}"
    );

    assert!(
        wait_for(20, || {
            let records = audit_records(&rig);
            records
                .iter()
                .any(|r| r["phase"] == "result" && r["ok"] == false)
                && records
                    .iter()
                    .any(|r| r["decision"] == "deny" && r["selector"] == "make")
        }),
        "{:?}",
        audit_records(&rig)
    );
    let records = audit_records(&rig);
    let invoke: Vec<&Value> = records.iter().filter(|r| r["selector"] == "make").collect();
    // every line about the call has the verb as its selector, and the arguments only as a count
    // and a digest
    for r in &invoke {
        assert_eq!(r["argc"], 2, "{r}");
        assert_eq!(r["args_sha256"], digest.as_str(), "{r}");
        assert!(
            !r.to_string().contains("\"a\",\"b\""),
            "the arguments themselves are not written: {r}"
        );
    }
    let deny = invoke
        .iter()
        .find(|r| r["decision"] == "deny")
        .expect("the AI's refusal is on file");
    assert_eq!(deny["actor"]["class"], "ai");
    assert_eq!(deny["deny"]["code"], "E_POLICY_DENIED");
    let allow = invoke
        .iter()
        .find(|r| r["decision"] == "allow")
        .expect("the person's call is on file");
    assert_eq!(allow["actor"]["class"], "human");
    assert_eq!(allow["tier"], "caution");
    let result = records
        .iter()
        .find(|r| r["phase"] == "result" && r["req"] == allow["req"])
        .expect("the person's call has a result line");
    assert_eq!(
        result["ok"], false,
        "a verb that exits with 3 is a failed result: {result}"
    );
}

// ---- the CLI ---------------------------------------------------------------------------------

#[test]
fn the_verb_table_follows_the_adapters_and_the_cli_relays_a_verb() {
    let rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "invoke_output": { "text": "hello from the tool" } }, { "invoke_output": { "stream": "stderr", "text": "warn\u{1b}[2Jx" } }, { "reply": { "result": { "exit_code": 4, "result": { "ok": false } } } }] }]),
        vec![verb("probe", "safe", false, false)],
    );
    let table_path = rig.kit.home.join("adapters").join("verbs.json");
    let table: Value = serde_json::from_slice(&std::fs::read(&table_path).unwrap()).unwrap();
    assert_eq!(table["verbs"][0]["verb"], "probe");
    assert_eq!(table["verbs"][0]["adapter"], NAME);
    assert_eq!(table["verbs"][0]["tier"], "safe");

    let mut cmd = rig.kit.cli();
    cmd.current_dir(&rig.project).args(["probe", "x"]);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(4), "the exit code of the verb");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the tool\n"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("warn") && !err.contains('\u{1b}'),
        "the terminal is not handed escape sequences: {err:?}"
    );

    let mut cmd = rig.kit.cli();
    cmd.current_dir(&rig.project).args(["--json", "probe"]);
    let out = cmd.output().unwrap();
    let json: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(json, json!({ "exit_code": 4, "result": { "ok": false } }));

    // disabling takes the verb out of the table
    assert_eq!(rig.kit.run(&["adapter", "disable", NAME]).code, 0);
    let table: Value = serde_json::from_slice(&std::fs::read(&table_path).unwrap()).unwrap();
    assert_eq!(table["verbs"], json!([]));
}

#[test]
fn unknown_verb_prints_the_adapter_hint() {
    let kit = Kit::new();
    let run = kit.run(&["git-start"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(
        run.stderr.contains("oxidepm adapter import git-start"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("not installed"), "{}", run.stderr);
    // nobody provides this one: clap's message alone
    let run = kit.run(&["nonesuch-command"]);
    assert_eq!(run.code, 2);
    assert!(!run.stderr.contains("adapter import"), "{}", run.stderr);
}

#[test]
fn deploy_and_diff_commands_print_moved_message() {
    let kit = Kit::new();
    for args in [
        &["deploy", "myhost"][..],
        &["deploy", "myhost", "--dry-run"][..],
        &["diff", "myhost"][..],
    ] {
        let out = kit
            .cli()
            .env("PATH", &kit.home)
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        let hint = format!(
            "'{}' is provided by the adapter 'deploy', which is not installed",
            args[0]
        );
        assert!(stderr.contains(&hint), "{args:?}: {stderr}");
        assert!(
            stderr.contains("oxidepm adapter import deploy"),
            "{args:?}: {stderr}"
        );
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started"
    );
}

#[test]
fn git_start_command_prints_moved_message() {
    let kit = Kit::new();
    let url = "https://example.com/a/b.git";
    let run = kit.run(&["git-start", url]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(
        run.stderr.contains("oxidepm adapter import git-start"),
        "{}",
        run.stderr
    );

    for (flag, value) in [
        ("--git", url),
        ("--branch", "main"),
        ("--clone-dir", "project"),
    ] {
        let run = kit.run(&["start", flag, value, "--help"]);
        assert_eq!(run.code, 2, "{flag}: {}", run.all());
        assert!(
            run.stderr.contains(flag) && run.stderr.contains("oxidepm git-start"),
            "{}",
            run.stderr
        );
    }
    let run = kit.run(&["start"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(run.stderr.contains("<TARGET>"), "{}", run.stderr);

    for args in [
        vec!["start", "--git", url],
        vec!["start", &format!("--git={url}")],
    ] {
        let run = kit.run(&args);
        assert_eq!(run.code, 2, "{}", run.all());
        assert!(run.stderr.contains("oxidepm git-start"), "{}", run.stderr);
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started"
    );
}

#[test]
fn notify_command_prints_moved_message() {
    // `oxidepm notify` was a command of the core; the notifier adapter provides it now
    let kit = Kit::new();
    for args in [
        vec!["notify", "status"],
        vec!["notify", "telegram", "--chat", "1"],
        vec!["notify", "test"],
    ] {
        let run = kit.run(&args);
        assert_eq!(run.code, 2, "{args:?}: {}", run.all());
        assert!(
            run.stderr
                .contains("'notify' is provided by the adapter 'notifier', which is not installed"),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr.contains("oxidepm adapter import notifier"),
            "{}",
            run.stderr
        );
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started to say so"
    );
    assert!(
        !kit.home.join("notify.toml").exists(),
        "and nothing was written"
    );
}

#[test]
fn web_command_prints_moved_message() {
    // `oxidepm web` was a command of the core (a server in the CLI process); the ui-web adapter provides it now
    let kit = Kit::new();
    for args in [
        vec!["web"],
        vec!["web", "status"],
        vec!["web", "new-key"],
        vec!["web", "--port", "9615", "--api-key", "x"],
    ] {
        let run = kit.run(&args);
        assert_eq!(run.code, 2, "{args:?}: {}", run.all());
        assert!(
            run.stderr
                .contains("'web' is provided by the adapter 'ui-web', which is not installed"),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr.contains("oxidepm adapter import ui-web"),
            "{}",
            run.stderr
        );
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started to say so"
    );
}

#[test]
fn monit_command_prints_moved_message() {
    // `oxidepm monit` was a dashboard inside the CLI; the ui-tui adapter provides it now
    let kit = Kit::new();
    for args in [vec!["monit"], vec!["monit", "--fast"]] {
        let run = kit.run(&args);
        assert_eq!(run.code, 2, "{args:?}: {}", run.all());
        assert!(
            run.stderr
                .contains("'monit' is provided by the adapter 'ui-tui', which is not installed"),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr.contains("oxidepm adapter import ui-tui"),
            "{}",
            run.stderr
        );
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started to say so"
    );
}

#[test]
fn clap_typo_suggests_and_starts_no_daemon() {
    let kit = Kit::new();
    let run = kit.run(&["statu"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(
        run.stderr.contains("status"),
        "a suggestion is printed: {}",
        run.stderr
    );
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started"
    );
    assert!(!kit.home.join("daemon.lock").exists() && !kit.home.join("daemon.pid").exists());
    // a verb that is in the table, with no daemon running, is not started either
    std::fs::create_dir_all(kit.home.join("adapters")).unwrap();
    std::fs::write(
        kit.home.join("adapters").join("verbs.json"),
        json!({ "verbs": [{ "verb": "probe", "adapter": "x", "summary": "", "tier": "safe", "mode": "relayed" }] }).to_string(),
    )
    .unwrap();
    let run = kit.run(&["probe"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("not running"), "{}", run.stderr);
    assert!(!kit.home.join("daemon.sock").exists(), "still no daemon");
}

#[test]
fn start_check_starts_the_daemon_and_passes_option_like_targets() {
    let mut rig = Rig::simple(
        json!([{ "on": "command.invoke", "do": [{ "reply": { "result": { "exit_code": 3 } } }] }]),
        vec![verb("check", "safe", false, false)],
    );
    let kit = &rig.kit;
    let project = &rig.project;
    let run_check = |target: &[&str]| {
        kit.cli()
            .envs(kit.script_env(NAME))
            .env(
                "FIXTURE_REPORT",
                kit.home.join("adapters/state/tools/report.jsonl"),
            )
            .current_dir(project)
            .args(["start", "--check"])
            .args(target)
            .output()
            .unwrap()
    };
    let live = run_check(&["app.js"]);
    assert_eq!(rig.daemon.rpc(json!({"type": "kill"}))["type"], "ok");
    assert!(rig.daemon.wait_exit().success());
    let cold = run_check(&["--", "-app.js"]);
    if rig.daemon.socket().exists() {
        let stopped = rig.kit.run(&["kill"]);
        assert_eq!(stopped.code, 0, "{}", stopped.all());
        assert!(wait_for(20, || !rig.daemon.socket().exists()));
    }

    let invocations: Vec<_> = rig
        .report()
        .into_iter()
        .filter(|r| r["dir"] == "in" && r["frame"]["method"] == "command.invoke")
        .map(|r| r["frame"]["params"]["argv"].clone())
        .collect();
    let live_text = String::from_utf8_lossy(&live.stderr);
    let cold_text = String::from_utf8_lossy(&cold.stderr);
    assert!(
        live_text.contains("Preflight checks failed")
            && cold_text.contains("Preflight checks failed")
            && invocations == [json!(["--", "app.js"]), json!(["--", "-app.js"])],
        "live: {live_text}; cold: {cold_text}; argv: {invocations:?}",
    );
}

#[test]
fn check_and_init_commands_print_moved_message() {
    let kit = Kit::new();
    for args in [
        vec!["check", "."],
        vec!["check", ".", "--fix"],
        vec!["init"],
        vec!["init", "dir"],
    ] {
        let verb = args[0];
        let run = kit.run(&args);
        assert_eq!(run.code, 2, "{args:?}: {}", run.all());
        assert!(
            run.stderr.contains(&format!(
                "'{verb}' is provided by the adapter '{verb}', which is not installed"
            )),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr
                .contains(&format!("oxidepm adapter import {verb}")),
            "{}",
            run.stderr
        );
    }
    assert!(
        !kit.home.join("daemon.sock").exists(),
        "no daemon was started"
    );
}
