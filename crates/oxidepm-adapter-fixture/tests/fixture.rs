//! The fixture adapter, driven over a direct pipe (no daemon): its protocol answers, the script
//! engine, the report file, and a smoke test for each fixture action. A smoke test checks
//! only that the behaviour happens.
//!
//! Tests wait for things to happen (a frame, a report entry) and never for a fixed time, except
//! for the negative checks ("nothing arrives"), which can only pass too easily under load, never
//! fail.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use oxidepm_adapter_fixture::report::read_report;
use oxidepm_adapter_proto::{ErrorObject, Frame, FrameReader, Read};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_oxidepm-fixture-adapter");
const LIMIT: Duration = Duration::from_secs(15);
const QUIET: Duration = Duration::from_millis(300);
/// The limit of the real protocol for a line from an adapter.
const MAX_LINE: usize = 256 * 1024;

const GOLDEN_INITIALIZE: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/initialize.request.json");
const GOLDEN_INITIALIZE_REPLY: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/initialize.response.json");
const GOLDEN_PING: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/ping.request.json");
const GOLDEN_PING_REPLY: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/ping.response.json");
const GOLDEN_SHUTDOWN: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/shutdown.request.json");
const GOLDEN_EVENT: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/event.notification.json");
const GOLDEN_PREPARE: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/runtime.prepare.request.json");
const GOLDEN_PREPARE_REPLY: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/runtime.prepare.response.json");
const GOLDEN_PLAN_REPLY: &str =
    include_str!("../../oxidepm-adapter-proto/tests/golden/runtime.plan.response.json");

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("golden file is JSON")
}

/// `request` with another id.
fn with_id(text: &str, id: u64) -> String {
    let mut v = value(text);
    v["id"] = json!(id);
    v.to_string()
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.contains(") Z "))
            .unwrap_or(false)
}

struct Fx {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Read>,
    dir: tempfile::TempDir,
    report: PathBuf,
    stderr: PathBuf,
    reaped: Option<ExitStatus>,
}

#[cfg(target_os = "linux")]
fn exclude_test_runner_descriptors(command: &mut Command) {
    let inherited: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd >= 3 && unsafe { libc::fcntl(*fd, libc::F_GETFD) } & libc::FD_CLOEXEC == 0)
        .collect();
    // SAFETY: change only descriptor flags in the forked fixture process.
    unsafe {
        command.pre_exec(move || {
            for fd in &inherited {
                if libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EBADF) {
                        return Err(error);
                    }
                }
            }
            Ok(())
        });
    }
}

impl Fx {
    fn start(script: Value) -> Fx {
        Fx::start_env(script, &[], true)
    }

