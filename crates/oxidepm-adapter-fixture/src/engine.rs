//! The engine: reads frames from stdin, finds the rule for each, and carries out its actions.
//!
//! Everything the fixture does goes through [`Engine`]. The reader thread handles one frame at a
//! time. Actions run on it, except that `sleep` hands the rest of its list to a timer thread and
//! `flood_stdout`, `flood_stderr`, `giant_line` and `busy_loop` run on a thread of their own, so
//! the fixture keeps reading (it has to read a `cancel`, for one thing). `deaf` stops the reader;
//! when it comes from a timer thread, the reader stops after the next frame it was already
//! blocked on.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxidepm_adapter_proto::codes::{self, rpc};
use oxidepm_adapter_proto::frame::{from_params, to_params};
use oxidepm_adapter_proto::messages::{
    method, InitializeParams, InitializeResult, InvokeOutputParams, OutputStream,
};
use oxidepm_adapter_proto::{ErrorObject, Frame, FrameReader, Params, Read, SUPPORTED_PROTOCOLS};
use serde_json::{json, Map, Value};

use crate::child;
use crate::report::Report;
use crate::script::{Action, Cmd, On, Rule, Script};

/// The longest line the fixture accepts from the daemon.
const MAX_IN_LINE: usize = 1024 * 1024;
/// A hard cap on what one flood or one fill may write, so a typo cannot fill a disk.
const MAX_FLOOD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GIANT_LINE: u64 = 1 << 30;
/// The same for `fill_state_dir`: enough to pass the 64 MiB quota of an adapter.
const MAX_FILL_BYTES: u64 = 128 * 1024 * 1024;

/// The request being handled, when there is one.
#[derive(Clone)]
struct Call {
    id: u64,
    method: String,
    params: Params,
}

struct Counters {
    matched: u32,
    fired: u32,
}

#[derive(Default)]
struct State {
    state_dir: Option<String>,
    /// Requests the fixture made, by id.
    waiting: HashMap<u64, &'static str>,
    next_id: u64,
    /// Requests to answer with -32024 once a `cancel` for them arrives.
    cancellable: HashSet<u64>,
    stay_alive: bool,
    deaf: bool,
}

/// `SIGWINCH` seen since `watch_winch`.
static WINCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