    fn start_env(script: Value, extra_env: &[(&str, &str)], protocols: bool) -> Fx {
        let dir = tempfile::tempdir().expect("temp dir");
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let report = dir.path().join("report.jsonl");
        let stderr = dir.path().join("stderr.log");
        std::fs::create_dir(dir.path().join("state")).unwrap();
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("FIXTURE_SCRIPT", &script_path)
            .env("FIXTURE_REPORT", &report)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .process_group(0);
        if protocols {
            command.env("OXIDEPM_ADAPTER_PROTOCOLS", "1");
        }
        for (k, v) in extra_env {
            command.env(k, v);
        }
        #[cfg(target_os = "linux")]
        exclude_test_runner_descriptors(&mut command);
        let mut child = command.spawn().expect("start the fixture");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = FrameReader::new(BufReader::new(stdout), MAX_LINE);
            while let Ok(read) = reader.read_frame() {
                let eof = read == Read::Eof;
                if tx.send(read).is_err() || eof {
                    break;
                }
            }
        });
        Fx {
            child,
            stdin,
            rx,
            dir,
            report,
            stderr,
            reaped: None,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// The next thing the fixture wrote.
    fn recv(&self) -> Read {
        match self.rx.recv_timeout(LIMIT) {
            Ok(read) => read,
            Err(e) => panic!("the fixture wrote nothing: {e}"),
        }
    }

    fn recv_frame(&self) -> Frame {
        match self.recv() {
            Read::Frame(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// The `result` object of the next frame, which must be a response to `id`.
    fn recv_result(&self, id: u64) -> Value {
        match self.recv_frame() {
            Frame::Response { id: got, result } => {
                assert_eq!(got, id);
                Value::Object(result)
            }
            other => panic!("expected a response to {id}, got {other:?}"),
        }
    }

    fn recv_error(&self, id: u64) -> ErrorObject {
        match self.recv_frame() {
            Frame::Error { id: got, error } => {
                assert_eq!(got, id);
                error
            }
            other => panic!("expected an error for {id}, got {other:?}"),
        }
    }

    /// Nothing arrives for a short while.
    fn expect_quiet(&self) {
        match self.rx.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("expected silence, got {other:?}"),
        }
    }

    fn report(&self) -> Vec<Value> {
        read_report(&self.report)
    }

    /// The first report entry of kind `what` that `pred` accepts, waiting for it to be written.
    fn wait_entry(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(entry) = self
                .report()
                .into_iter()
                .find(|e| e["what"] == what && pred(e))
            {
                return entry;
            }
            assert!(
                Instant::now() < deadline,
                "no {what:?} entry in the report: {:#?}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn entry(&self, what: &str) -> Value {
        self.wait_entry(what, |_| true)
    }

    fn init(&mut self) -> Value {
        let state = self.path("state");
        let mut v = value(GOLDEN_INITIALIZE);
        v["params"]["paths"]["state"] = json!(state.to_string_lossy());
        self.send(&v.to_string());
        self.recv_result(1)
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = Some(status);
                return status;
            }
            assert!(Instant::now() < deadline, "the fixture did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn running(&mut self) -> bool {
        self.reaped.is_none() && self.child.try_wait().unwrap().is_none()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        // Only the group of a fixture that is still ours (not yet waited for, so its pid cannot
        // have been reused).
        if self.reaped.is_none() {
            unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Grandchildren that left the group: only those the report names, and only while the
        // command line is still exactly what was asked for.
        for entry in read_report(&self.report)
            .into_iter()
            .filter(|e| e["what"] == "grandchild")
        {
            let (Some(pid), Some(argv)) = (entry["child_pid"].as_u64(), entry["argv"].as_array())
            else {
                continue;
            };
            let want: Vec<u8> = argv
                .iter()
                .flat_map(|a| a.as_str().unwrap_or("").bytes().chain(Some(0)))
                .collect();
            if std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|c| c == want)
                .unwrap_or(false)
            {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

/// A command line that no other test or program uses: `sleep 31.<n>`.
fn marker_sleep() -> Value {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    json!(["sleep", format!("31.{}{:04}", std::process::id() % 1000, n)])
}

// ---- the protocol ---------------------------------------------------------------------------

#[test]
fn fixture_answers_initialize() {
    let mut fx = Fx::start(json!([]));
    fx.send(GOLDEN_INITIALIZE.trim());
    let frame = fx.recv_frame();
    assert_eq!(
        frame.to_value(),
        value(GOLDEN_INITIALIZE_REPLY),
        "the golden reply, name and version echoed"
    );

    // the highest protocol both speak
    fx.send(&request(2, "initialize", {
        let mut p = value(GOLDEN_INITIALIZE)["params"].clone();
        p["protocols"] = json!([1, 7, 9]);
        p
    }));
    assert_eq!(fx.recv_result(2)["protocol"], 1);

    // none in common: -32022 with the supported list
    fx.send(&request(3, "initialize", {
        let mut p = value(GOLDEN_INITIALIZE)["params"].clone();
        p["protocols"] = json!([7]);
        p
    }));
    let error = fx.recv_error(3);
    assert_eq!(error.code, -32022);
    assert_eq!(error.name(), Some("E_ADAPTER_PROTOCOL"));
    assert_eq!(error.data.unwrap()["supported"], json!([1]));
}

#[test]
fn fixture_answers_the_other_golden_requests() {
    let mut fx = Fx::start(json!([]));
    fx.send(GOLDEN_PING.trim());
    assert_eq!(fx.recv_frame().to_value(), value(GOLDEN_PING_REPLY));
    // an unknown method is -32601, an unknown notification is ignored
    fx.send(&request(8, "no.such.method", json!({})));
    assert_eq!(fx.recv_error(8).code, -32601);
    fx.send(&notification("no.such.notification", json!({})));
    fx.send(GOLDEN_EVENT.trim());
    fx.send(&request(9, "runtime.prepare", json!({})));
    assert_eq!(fx.recv_result(9), json!({ "steps": [], "notes": [] }));
    fx.send(&request(10, "command.invoke", json!({})));
    assert_eq!(fx.recv_result(10), json!({ "exit_code": 0 }));
    fx.send(&request(11, "runtime.plan", json!({})));
    assert_eq!(fx.recv_error(11).code, -32603, "no plan is scripted");
}

#[test]
fn fixture_shutdown_replies_then_exits_cleanly() {
    let mut fx = Fx::start(json!([]));
    fx.send(GOLDEN_SHUTDOWN.trim());
    assert_eq!(
        fx.recv_frame().to_value(),
        json!({ "jsonrpc": "2.0", "id": 3, "result": {} })
    );
    assert!(fx.wait_exit().success());
}

#[test]
fn fixture_exits_on_stdin_eof() {
    let mut fx = Fx::start(json!([]));
    fx.close_stdin();
    assert_eq!(fx.wait_exit().code(), Some(0));
}

#[test]
fn fixture_refuses_to_run_by_hand() {
    let mut fx = Fx::start_env(json!([]), &[], false);
    assert_eq!(fx.wait_exit().code(), Some(1));
    let text = std::fs::read_to_string(&fx.stderr).unwrap();
    assert!(
        text.contains("this is an OxidePM adapter; it is started by oxidepmd"),
        "{text}"
    );
}

#[test]
fn fixture_refuses_a_script_it_cannot_read() {
    let mut fx = Fx::start(json!({ "not": "a list" }));
    assert_eq!(fx.wait_exit().code(), Some(2));
    assert!(std::fs::read_to_string(&fx.stderr)
        .unwrap()
        .contains("array of rules"));
}

// ---- the script -----------------------------------------------------------------------------

#[test]
fn fixture_script_selects_behaviour() {
    // the first rule that applies fires; `times` ends a rule, `after` delays one
    let mut fx = Fx::start(json!([
        { "on": "ping", "times": 1, "do": { "error": { "code": -32603, "message": "first" } } },
        { "on": "ping", "after": 2, "do": { "reply": { "result": { "late": true } } } },
        { "on": "start", "do": "hang" },
        { "on": "after_ms:20", "do": "hang" },
        { "on": "event", "do": "hang" },
        { "on": "stdin_eof", "do": [ { "hang": {} }, { "exit_clean": {} } ] },
    ]));
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(fx.recv_error(1).message, "first");
    fx.send(&with_id(GOLDEN_PING, 2));
    assert_eq!(
        fx.recv_result(2),
        json!({}),
        "rule 0 is spent and rule 1 is not due: the default answer"
    );
    fx.send(&with_id(GOLDEN_PING, 3));
    assert_eq!(fx.recv_result(3), json!({}));
    fx.send(&with_id(GOLDEN_PING, 4));
    assert_eq!(
        fx.recv_result(4),
        json!({ "late": true }),
        "the fourth ping is the first past `after: 2`"
    );

    // every kind of trigger
    fx.send(GOLDEN_EVENT.trim());
    let fired = |fx: &Fx, index: u64| fx.wait_entry("rule", |e| e["index"] == index);
    assert_eq!(fired(&fx, 2)["on"], "Start");
    assert_eq!(fired(&fx, 3)["on"], "AfterMs(20)");
    assert_eq!(fired(&fx, 4)["on"], "Method(\"event\")");
    fx.close_stdin();
    assert_eq!(fired(&fx, 5)["do"], json!(["hang", "exit_clean"]));
    assert_eq!(fx.wait_exit().code(), Some(0));
}

#[test]
fn a_rule_can_be_limited_to_one_install_path() {
    // the path of the fixture binary in a test is under target/, never under "version-9"
    let mut fx = Fx::start(json!([
        { "on": "ping", "only_in": "version-9", "do": { "error": { "code": -32603, "message": "only there" } } },
        { "on": "ping", "only_in": "oxidepm-fixture-adapter", "do": { "reply": { "result": { "here": true } } } },
    ]));
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(
        fx.recv_result(1),
        json!({ "here": true }),
        "the first rule is for another path, the second matches"
    );
    fx.close_stdin();
    fx.wait_exit();
}

#[test]
fn fixture_report_file_records_what_it_saw() {
    let mut fx = Fx::start(json!([]));
    fx.init();
    fx.send(&with_id(GOLDEN_PING, 2));
    fx.recv_result(2);
    fx.send("this is not a frame");
    fx.entry("violation");
    fx.close_stdin();
    assert_eq!(fx.wait_exit().code(), Some(0));

    // every line is a whole JSON object with the common fields
    let text = std::fs::read_to_string(&fx.report).unwrap();
    for line in text.lines() {
        let v: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("torn line {line:?}: {e}"));
        assert!(
            v["ms"].is_u64() && v["pid"] == fx.pid() && v["what"].is_string(),
            "{line}"
        );
    }
    let report = fx.report();
    let kinds: Vec<&str> = report.iter().map(|e| e["what"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"start"));
    assert_eq!(kinds.last(), Some(&"exit"));
    let frames = |dir: &str| -> Vec<&Value> {
        report
            .iter()
            .filter(|e| e["what"] == "frame" && e["dir"] == dir)
            .collect()
    };
    let seen: Vec<&str> = frames("in")
        .iter()
        .map(|e| e["frame"]["method"].as_str().unwrap())
        .collect();
    assert_eq!(seen, ["initialize", "ping"]);
    assert_eq!(frames("out").len(), 2, "the two answers");
    assert!(report.iter().any(|e| e["what"] == "violation"));
    assert!(report.iter().any(|e| e["what"] == "stdin_eof"));
}

// ---- one smoke test per behaviour -----------------------------------------------------------

#[test]
fn smoke_reply_and_error() {
    let mut fx = Fx::start(json!([
        { "on": "runtime.prepare", "do": { "reply": { "result": value(GOLDEN_PREPARE_REPLY)["result"] } } },
        { "on": "runtime.plan", "do": { "reply": { "result": value(GOLDEN_PLAN_REPLY)["result"] } } },
        { "on": "command.invoke", "times": 1, "do": { "error": { "code": -32023, "message": "no" } } },
        { "on": "command.invoke", "do": { "error": { "code": -32000, "message": "odd", "name": "E_FIXTURE" } } },
    ]));
    fx.send(&with_id(GOLDEN_PREPARE, 1));
    assert_eq!(
        fx.recv_frame().to_value()["result"],
        value(GOLDEN_PREPARE_REPLY)["result"]
    );
    fx.send(&request(2, "runtime.plan", json!({})));
    assert_eq!(fx.recv_result(2), value(GOLDEN_PLAN_REPLY)["result"]);
    fx.send(&request(3, "command.invoke", json!({})));
    let e = fx.recv_error(3);
    assert_eq!(
        (e.code, e.name()),
        (-32023, Some("E_POLICY_DENIED")),
        "the usual name of the code"
    );
    fx.send(&request(4, "command.invoke", json!({})));
    assert_eq!(fx.recv_error(4).name(), Some("E_FIXTURE"));
}

#[test]
fn smoke_invoke_output_reports_progress_for_the_call_being_handled() {
    let mut fx = Fx::start(json!([
        { "on": "command.invoke", "do": [
            { "invoke_output": { "text": "one" } },
            { "invoke_output": { "stream": "status", "bytes": 5, "count": 2 } },
            "reply"
        ] },
    ]));
    fx.send(&request(
        7,
        "command.invoke",
        json!({ "verb": "v", "cwd": "/", "actor": { "class": "human", "label": "t" } }),
    ));
    let mut frames = Vec::new();
    for _ in 0..3 {
        frames.push(fx.recv_frame().to_value());
    }
    assert_eq!(frames[0]["method"], "invoke.output");
    assert_eq!(
        frames[0]["params"],
        json!({ "id": 7, "stream": "stdout", "text": "one" })
    );
    assert_eq!(
        frames[1]["params"],
        json!({ "id": 7, "stream": "status", "text": "xxxxx" })
    );
    assert_eq!(frames[2]["params"]["id"], 7);
    assert_eq!(
        fx.recv_result(7)["exit_code"],
        0,
        "the answer comes after the progress"
    );
}

#[test]
fn smoke_sleep_answers_late_and_keeps_reading() {
    let mut fx = Fx::start(
        json!([{ "on": "ping", "times": 1, "do": [ { "sleep": { "ms": 400 } }, "reply" ] }]),
    );
    let sent = Instant::now();
    fx.send(&with_id(GOLDEN_PING, 1));
    fx.send(&with_id(GOLDEN_PING, 2));
    assert_eq!(
        fx.recv_result(2),
        json!({}),
        "the second ping is answered while the first sleeps"
    );
    assert_eq!(fx.recv_result(1), json!({}));
    assert!(sent.elapsed() >= Duration::from_millis(400));
}

#[test]
fn smoke_hang_reads_and_never_answers() {
    let mut fx = Fx::start(json!([{ "on": "ping", "times": 1, "do": "hang" }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    fx.expect_quiet();
    fx.entry("hang");
    fx.send(&with_id(GOLDEN_PING, 2));
    assert_eq!(fx.recv_result(2), json!({}), "still reading");
}

#[test]
fn smoke_deaf_stops_reading() {
    let mut fx = Fx::start(json!([{ "on": "ping", "times": 1, "do": ["reply", "deaf"] }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    fx.recv_result(1);
    fx.entry("deaf");
    fx.send(&with_id(GOLDEN_PING, 2));
    fx.expect_quiet();
    let pings = fx
        .report()
        .iter()
        .filter(|e| e["what"] == "frame" && e["dir"] == "in")
        .count();
    assert_eq!(pings, 1, "the second ping was never read");
    assert!(fx.running());
}

#[test]
fn smoke_crash_with_a_code() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": { "crash": { "code": 7 } } }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(fx.wait_exit().code(), Some(7));
}

#[test]
fn smoke_abort_dies_by_sigabrt() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": "abort" }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(fx.wait_exit().signal(), Some(libc::SIGABRT));
}

#[test]
fn smoke_exit_clean_after_a_reply() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": ["reply", "exit_clean"] }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    fx.recv_result(1);
    assert_eq!(fx.wait_exit().code(), Some(0));
}

#[test]
fn smoke_ignore_sigterm() {
    let mut fx = Fx::start(json!([{ "on": "start", "do": "ignore_sigterm" }]));
    fx.entry("ignore_sigterm");
    unsafe { libc::kill(fx.pid() as libc::pid_t, libc::SIGTERM) };
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(
        fx.recv_result(1),
        json!({}),
        "it is still alive and reading after SIGTERM"
    );
    assert!(fx.running());
}

#[test]
fn smoke_ignore_shutdown_survives_shutdown_and_eof() {
    let mut fx = Fx::start(json!([{ "on": "shutdown", "do": "ignore_shutdown" }]));
    fx.send(GOLDEN_SHUTDOWN.trim());
    fx.expect_quiet();
    fx.close_stdin();
    fx.entry("stdin_eof");
    std::thread::sleep(QUIET);
    assert!(fx.running(), "it outlives the polite requests");
}

#[test]
fn smoke_spawn_grandchild_outlives_the_fixture_in_its_group() {
    let mut fx = Fx::start(
        json!([{ "on": "start", "do": { "spawn_grandchild": { "cmd": marker_sleep() } } }]),
    );
    let entry = fx.entry("grandchild");
    let child = entry["child_pid"].as_u64().unwrap() as u32;
    assert!(pid_alive(child));
    assert_eq!(
        entry["pgid"],
        fx.pid(),
        "same process group as the fixture, which leads it"
    );
    fx.child.kill().unwrap();
    fx.wait_exit();
    assert!(pid_alive(child), "it outlived the fixture");
    // `Drop` ends it: the report names it and its command line still matches
}

#[test]
fn smoke_spawn_setsid_grandchild_leaves_the_group() {
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "spawn_setsid_grandchild": { "cmd": marker_sleep() } } }]),
    );
    let entry = fx.entry("grandchild");
    let child = entry["child_pid"].as_u64().unwrap();
    assert!(pid_alive(child as u32));
    assert_eq!(entry["pgid"], child, "its own group, led by itself");
    assert_ne!(entry["pgid"], fx.pid());
}

#[test]
fn smoke_flood_stdout_sends_valid_frames() {
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "flood_stdout": { "fps": 0, "bytes": 200, "count": 300 } } }]),
    );
    for n in 0..300u64 {
        match fx.recv_frame() {
            Frame::Notification { method, params } => {
                assert_eq!(method, "fixture.flood");
                assert_eq!(params["n"], n);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(fx.wait_entry("flood", |_| true)["frames"], 300);
}

#[test]
fn smoke_flood_stdout_junk_sends_lines_that_are_not_frames() {
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "flood_stdout": { "fps": 0, "bytes": 50, "count": 20, "junk": true } } }]),
    );
    for _ in 0..20 {
        assert!(matches!(fx.recv(), Read::Violation(_)));
    }
}

#[test]
fn smoke_flood_stderr_writes_junk_to_stderr() {
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "flood_stderr": { "bps": 20000, "secs": 1 } } }]),
    );
    let done = fx.wait_entry("flood", |e| e["stream"] == "stderr");
    assert!(done["bytes"].as_u64().unwrap() >= 20000);
    let written = std::fs::metadata(&fx.stderr).unwrap().len();
    assert!(written >= 20000, "{written} bytes on stderr");
    // stdout carries nothing
    fx.expect_quiet();
}

#[test]
fn smoke_giant_line_is_skipped_by_the_bounded_reader() {
    let mut fx = Fx::start(json!([{ "on": "start", "do": { "giant_line": { "bytes": 300000 } } }]));
    assert_eq!(fx.recv(), Read::Overflow { discarded: 300000 });
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(fx.recv_result(1), json!({}), "in step again after the line");
}

#[test]
fn smoke_malformed_lines() {
    let fx = Fx::start(json!([
        { "on": "start", "do": [
            { "garbage": { "text": "not json at all" } },
            "invalid_utf8", "nul", "array_frame",
        ] }
    ]));
    use oxidepm_adapter_proto::FrameError::*;
    let kinds: Vec<_> = (0..4)
        .map(|_| match fx.recv() {
            Read::Violation(e) => e,
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(matches!(kinds[0], NotJson(_)), "{:?}", kinds[0]);
    assert_eq!(kinds[1], InvalidUtf8);
    assert_eq!(kinds[2], Nul);
    assert_eq!(kinds[3], Batch);
}

#[test]
fn smoke_partial_line_then_eof() {
    let fx = Fx::start(json!([{ "on": "start", "do": "partial_line_then_eof" }]));
    assert!(matches!(fx.recv(), Read::Truncated { .. }));
    assert_eq!(fx.recv(), Read::Eof);
}

#[test]
fn smoke_wrong_id_duplicate_and_unsolicited_replies() {
    let mut fx = Fx::start(json!([
        { "on": "ping", "times": 1, "do": "wrong_id" },
        { "on": "ping", "times": 1, "do": { "wrong_id": { "id": 55 } } },
        { "on": "ping", "times": 1, "do": "duplicate_reply" },
        { "on": "ping", "times": 1, "do": { "unsolicited_response": { "id": 77 } } },
    ]));
    fx.send(&with_id(GOLDEN_PING, 1));
    assert_eq!(fx.recv_result(1_000_001), json!({}), "an id nobody issued");
    fx.send(&with_id(GOLDEN_PING, 2));
    assert_eq!(fx.recv_result(55), json!({}));
    fx.send(&with_id(GOLDEN_PING, 3));
    let (first, second) = (fx.recv_frame(), fx.recv_frame());
    assert_eq!(first, second, "the same reply twice");
    assert!(matches!(first, Frame::Response { id: 3, .. }));
    fx.send(&with_id(GOLDEN_PING, 4));
    assert_eq!(fx.recv_result(77), json!({}), "a response nobody asked for");
    fx.expect_quiet();
}

#[test]
fn smoke_lies_in_the_initialize_reply() {
    let mut fx = Fx::start(json!([
        { "on": "initialize", "times": 1, "do": "lie_name" },
        { "on": "initialize", "times": 1, "do": { "lie_version": { "version": "6.6.6" } } },
        { "on": "initialize", "times": 1, "do": "lie_protocol" },
    ]));
    let golden = value(GOLDEN_INITIALIZE_REPLY)["result"].clone();
    fx.send(&with_id(GOLDEN_INITIALIZE, 1));
    let r = fx.recv_result(1);
    assert_eq!(
        (&r["name"], &r["version"], &r["protocol"]),
        (
            &json!("not-the-adapter"),
            &golden["version"],
            &golden["protocol"]
        )
    );
    fx.send(&with_id(GOLDEN_INITIALIZE, 2));
    let r = fx.recv_result(2);
    assert_eq!(
        (&r["name"], &r["version"]),
        (&golden["name"], &json!("6.6.6"))
    );
    fx.send(&with_id(GOLDEN_INITIALIZE, 3));
    assert_eq!(fx.recv_result(3)["protocol"], 99);
}

/// Answer the fixture's `host.request` or `host.audit_note` with an empty result.
fn answer_daemon_request(fx: &mut Fx, method: &str) -> (u64, Value) {
    match fx.recv_frame() {
        Frame::Request {
            id,
            method: got,
            params,
        } => {
            assert_eq!(got, method);
            fx.send(&json!({ "jsonrpc": "2.0", "id": id, "result": { "response": { "type": "pong" } } }).to_string());
            (id, Value::Object(params))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn smoke_host_request_and_the_answer_is_recorded() {
    let mut fx = Fx::start(json!([
        { "on": "ping", "times": 1, "do": { "host_request": { "request": { "type": "status" } } } },
        { "on": "ping", "times": 1, "do": { "host_request": { "request": { "type": "stop" }, "dry_run": true } } },
    ]));
    fx.send(&with_id(GOLDEN_PING, 1));
    let (id, params) = answer_daemon_request(&mut fx, "host.request");
    assert_eq!(id, 1, "the fixture numbers its own requests from 1");
    assert_eq!(params, json!({ "request": { "type": "status" } }));
    let answer = fx.wait_entry("host_answer", |e| e["id"] == 1);
    assert_eq!(answer["frame"]["result"]["response"]["type"], "pong");
    fx.send(&with_id(GOLDEN_PING, 2));
    let (id, params) = answer_daemon_request(&mut fx, "host.request");
    assert_eq!(id, 2, "never reuses an id");
    assert_eq!(params["dry_run"], true);
}

#[test]
fn smoke_forge_actor_puts_forged_fields_in_the_request_and_the_params() {
    let mut fx =
        Fx::start(json!([{ "on": "ping", "do": { "forge_actor": { "actor": "human:root" } } }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    let (_, params) = answer_daemon_request(&mut fx, "host.request");
    for place in [&params, &params["request"]] {
        assert_eq!(place["actor"], "human:root");
        assert_eq!(place["reveal"], true);
        assert_eq!(place["dry_run"], false);
    }
    assert_eq!(params["request"]["type"], "ping", "the default request");
}

#[test]
fn smoke_audit_note() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": { "audit_note": { "text": "hello" } } }]));
    fx.send(&with_id(GOLDEN_PING, 1));
    let (_, params) = answer_daemon_request(&mut fx, "host.audit_note");
    assert_eq!(params, json!({ "text": "hello" }));
    fx.entry("host_answer");
}

/// A Unix listener that answers each connection's first line with `{"ok":true}` and sends the
/// lines it got to a channel.
fn listener(path: &Path) -> Receiver<String> {
    let listener = UnixListener::bind(path).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut line).is_ok() {
                let _ = (&stream).write_all(b"{\"ok\":true}\n");
                let _ = tx.send(line.trim_end().to_string());
            }
        }
    });
    rx
}

#[test]
fn smoke_connect_socket_sends_and_records_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let lines = listener(&sock);
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "connect_socket": { "path": sock.to_string_lossy() } },
        { "connect_socket": { "path": sock.to_string_lossy(), "send": "custom line" } },
        { "connect_socket": { "path": dir.path().join("none.sock").to_string_lossy() } },
    ] }]));
    assert_eq!(
        value(&lines.recv_timeout(LIMIT).unwrap())["request"]["type"],
        "ping",
        "the default envelope"
    );
    assert_eq!(lines.recv_timeout(LIMIT).unwrap(), "custom line");
    let sockets = |fx: &Fx| {
        fx.report()
            .into_iter()
            .filter(|e| e["what"] == "socket")
            .collect::<Vec<_>>()
    };
    let deadline = Instant::now() + LIMIT;
    while sockets(&fx).len() < 3 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let s = sockets(&fx);
    assert_eq!(
        (
            s[0]["ok"].clone(),
            s[0]["answer"].clone(),
            s[0]["from"].clone()
        ),
        (json!(true), json!("{\"ok\":true}"), json!("fixture"))
    );
    assert_eq!(
        (s[2]["ok"].clone(), s[2]["os_error"].clone()),
        (json!(false), json!(2)),
        "no such socket: ENOENT"
    );
}