extern "C" fn on_winch(_signal: libc::c_int) {
    WINCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

pub struct Engine {
    /// The session connection, when the daemon started the fixture for a session: the protocol
    /// runs over it, and standard input and output are the terminal.
    channel: std::sync::OnceLock<Mutex<std::os::unix::net::UnixStream>>,
    rules: Mutex<Vec<(Rule, Counters)>>,
    delays: Vec<u64>,
    report: Report,
    state: Mutex<State>,
    held: Mutex<Vec<Vec<u8>>>,
}

fn os_error(e: &std::io::Error) -> Value {
    json!({ "error": e.to_string(), "os_error": e.raw_os_error() })
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (base.as_object_mut(), extra) {
        a.extend(b);
    }
    base
}

fn cpu_ms(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

fn park_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

impl Engine {
    pub fn new(script: Script, report: Report) -> Arc<Engine> {
        let delays = script.delays();
        let rules = script
            .rules
            .into_iter()
            .map(|r| {
                (
                    r,
                    Counters {
                        matched: 0,
                        fired: 0,
                    },
                )
            })
            .collect();
        Arc::new(Engine {
            channel: std::sync::OnceLock::new(),
            rules: Mutex::new(rules),
            delays,
            report,
            state: Mutex::new(State {
                next_id: 1,
                ..State::default()
            }),
            held: Mutex::new(Vec::new()),
        })
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- output -------------------------------------------------------------------------

    /// Use `stream` for the protocol instead of standard input and output.
    pub fn use_channel(&self, stream: std::os::unix::net::UnixStream) {
        let _ = self.channel.set(Mutex::new(stream));
    }

    fn write_raw(&self, bytes: &[u8]) {
        if let Some(channel) = self.channel.get() {
            let mut stream = channel.lock().unwrap_or_else(|e| e.into_inner());
            let _ = stream.write_all(bytes);
            let _ = stream.flush();
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }

    fn send(&self, frame: &Frame) {
        self.report
            .record("frame", json!({ "dir": "out", "frame": frame.to_value() }));
        self.write_raw(&frame.encode());
    }

    fn exit(&self, code: i32) -> ! {
        self.report.record("exit", json!({ "code": code }));
        std::process::exit(code)
    }

    // ---- the main loop --------------------------------------------------------------------

    /// Run until stdin ends. Never returns.
    pub fn run(self: Arc<Self>) -> ! {
        self.report.record(
            "start",
            json!({ "args": std::env::args().collect::<Vec<_>>(), "ppid": unsafe { libc::getppid() } }),
        );
        for ms in self.delays.clone() {
            let engine = Arc::clone(&self);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                engine.fire(&On::AfterMs(ms), None);
            });
        }
        self.fire(&On::Start, None);

        let input: Box<dyn std::io::BufRead> = match self
            .channel
            .get()
            .map(|c| c.lock().unwrap_or_else(|e| e.into_inner()).try_clone())
        {
            Some(Ok(stream)) => Box::new(std::io::BufReader::new(stream)),
            Some(Err(e)) => {
                self.report
                    .record("channel_error", json!({ "error": e.to_string() }));
                self.exit(3)
            }
            None => Box::new(std::io::stdin().lock()),
        };
        let mut reader = FrameReader::new(input, MAX_IN_LINE);
        loop {
            if self.state().deaf {
                self.report.record("deaf", Value::Null);
                park_forever();
            }
            match reader.read_frame() {
                Ok(Read::Frame(frame)) => self.on_frame(frame),
                Ok(Read::Violation(e)) => self
                    .report
                    .record("violation", json!({ "reason": e.to_string() })),
                Ok(Read::Overflow { discarded }) => self
                    .report
                    .record("overflow", json!({ "discarded": discarded })),
                Ok(Read::Truncated { bytes }) => {
                    self.report.record("truncated", json!({ "bytes": bytes }))
                }
                Ok(Read::Eof) | Err(_) => break,
            }
        }
        self.report.record("stdin_eof", Value::Null);
        self.fire(&On::StdinEof, None);
        if self.state().stay_alive {
            park_forever();
        }
        self.exit(0)
    }

    fn on_frame(self: &Arc<Self>, frame: Frame) {
        self.report
            .record("frame", json!({ "dir": "in", "frame": frame.to_value() }));
        match frame {
            Frame::Request { id, method, params } => {
                if method == method::INITIALIZE {
                    if let Ok(p) = from_params::<InitializeParams>(&params) {
                        self.state().state_dir = Some(p.paths.state);
                    }
                }
                let call = Call {
                    id,
                    method: method.clone(),
                    params,
                };
                if !self.fire(&On::Method(method.clone()), Some(&call)) {
                    let mut actions = vec![Action::Reply { result: None }];
                    if method == self::method::SHUTDOWN {
                        actions.push(Action::ExitClean {});
                    }
                    self.run_actions(&actions, Some(&call));
                }
            }
            Frame::Notification { method, params } => {
                if method == self::method::CANCEL {
                    let id = params.get("id").and_then(Value::as_u64);
                    if let Some(id) = id {
                        if self.state().cancellable.remove(&id) {
                            self.send(&Frame::error(
                                id,
                                ErrorObject::new(rpc::CANCELLED, "cancelled"),
                            ));
                        }
                    }
                }
                let call = Call {
                    id: 0,
                    method: method.clone(),
                    params,
                };
                self.fire(&On::Method(method), Some(&call));
            }
            Frame::Response { id, .. } | Frame::Error { id, .. } => {
                let waiting = self.state().waiting.remove(&id);
                if let Some(request) = waiting {
                    self.report.record(
                        "host_answer",
                        json!({ "id": id, "request": request, "frame": frame.to_value() }),
                    );
                }
            }
        }
        if self.state().deaf {
            self.report.record("deaf", Value::Null);
            park_forever();
        }
    }

    // ---- rules --------------------------------------------------------------------------

    /// Run the first rule that applies to `on`. False when none did.
    fn fire(self: &Arc<Self>, on: &On, call: Option<&Call>) -> bool {
        let chosen = {
            let mut rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
            let mut found = None;
            for (index, (rule, counters)) in rules.iter_mut().enumerate() {
                if &rule.on != on {
                    continue;
                }
                if let Some(only) = &rule.only_in {
                    let here = std::env::current_exe()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    if !here.contains(only.as_str()) {
                        continue;
                    }
                }
                counters.matched += 1;
                if counters.matched <= rule.after || rule.times.is_some_and(|t| counters.fired >= t)
                {
                    continue;
                }
                counters.fired += 1;
                found = Some((index, rule.actions.clone()));
                break;
            }
            found
        };
        let Some((index, actions)) = chosen else {
            return false;
        };
        let names: Vec<String> = actions.iter().map(Action::name).collect();
        self.report.record(
            "rule",
            json!({ "index": index, "on": format!("{on:?}"), "do": names }),
        );
        self.run_actions(&actions, call);
        true
    }

    fn run_actions(self: &Arc<Self>, actions: &[Action], call: Option<&Call>) {
        for (i, action) in actions.iter().enumerate() {
            if let Action::Sleep { ms } = action {
                let (engine, rest, call, ms) = (
                    Arc::clone(self),
                    actions[i + 1..].to_vec(),
                    call.cloned(),
                    *ms,
                );
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(ms));
                    engine.run_actions(&rest, call.as_ref());
                });
                return;
            }
            self.do_action(action, call);
        }
    }

    // ---- answers ------------------------------------------------------------------------

    /// The well-behaved answer to a request.
    fn default_result(&self, call: &Call) -> Result<Params, ErrorObject> {
        match call.method.as_str() {
            method::INITIALIZE => {
                let p: InitializeParams = from_params(&call.params).map_err(|e| {
                    ErrorObject::new(rpc::INVALID_PARAMS, format!("bad initialize params: {e}"))
                })?;
                let protocol = p
                    .protocols
                    .iter()
                    .copied()
                    .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
                    .max();
                match protocol {
                    Some(protocol) => Ok(to_params(&InitializeResult {
                        protocol,
                        name: p.adapter.name,
                        version: p.adapter.version,
                    })
                    .unwrap_or_default()),
                    None => Err(ErrorObject {
                        code: rpc::UNSUPPORTED_PROTOCOL,
                        message: "none of the offered protocols is supported".into(),
                        data: Some(
                            json!({ "code": codes::E_ADAPTER_PROTOCOL, "supported": SUPPORTED_PROTOCOLS }),
                        ),
                    }),
                }
            }
            method::PING | method::SHUTDOWN => Ok(Params::new()),
            method::RUNTIME_PREPARE => {
                Ok(to_params(&json!({ "steps": [], "notes": [] })).unwrap_or_default())
            }
            method::COMMAND_INVOKE => Ok(to_params(&json!({ "exit_code": 0 })).unwrap_or_default()),
            method::RUNTIME_PLAN => Err(ErrorObject::new(
                rpc::INTERNAL_ERROR,
                "the fixture has no plan scripted",
            )),
            other => Err(ErrorObject::new(
                rpc::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }

    fn default_frame(&self, call: &Call, id: u64) -> Frame {
        match self.default_result(call) {
            Ok(result) => Frame::Response { id, result },
            Err(error) => Frame::Error { id, error },
        }
    }

    /// The request this action answers, or a note that there is none.
    fn need_call<'a>(&self, action: &Action, call: Option<&'a Call>) -> Option<&'a Call> {
        match call {
            Some(c) if c.id != 0 => Some(c),
            _ => {
                self.report.record(
                    "skipped",
                    json!({ "do": action.name(), "why": "no request to answer" }),
                );
                None
            }
        }
    }

    fn request_to_daemon(&self, name: &'static str, params: Params) {
        let id = {
            let mut state = self.state();
            let id = state.next_id;
            state.next_id += 1;
            state.waiting.insert(id, name);
            id
        };
        self.send(&Frame::Request {
            id,
            method: name.to_string(),
            params,
        });
    }

    // ---- actions ------------------------------------------------------------------------

    fn do_action(self: &Arc<Self>, action: &Action, call: Option<&Call>) {
        match action {
            Action::Sleep { .. } => {} // handled by run_actions
            Action::Reply { result } => {
                if let Some(call) = self.need_call(action, call) {
                    let frame = match result {
                        Some(result) => Frame::Response {
                            id: call.id,
                            result: result.clone(),
                        },
                        None => self.default_frame(call, call.id),
                    };
                    self.send(&frame);
                }
            }
            Action::Error {
                code,
                message,
                name,
            } => {
                if let Some(call) = self.need_call(action, call) {
                    let name = name.as_deref().or_else(|| codes::name_for_rpc_code(*code));
                    let error = match name {
                        Some(name) => ErrorObject::with_name(*code, name, message.clone()),
                        None => ErrorObject::new(*code, message.clone()),
                    };
                    self.send(&Frame::error(call.id, error));
                }
            }
            Action::Hang {} => self.report.record("hang", Value::Null),
            Action::Deaf {} => self.state().deaf = true,
            Action::Crash { code } => self.exit(*code),
            Action::Abort {} => {
                self.report.record("abort", Value::Null);
                let none = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                unsafe {
                    libc::setrlimit(libc::RLIMIT_CORE, &none);
                    libc::abort()
                }
            }
            Action::ExitClean {} => self.exit(0),
            Action::IgnoreSigterm {} => {
                unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
                self.report.record("ignore_sigterm", Value::Null);
            }
            Action::IgnoreShutdown {} => {
                self.state().stay_alive = true;
                self.report.record("ignore_shutdown", Value::Null);
            }
            Action::SpawnGrandchild { cmd } => self.spawn_grandchild(cmd, false),
            Action::SpawnSetsidGrandchild { cmd } => self.spawn_grandchild(cmd, true),
            Action::FloodStdout {
                fps,
                bytes,
                count,
                junk,
            } => {
                let (engine, fps, bytes, count, junk) =
                    (Arc::clone(self), *fps, *bytes, *count, *junk);
                std::thread::spawn(move || engine.flood_stdout(fps, bytes, count, junk));
            }
            Action::FloodStderr { bps, secs } => {
                let (engine, bps, secs) = (Arc::clone(self), *bps, secs.unwrap_or(1));
                std::thread::spawn(move || engine.flood_stderr(bps, secs));
            }
            Action::InvokeOutput {
                stream,
                text,
                count,
                bytes,
                every_ms,
            } => {
                if let Some(call) = self.need_call(action, call) {
                    let stream = match stream.as_deref() {
                        Some("stderr") => OutputStream::Stderr,
                        Some("status") => OutputStream::Status,
                        _ => OutputStream::Stdout,
                    };
                    for n in 0..count.unwrap_or(1).clamp(1, 100_000) {
                        let text = match bytes {
                            Some(b) => "x".repeat(*b),
                            None => text.clone().unwrap_or_else(|| "progress".to_string()),
                        };
                        if let Ok(frame) = Frame::notification(
                            method::INVOKE_OUTPUT,
                            &InvokeOutputParams {
                                id: call.id,
                                stream,
                                text,
                            },
                        ) {
                            self.send(&frame);
                        }
                        if let (Some(ms), true) = (every_ms, n + 1 < count.unwrap_or(1)) {
                            std::thread::sleep(Duration::from_millis(*ms));
                        }
                    }
                }
            }
            Action::TerminalReport {} => self.terminal_report(),
            Action::TerminalEcho { lines } => self.terminal_echo(lines.unwrap_or(1)),
            Action::TerminalRaw {} => {
                // SAFETY: cfmakeraw and tcsetattr work on a termios we own, for descriptor 0.
                let done = unsafe {
                    let mut settings: libc::termios = std::mem::zeroed();
                    libc::tcgetattr(0, &mut settings) == 0 && {
                        libc::cfmakeraw(&mut settings);
                        libc::tcsetattr(0, libc::TCSANOW, &settings) == 0
                    }
                };
                self.report.record("terminal_raw", json!({ "ok": done }));
            }
            Action::WatchWinch {} => {
                // SAFETY: the handler only increments an atomic counter, which is async-signal-safe.
                unsafe {
                    libc::signal(
                        libc::SIGWINCH,
                        on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t,
                    )
                };
                self.report.record("watch_winch", Value::Null);
            }
            Action::ReportWinch {} => {
                self.report.record(
                    "winch",
                    json!({ "count": WINCH.load(std::sync::atomic::Ordering::SeqCst) }),
                );
            }
            Action::StderrText { text } => {
                use std::io::Write;
                let mut err = std::io::stderr();
                let _ = err.write_all(text.as_bytes());
                let _ = err.write_all(b"\n");
                let _ = err.flush();
            }
            Action::GiantLine { bytes } => {
                let (engine, bytes) = (Arc::clone(self), *bytes);
                std::thread::spawn(move || engine.giant_line(bytes));
            }
            Action::Garbage { text } => self.write_raw(format!("{text}\n").as_bytes()),
            Action::InvalidUtf8 {} => {
                self.write_raw(b"{\"jsonrpc\":\"2.0\",\"method\":\"\xff\xfe\"}\n")
            }
            Action::Nul {} => self.write_raw(b"{\"jsonrpc\":\"2.0\",\"method\":\"x\0y\"}\n"),
            Action::ArrayFrame {} => self
                .write_raw(b"[{\"jsonrpc\":\"2.0\",\"method\":\"fixture.batch\",\"params\":{}}]\n"),
            Action::PartialLineThenEof {} => {
                self.write_raw(b"{\"jsonrpc\":\"2.0\",\"met");
                // The pipe's write end is replaced by /dev/null, so the reader sees end of file
                // and a later open() cannot be handed descriptor 1.
                unsafe {
                    let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                    if null >= 0 {
                        libc::dup2(null, 1);
                        if null != 1 {
                            libc::close(null);
                        }
                    }
                }
                self.report.record("stdout_closed", Value::Null);
            }
            Action::WrongId { id } => {
                if let Some(call) = self.need_call(action, call) {
                    let id = id.unwrap_or(call.id + 1_000_000);
                    self.send(&self.default_frame(call, id));
                }
            }
            Action::DuplicateReply {} => {
                if let Some(call) = self.need_call(action, call) {
                    let frame = self.default_frame(call, call.id);
                    self.send(&frame);
                    self.send(&frame);
                }
            }
            Action::UnsolicitedResponse { id } => self.send(&Frame::Response {
                id: *id,
                result: Params::new(),
            }),
            Action::LieName { name } => {
                let name = name.clone().unwrap_or_else(|| "not-the-adapter".into());
                self.lie(action, call, |r| {
                    r.insert("name".into(), json!(name));
                });
            }
            Action::LieVersion { version } => {
                let version = version.clone().unwrap_or_else(|| "9.9.9".into());
                self.lie(action, call, |r| {
                    r.insert("version".into(), json!(version));
                });
            }
            Action::LieProtocol { protocol } => {
                let protocol = protocol.unwrap_or(99);
                self.lie(action, call, |r| {
                    r.insert("protocol".into(), json!(protocol));
                });
            }
            Action::HostRequest {
                request,
                dry_run,
                end_user,
                repeat,
            } => {
                let mut params = Map::new();
                params.insert("request".into(), request.clone());
                if *dry_run {
                    params.insert("dry_run".into(), json!(true));
                }
                if let Some(end_user) = end_user {
                    params.insert("end_user".into(), json!(end_user));
                }
                for _ in 0..repeat.unwrap_or(1).max(1) {
                    self.request_to_daemon("host.request", params.clone());
                }
            }
            Action::ForgeActor { actor, request } => {
                let mut request = request.clone().unwrap_or_else(|| json!({ "type": "ping" }));
                let forged = json!({ "actor": actor, "reveal": true, "dry_run": false });
                request = merge(request, forged.clone());
                let mut params = Map::new();
                params.insert("request".into(), request);
                if let Value::Object(f) = forged {
                    params.extend(f);
                }
                self.request_to_daemon("host.request", params);
            }
            Action::ConnectSocket { path, send } => child::socket_exchange(
                &self.report,
                "fixture",
                path,
                send.as_deref().unwrap_or(child::DEFAULT_SEND),
                true,
            ),
            Action::DialFromChild { path, send } => child::run_child(
                &self.report,
                "dial",
                path,
                send.as_deref().unwrap_or(child::DEFAULT_SEND),
            ),
            Action::ConnectWriteExit { path, send } => child::run_child(
                &self.report,
                "write_exit",
                path,
                send.as_deref().unwrap_or(child::DEFAULT_SEND),
            ),
            Action::EchoEnv {} => {
                let env: Map<String, Value> = std::env::vars_os()
                    .map(|(k, v)| (k.to_string_lossy().into_owned(), json!(v.to_string_lossy())))
                    .collect();
                self.report.record("env", json!({ "env": env }));
            }
            Action::EchoFds {} => self.echo_fds(),
            Action::EchoLimits {} => self.echo_limits(),
            Action::EchoNnp {} => {
                // SAFETY: a read of a flag of the calling thread.
                let value = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
                self.report.record("nnp", json!({ "no_new_privs": value }));
            }
            Action::Touch { path } => {
                let result = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(path)
                    .and_then(|mut f| f.write_all(b"fixture\n"));
                self.report
                    .record("touch", file_outcome(json!({ "path": path }), result));
            }
            Action::InetConnect { addr } => {
                use std::net::{TcpStream, ToSocketAddrs};
                let result = addr
                    .to_socket_addrs()
                    .and_then(|mut it| it.next().ok_or_else(|| std::io::Error::other("no address")))
                    .and_then(|a| {
                        TcpStream::connect_timeout(&a, Duration::from_secs(2)).map(|_| ())
                    });
                self.report.record(
                    "inet_connect",
                    file_outcome(json!({ "addr": addr }), result),
                );
            }
            Action::Exec { path } => {
                let result = Command::new(path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let entry = json!({ "path": path });
                self.report.record(
                    "exec",
                    match result {
                        Ok(status) => merge(entry, json!({ "ok": true, "status": status.code(), "signal": std::os::unix::process::ExitStatusExt::signal(&status) })),
                        Err(e) => merge(merge(entry, json!({ "ok": false })), os_error(&e)),
                    },
                );
            }
            Action::Alloc { mb } => self.alloc(*mb),
            Action::BusyLoop { ms } => {
                let (engine, ms) = (Arc::clone(self), *ms);
                std::thread::spawn(move || engine.busy_loop(ms));
            }
            Action::AuditNote { text } => {
                let mut params = Map::new();
                params.insert("text".into(), json!(text));
                self.request_to_daemon("host.audit_note", params);
            }
            Action::ReplyCancelledAfterCancel {} => {
                if let Some(call) = self.need_call(action, call) {
                    self.state().cancellable.insert(call.id);
                    self.report
                        .record("reply_cancelled_after_cancel", json!({ "id": call.id }));
                }
            }
            Action::ExecShell { cmd } => {
                let result = Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .stdin(Stdio::null())
                    .output();
                let entry = json!({ "cmd": cmd });
                self.report.record(
                    "exec_shell",
                    match result {
                        Ok(out) => merge(
                            entry,
                            json!({ "ok": true, "status": out.status.code(),
                                    "stdout": clip(&out.stdout), "stderr": clip(&out.stderr) }),
                        ),
                        Err(e) => merge(merge(entry, json!({ "ok": false })), os_error(&e)),
                    },
                );
            }
            Action::ForkBomb { max, hold_ms } => {
                self.fork_bomb(max.unwrap_or(1000).min(4096), hold_ms.unwrap_or(0))
            }
            Action::FillStateDir { mb, dir } => self.fill_dir(*mb, dir.clone()),
            Action::AllocChildren { count, mb, hold_ms } => {
                self.alloc_children(*count, *mb, hold_ms.unwrap_or(0))
            }
            Action::ReadFile { path } => {
                let result = std::fs::File::open(path).and_then(|mut f| {
                    let mut buf = [0u8; 64];
                    std::io::Read::read(&mut f, &mut buf)
                });
                self.report.record(
                    "read_file",
                    file_outcome(json!({ "path": path }), result.map(|_| ())),
                );
            }
            Action::ListDir { path } => {
                let result = std::fs::read_dir(path).map(|d| d.count());
                self.report.record(
                    "list_dir",
                    file_outcome(json!({ "path": path }), result.map(|_| ())),
                );
            }
            Action::InetSocket {} => {
                // SAFETY: socket(2) takes no pointers; the descriptor is closed at once.
                let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
                let result = if fd < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    unsafe { libc::close(fd) };
                    Ok(())
                };
                self.report
                    .record("inet_socket", file_outcome(json!({}), result));
            }
            Action::IoUringSetup {} => {
                let mut params = [0u8; 120];
                // SAFETY: io_uring_setup writes at most the 120 bytes of io_uring_params.
                let fd =
                    unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) };
                let result = if fd < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    unsafe { libc::close(fd as i32) };
                    Ok(())
                };
                self.report
                    .record("io_uring_setup", file_outcome(json!({}), result));
            }
            Action::UnixConnect { path } => {
                let result = std::os::unix::net::UnixStream::connect(path).map(|_| ());
                self.report.record(
                    "unix_connect",
                    file_outcome(json!({ "path": path }), result),
                );
            }
            Action::AbstractConnect { name } => {
                let result = {
                    use std::os::linux::net::SocketAddrExt;
                    std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
                        .and_then(|addr| std::os::unix::net::UnixStream::connect_addr(&addr))
                        .map(|_| ())
                };
                self.report.record(
                    "abstract_connect",
                    file_outcome(json!({ "name": name }), result),
                );
            }
            Action::Signal { pid, signal } => {
                // SAFETY: kill(2) takes two integers.
                let result = if unsafe { libc::kill(*pid, *signal) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                };
                self.report.record(
                    "signal",
                    file_outcome(json!({ "pid": pid, "signal": signal }), result),
                );
            }
            Action::UnixSocket {} => {
                // SAFETY: socket(2) takes no pointers; the descriptor is closed at once.
                let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
                let result = if fd < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    unsafe { libc::close(fd) };
                    Ok(())
                };
                self.report
                    .record("unix_socket", file_outcome(json!({}), result));
            }
            Action::Execve { path } => {
                use std::ffi::CString;
                if let Ok(program) = CString::new(path.as_str()) {
                    let argv = [program.as_ptr(), std::ptr::null()];
                    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                    self.report
                        .record("execve", json!({ "path": path, "state": "trying" }));
                    // SAFETY: both arrays are NUL-terminated lists of valid C strings that outlive the call.
                    unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
                    self.report.record(
                        "execve",
                        merge(
                            json!({ "path": path, "state": "refused" }),
                            os_error(&std::io::Error::last_os_error()),
                        ),
                    );
                }
            }
            Action::TcpBind { addr } => {
                let result = std::net::TcpListener::bind(addr.as_str()).map(|_| ());
                self.report
                    .record("tcp_bind", file_outcome(json!({ "addr": addr }), result));
            }
            Action::EchoStatus {} => {
                let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
                let field = |name: &str| {
                    status
                        .lines()
                        .find_map(|l| l.strip_prefix(name))
                        .map(|v| v.trim().to_string())
                };
                self.report.record(
                    "status",
                    json!({ "pid": std::process::id(), "status_pid": field("Pid:"), "no_new_privs": field("NoNewPrivs:"), "seccomp": field("Seccomp:") }),
                );
            }
        }
    }

    fn lie(&self, action: &Action, call: Option<&Call>, change: impl FnOnce(&mut Params)) {
        let Some(call) = self.need_call(action, call) else {
            return;
        };
        match self.default_result(call) {
            Ok(mut result) => {
                change(&mut result);
                self.send(&Frame::Response {
                    id: call.id,
                    result,
                });
            }
            Err(error) => self.send(&Frame::error(call.id, error)),
        }
    }

    fn spawn_grandchild(&self, cmd: &Cmd, setsid: bool) {
        let argv = cmd.argv();
        let Some((program, args)) = argv.split_first() else {
            self.report
                .record("grandchild", json!({ "error": "empty command" }));
            return;
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if setsid {
            // SAFETY: setsid is async-signal-safe and touches no memory.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        let entry = json!({ "argv": argv, "setsid": setsid });
        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
                self.report.record("grandchild", merge(entry, json!({ "child_pid": pid, "pgid": pgid, "own_pgid": unsafe { libc::getpgrp() } })));
            }
            Err(e) => self.report.record("grandchild", merge(entry, os_error(&e))),
        }
    }

    fn flood_stdout(&self, fps: u32, bytes: usize, count: Option<u64>, junk: bool) {
        let count = count.unwrap_or(fps as u64).clamp(1, 1_000_000);
        let frame = |n: u64| -> Vec<u8> {
            if junk {
                let mut line = vec![b'x'; bytes.max(1)];
                line.push(b'\n');
                return line;
            }
            let head = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"fixture.flood\",\"params\":{{\"n\":{n},\"pad\":\"");
            let tail = "\"}}\n";
            let pad = bytes.saturating_sub(head.len() + tail.len());
            let mut line = head.into_bytes();
            line.extend(std::iter::repeat_n(b'x', pad));
            line.extend_from_slice(tail.as_bytes());
            line
        };
        let interval = (fps > 0).then(|| Duration::from_secs_f64(1.0 / fps as f64));
        let begun = Instant::now();
        let (mut sent, mut total) = (0u64, 0u64);
        for n in 0..count {
            let line = frame(n);
            if total + line.len() as u64 > MAX_FLOOD_BYTES {
                break;
            }
            self.write_raw(&line);
            sent += 1;
            total += line.len() as u64;
            if let Some(interval) = interval {
                let due = begun + interval * (n as u32 + 1);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        }
        self.report.record(
            "flood",
            json!({ "stream": "stdout", "junk": junk, "frames": sent, "bytes": total }),
        );
    }

    fn flood_stderr(&self, bps: u64, secs: u64) {
        let ticks = secs.clamp(1, 600) * 10;
        let per_tick = (bps / 10).max(1).min(MAX_FLOOD_BYTES / ticks.max(1));
        let line = [&[b'e'; 99][..], b"\n"].concat();
        let mut total = 0u64;
        for _ in 0..ticks {
            let mut chunk = Vec::with_capacity(per_tick as usize + 100);
            while (chunk.len() as u64) < per_tick {
                chunk.extend_from_slice(&line);
            }
            let _ = std::io::stderr().write_all(&chunk);
            total += chunk.len() as u64;
            std::thread::sleep(Duration::from_millis(100));
        }
        self.report
            .record("flood", json!({ "stream": "stderr", "bytes": total }));
    }

    fn giant_line(&self, bytes: u64) {
        let bytes = bytes.min(MAX_GIANT_LINE);
        let head = b"{\"jsonrpc\":\"2.0\",\"method\":\"fixture.giant\",\"params\":{\"pad\":\"";
        let tail = b"\"}}";
        let (head, tail): (&[u8], &[u8]) = if bytes as usize >= head.len() + tail.len() {
            (head, tail)
        } else {
            (b"", b"")
        };
        let mut pad = bytes - (head.len() + tail.len()) as u64;
        let chunk = vec![b'a'; 64 * 1024];
        let mut out = std::io::stdout().lock();
        let mut ok = out.write_all(head).is_ok();
        while ok && pad > 0 {
            let n = pad.min(chunk.len() as u64) as usize;
            ok = out.write_all(&chunk[..n]).is_ok();
            pad -= n as u64;
        }
        ok = ok
            && out.write_all(tail).is_ok()
            && out.write_all(b"\n").is_ok()
            && out.flush().is_ok();
        drop(out);
        self.report
            .record("giant_line", json!({ "bytes": bytes, "written": ok }));
    }

    fn terminal_report(&self) {
        // SAFETY: plain queries on descriptors and the process; nothing is written through pointers
        // except the window size, which is a local struct.
        let (in_tty, out_tty, group, session, foreground, size, dev_tty) = unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            let got = libc::ioctl(1, libc::TIOCGWINSZ, &mut ws);
            let tty = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            if tty >= 0 {
                libc::close(tty);
            }
            (
                libc::isatty(0) == 1,
                libc::isatty(1) == 1,
                libc::getpgrp(),
                libc::getsid(0),
                libc::tcgetpgrp(0),
                (got == 0).then_some((ws.ws_row, ws.ws_col)),
                tty >= 0,
            )
        };
        self.report.record(
            "terminal",
            json!({
                "stdin_tty": in_tty,
                "stdout_tty": out_tty,
                "pgrp": group,
                "sid": session,
                "foreground_pgrp": foreground,
                "rows_cols": size,
                "dev_tty_openable": dev_tty,
                "session_socket_env": std::env::var_os("OXIDEPM_SESSION_SOCKET").is_some(),
            }),
        );
    }

    fn terminal_echo(&self, lines: u32) {
        use std::io::BufRead as _;
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        for _ in 0..lines {
            let mut line = String::new();
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                self.report.record("terminal_eof", Value::Null);
                return;
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            self.report.record("terminal_line", json!({ "line": line }));
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "echo:{line}");
            let _ = out.flush();
        }
    }

    fn echo_fds(&self) {
        let mut fds = Vec::new();
        if let Ok(dir) = std::fs::read_dir("/proc/self/fd") {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let target = std::fs::read_link(entry.path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // the descriptor of the directory being read
                if target.starts_with("/proc/") && target.ends_with("/fd") {
                    continue;
                }
                if let Ok(fd) = name.parse::<u32>() {
                    fds.push(json!({ "fd": fd, "target": target }));
                }
            }
        }
        fds.sort_by_key(|f| f["fd"].as_u64());
        self.report.record("fds", json!({ "fds": fds }));
    }

    fn echo_limits(&self) {
        let mut limits = Map::new();
        for (name, resource) in [
            ("nofile", libc::RLIMIT_NOFILE),
            ("fsize", libc::RLIMIT_FSIZE),
            ("core", libc::RLIMIT_CORE),
            ("data", libc::RLIMIT_DATA),
            ("as", libc::RLIMIT_AS),
            ("nproc", libc::RLIMIT_NPROC),
            ("cpu", libc::RLIMIT_CPU),
            ("stack", libc::RLIMIT_STACK),
        ] {
            let mut r = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if unsafe { libc::getrlimit(resource, &mut r) } == 0 {
                let value = |v: libc::rlim_t| {
                    if v == libc::RLIM_INFINITY {
                        Value::Null
                    } else {
                        json!(v)
                    }
                };
                limits.insert(
                    name.into(),
                    json!({ "soft": value(r.rlim_cur), "hard": value(r.rlim_max) }),
                );
            }
        }
        // SAFETY: a read of this process's niceness; -1 is also a legal value, so errno is cleared first.
        let nice = unsafe {
            clear_errno();
            libc::getpriority(libc::PRIO_PROCESS, 0)
        };
        let oom = std::fs::read_to_string("/proc/self/oom_score_adj")
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok());
        self.report.record(
            "limits",
            json!({ "limits": limits, "nice": nice, "oom_score_adj": oom }),
        );
    }

    fn alloc(&self, mb: u64) {
        let size = (mb as usize).saturating_mul(1024 * 1024);
        let mut block: Vec<u8> = Vec::new();
        if let Err(e) = block.try_reserve_exact(size) {
            self.report.record(
                "alloc",
                json!({ "mb": mb, "ok": false, "error": e.to_string() }),
            );
            return;
        }
        block.resize(size, 0);
        for i in (0..size).step_by(4096) {
            block[i] = 1; // touch every page
        }
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(block);
        self.report.record("alloc", json!({ "mb": mb, "ok": true }));
    }

    /// Spin until this thread has used `ms` milliseconds of CPU time (so the CPU use does not
    /// depend on how busy the machine is), or for ever.
    fn busy_loop(&self, ms: Option<u64>) {
        self.report
            .record("busy_loop", json!({ "state": "started" }));
        let mut x = 0u64;
        loop {
            for _ in 0..100_000 {
                x = std::hint::black_box(
                    x.wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407),
                );
            }
            if ms.is_some_and(|ms| cpu_ms(libc::CLOCK_THREAD_CPUTIME_ID) >= ms) {
                break;
            }
        }
        self.report.record(
            "busy_loop",
            json!({ "state": "done", "cpu_ms": cpu_ms(libc::CLOCK_THREAD_CPUTIME_ID) }),
        );
    }

    fn fork_bomb(&self, max: u32, hold_ms: u64) {
        let mut children: Vec<libc::pid_t> = Vec::new();
        let mut failure = None;
        for _ in 0..max {
            // SAFETY: the child calls only async-signal-safe functions (alarm, pause, _exit).
            match unsafe { libc::fork() } {
                0 => unsafe {
                    libc::alarm(60);
                    libc::pause();
                    libc::_exit(0)
                },
                -1 => {
                    failure = Some(std::io::Error::last_os_error());
                    break;
                }
                pid => children.push(pid),
            }
        }
        let count = children.len();
        std::thread::sleep(Duration::from_millis(hold_ms));
        for pid in children {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
        let entry = json!({ "forks": count, "max": max });
        self.report.record(
            "fork_bomb",
            match failure {
                Some(e) => merge(entry, os_error(&e)),
                None => entry,
            },
        );
    }

    fn alloc_children(&self, count: u32, mb: u64, hold_ms: u64) {
        let size = (mb as usize).saturating_mul(1024 * 1024);
        let mut children: Vec<libc::pid_t> = Vec::new();
        for _ in 0..count.min(64) {
            // SAFETY: the child calls only async-signal-safe functions (mmap, alarm, pause, _exit)
            // and touches memory it mapped itself.
            match unsafe { libc::fork() } {
                0 => unsafe {
                    let p = libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    if p != libc::MAP_FAILED {
                        let bytes = p as *mut u8;
                        let mut i = 0;
                        while i < size {
                            *bytes.add(i) = 1;
                            i += 4096;
                        }
                    }
                    libc::alarm(60);
                    libc::pause();
                    libc::_exit(0)
                },
                -1 => break,
                pid => children.push(pid),
            }
        }
        let started = children.len();
        std::thread::sleep(Duration::from_millis(hold_ms));
        for pid in children {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
        self.report
            .record("alloc_children", json!({ "children": started, "mb": mb }));
    }

    fn fill_dir(&self, mb: u64, dir: Option<String>) {
        let dir = dir.or_else(|| self.state().state_dir.clone());
        let Some(dir) = dir else {
            self.report.record(
                "fill_state_dir",
                json!({ "written_mb": 0, "error": "no state directory known" }),
            );
            return;
        };
        let block = vec![0x5au8; 1024 * 1024];
        let mut written = 0u64;
        let mut failure = None;
        for i in 0..mb.min(MAX_FILL_BYTES / (1024 * 1024)) {
            let path = std::path::Path::new(&dir).join(format!("fill-{i}.bin"));
            match std::fs::write(&path, &block) {
                Ok(()) => written += 1,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let entry = json!({ "dir": dir, "written_mb": written });
        self.report.record(
            "fill_state_dir",
            match failure {
                Some(e) => merge(entry, os_error(&e)),
                None => entry,
            },
        );
    }
}

fn clip(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]).into_owned()
}

fn file_outcome(entry: Value, result: std::io::Result<()>) -> Value {
    match result {
        Ok(()) => merge(entry, json!({ "ok": true })),
        Err(e) => merge(merge(entry, json!({ "ok": false })), os_error(&e)),
    }
}

/// Set errno to 0 for the calling thread, so a call whose legal result is -1 can be checked.
///
/// # Safety
/// Writes the thread's errno slot; always valid to do.
unsafe fn clear_errno() {
    *libc::__errno_location() = 0;
}