#[test]
fn smoke_dial_from_child_comes_from_a_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let lines = listener(&sock);
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "dial_from_child": { "path": sock.to_string_lossy() } } }]),
    );
    lines.recv_timeout(LIMIT).unwrap();
    let s = fx.wait_entry("socket", |e| e["from"] == "child");
    assert_eq!(s["ok"], true);
    assert_eq!(s["answer"], "{\"ok\":true}");
    assert_eq!(s["ppid"], fx.pid(), "its parent is the fixture");
    assert_ne!(s["pid"], fx.pid(), "a different process");
    assert_eq!(fx.wait_entry("child", |_| true)["exit"], 0);
}

#[test]
fn smoke_connect_write_exit_leaves_without_reading() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let lines = listener(&sock);
    let fx = Fx::start(
        json!([{ "on": "start", "do": { "connect_write_exit": { "path": sock.to_string_lossy(), "send": "bye" } } }]),
    );
    assert_eq!(lines.recv_timeout(LIMIT).unwrap(), "bye");
    let s = fx.wait_entry("socket", |e| e["from"] == "child");
    assert_eq!(s["ok"], true);
    assert!(s.get("answer").is_none());
    assert_eq!(s["ppid"], fx.pid());
}

#[test]
fn smoke_echo_env_shows_only_what_the_parent_gave() {
    let fx = Fx::start_env(
        json!([{ "on": "start", "do": "echo_env" }]),
        &[("FIXTURE_EXTRA", "yes")],
        true,
    );
    let env = fx.entry("env")["env"].clone();
    assert_eq!(env["FIXTURE_EXTRA"], "yes");
    assert_eq!(env["OXIDEPM_ADAPTER_PROTOCOLS"], "1");
    assert!(env.get("HOME").is_none(), "{env}");
}

#[test]
fn smoke_echo_fds() {
    let fx = Fx::start(json!([{ "on": "start", "do": "echo_fds" }]));
    let fds = fx.entry("fds")["fds"].as_array().unwrap().clone();
    let report = fx.report.to_string_lossy().into_owned();
    assert!(
        fds.iter().any(|f| f["fd"] == 0)
            && fds.iter().any(|f| f["fd"] == 1)
            && fds.iter().any(|f| f["fd"] == 2),
        "{fds:?}"
    );
    for f in fds.iter().filter(|f| f["fd"].as_u64().unwrap() > 2) {
        assert_eq!(
            f["target"],
            json!(report),
            "only the report file besides the standard streams: {fds:?}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn smoke_echo_limits_and_nnp() {
    let fx = Fx::start(json!([{ "on": "start", "do": ["echo_limits", "echo_nnp"] }]));
    let limits = fx.entry("limits")["limits"].clone();
    let mut mine = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut mine) };
    assert_eq!(
        limits["nofile"]["soft"], mine.rlim_cur,
        "inherited from the test"
    );
    for name in ["fsize", "core", "data", "as", "nproc", "cpu", "stack"] {
        assert!(limits.get(name).is_some(), "{name}");
    }
    let mine = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    assert_eq!(fx.entry("nnp")["no_new_privs"], mine);
}

#[test]
fn smoke_touch() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("touched");
    let missing = dir.path().join("no/such/dir/file");
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "touch": { "path": file.to_string_lossy() } },
        { "touch": { "path": missing.to_string_lossy() } },
    ] }]));
    assert_eq!(
        fx.wait_entry("touch", |e| e["ok"] == true)["path"],
        json!(file.to_string_lossy())
    );
    assert!(file.exists());
    assert_eq!(fx.wait_entry("touch", |e| e["ok"] == false)["os_error"], 2);
}

#[test]
fn smoke_inet_connect() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap().to_string();
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "inet_connect": { "addr": addr } },
        { "inet_connect": { "addr": "not an address" } },
    ] }]));
    assert_eq!(
        fx.wait_entry("inet_connect", |e| e["ok"] == true)["addr"],
        json!(addr)
    );
    assert_eq!(
        fx.wait_entry("inet_connect", |e| e["ok"] == false)["addr"],
        "not an address"
    );
    server.accept().unwrap();
}

#[test]
fn smoke_exec() {
    let truth = ["/bin/true", "/usr/bin/true"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a true(1)");
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "exec": { "path": truth } },
        { "exec": { "path": "/no/such/program" } },
    ] }]));
    let ran = fx.wait_entry("exec", |e| e["ok"] == true);
    assert_eq!(
        (ran["path"].clone(), ran["status"].clone()),
        (json!(truth), json!(0))
    );
    assert_eq!(fx.wait_entry("exec", |e| e["ok"] == false)["os_error"], 2);
}

#[test]
fn smoke_alloc_holds_touched_memory() {
    let fx = Fx::start(json!([{ "on": "start", "do": { "alloc": { "mb": 24 } } }]));
    assert_eq!(fx.wait_entry("alloc", |_| true)["ok"], true);
    let status = std::fs::read_to_string(format!("/proc/{}/status", fx.pid())).unwrap();
    let rss_kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .unwrap()
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .unwrap();
    assert!(rss_kb >= 24 * 1024, "resident {rss_kb} kB");
}

#[test]
fn smoke_busy_loop_uses_cpu() {
    let fx = Fx::start(json!([{ "on": "start", "do": { "busy_loop": { "ms": 200 } } }]));
    let done = fx.wait_entry("busy_loop", |e| e["state"] == "done");
    assert!(done["cpu_ms"].as_u64().unwrap() >= 200, "{done}");
}

#[test]
fn smoke_reply_cancelled_after_cancel() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": "reply_cancelled_after_cancel" }]));
    fx.send(&with_id(GOLDEN_PING, 5));
    fx.expect_quiet();
    fx.send(&notification("cancel", json!({ "id": 5 })));
    let e = fx.recv_error(5);
    assert_eq!(e.code, -32024);
    // a cancel for another id is not answered
    fx.send(&notification("cancel", json!({ "id": 6 })));
    fx.expect_quiet();
}

#[test]
fn smoke_exec_shell() {
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "exec_shell": { "cmd": "echo hi; echo oops >&2; exit 3" } },
    ] }]));
    let e = fx.entry("exec_shell");
    assert_eq!(
        (
            e["ok"].clone(),
            e["status"].clone(),
            e["stdout"].clone(),
            e["stderr"].clone()
        ),
        (json!(true), json!(3), json!("hi\n"), json!("oops\n"))
    );
}

#[test]
fn smoke_fork_bomb_is_flat_bounded_and_cleans_up() {
    let fx = Fx::start(json!([{ "on": "start", "do": { "fork_bomb": { "max": 6 } } }]));
    let e = fx.entry("fork_bomb");
    assert_eq!(e["forks"], 6);
    assert!(e.get("error").is_none());
    // none of the children is left
    let children = std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", fx.pid()))
        .unwrap_or_default();
    assert!(children.trim().is_empty(), "children left: {children}");
}

#[test]
fn smoke_fill_state_dir() {
    let mut fx = Fx::start(json!([
        { "on": "ping", "times": 1, "do": { "fill_state_dir": { "mb": 3 } } },
        { "on": "ping", "times": 1, "do": { "fill_state_dir": { "mb": 1, "dir": "/no/such/dir" } } },
    ]));
    // before `initialize` the fixture knows no state directory
    fx.send(&with_id(GOLDEN_PING, 1));
    assert!(fx.entry("fill_state_dir")["error"].as_str().is_some());
    fx.init();
    // second rule: an unwritable directory records the failure
    fx.send(&with_id(GOLDEN_PING, 2));
    let failed = fx.wait_entry("fill_state_dir", |e| e["dir"] == "/no/such/dir");
    assert_eq!(
        (failed["written_mb"].clone(), failed["os_error"].clone()),
        (json!(0), json!(2))
    );
}

#[test]
fn smoke_fill_state_dir_writes_into_the_state_dir_from_initialize() {
    let mut fx = Fx::start(json!([{ "on": "ping", "do": { "fill_state_dir": { "mb": 3 } } }]));
    fx.init();
    fx.send(&with_id(GOLDEN_PING, 2));
    let e = fx.entry("fill_state_dir");
    assert_eq!(e["written_mb"], 3);
    let size: u64 = (0..3)
        .map(|i| {
            std::fs::metadata(fx.path("state").join(format!("fill-{i}.bin")))
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(size, 3 * 1024 * 1024);
}

// ---- the behaviours added for the sandbox tests ----------------------------------

#[test]
fn smoke_alloc_children_spread_memory_over_a_tree_and_clean_up() {
    let fx =
        Fx::start(json!([{ "on": "start", "do": { "alloc_children": { "count": 2, "mb": 8 } } }]));
    let e = fx.entry("alloc_children");
    assert_eq!(
        (e["children"].clone(), e["mb"].clone()),
        (json!(2), json!(8)),
        "{e}"
    );
    let children = std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", fx.pid()))
        .unwrap_or_default();
    assert!(children.trim().is_empty(), "children left: {children}");
}

#[test]
fn smoke_read_file_and_list_dir_report_what_the_kernel_said() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("readable");
    std::fs::write(&file, b"hello").unwrap();
    let missing = dir.path().join("missing");
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "read_file": { "path": file.to_string_lossy() } },
        { "read_file": { "path": missing.to_string_lossy() } },
        { "list_dir": { "path": dir.path().to_string_lossy() } },
    ] }]));
    assert_eq!(
        fx.wait_entry("read_file", |e| e["ok"] == true)["path"],
        json!(file.to_string_lossy())
    );
    assert_eq!(
        fx.wait_entry("read_file", |e| e["ok"] == false)["os_error"],
        2
    );
    assert_eq!(fx.wait_entry("list_dir", |_| true)["ok"], true);
}

#[test]
fn smoke_inet_socket_and_io_uring_setup_are_tried() {
    let fx = Fx::start(json!([{ "on": "start", "do": ["inet_socket", "io_uring_setup"] }]));
    // outside a sandbox the socket works; whether io_uring does depends on the kernel, so only that it was tried
    assert_eq!(fx.wait_entry("inet_socket", |_| true)["ok"], true);
    let _ = fx.wait_entry("io_uring_setup", |_| true);
}

#[test]
fn smoke_unix_and_abstract_connect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("svc.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    let fx = Fx::start(json!([{ "on": "start", "do": [
        { "unix_connect": { "path": path.to_string_lossy() } },
        { "unix_connect": { "path": dir.path().join("none.sock").to_string_lossy() } },
        { "abstract_connect": { "name": "oxidepm-fixture-no-such-socket" } },
    ] }]));
    assert_eq!(
        fx.wait_entry("unix_connect", |e| e["ok"] == true)["path"],
        json!(path.to_string_lossy())
    );
    assert_eq!(
        fx.wait_entry("unix_connect", |e| e["ok"] == false)["os_error"],
        2
    );
    assert_eq!(fx.wait_entry("abstract_connect", |_| true)["ok"], false);
}

#[test]
fn smoke_echo_status_reports_its_own_process() {
    let fx = Fx::start(json!([{ "on": "start", "do": "echo_status" }]));
    let e = fx.entry("status");
    assert_eq!(e["pid"], fx.pid());
    assert_eq!(e["status_pid"], fx.pid().to_string());
    assert!(
        e["no_new_privs"].is_string() && e["seccomp"].is_string(),
        "{e}"
    );
}

#[test]
fn smoke_unix_socket_tcp_bind_and_execve() {
    let fx = Fx::start(json!([{ "on": "start", "do": [
        "unix_socket",
        { "tcp_bind": { "addr": "127.0.0.1:0" } },
        { "tcp_bind": { "addr": "not an address" } },
        { "execve": { "path": "/no/such/program" } },
    ] }]));
    assert_eq!(fx.wait_entry("unix_socket", |_| true)["ok"], true);
    assert_eq!(
        fx.wait_entry("tcp_bind", |e| e["ok"] == true)["addr"],
        "127.0.0.1:0"
    );
    assert_eq!(
        fx.wait_entry("tcp_bind", |e| e["ok"] == false)["addr"],
        "not an address"
    );
    // a refused execve returns, and says why
    assert_eq!(
        fx.wait_entry("execve", |e| e["state"] == "refused")["os_error"],
        2
    );
}

#[test]
fn smoke_signal_checks_a_process() {
    let me = std::process::id() as i32;
    let fx = Fx::start(
        json!([{ "on": "start", "do": [{ "signal": { "pid": me, "signal": 0 } }, { "signal": { "pid": 2147483000, "signal": 0 } }] }]),
    );
    assert_eq!(fx.wait_entry("signal", |e| e["ok"] == true)["pid"], me);
    assert_eq!(fx.wait_entry("signal", |e| e["ok"] == false)["os_error"], 3);
}
