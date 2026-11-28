//! Sessions: a verb that owns the terminal. The real `oxidepmd` opens a session on
//! request, starts the tool adapter itself on the terminal the client hands over, and speaks the
//! adapter protocol with it over a socket made for that one session.
//!
//! The clients here are the library's `SessionClient`, which is what the CLI uses, and a
//! pseudo-terminal pair for the terminal. Tests marked [os] depend on the kernel.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::adapter_kit::*;
use common::daemon::DaemonProc;
use common::fixture::pid_alive;
use oxidepm_ipc::session::{
    ServerLine, SessionClient, SessionControl, SessionEvents, SessionOffer,
};
use serde_json::{json, Value};

const NAME: &str = "monitor";

/// One verb entry of a manifest.
fn verb(name: &str, mode: &str, tier: &str) -> Value {
    json!({ "name": name, "summary": format!("The {name} verb."), "tier": tier, "mode": mode, "writes": false, "dry_run": false, "max_runtime_secs": 60 })
}

/// A pseudo-terminal: the adapter gets the slave, the test reads and types on the master.
struct Pty {
    master: std::fs::File,
    slave: OwnedFd,
}

impl Pty {
    fn new() -> Pty {
        let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
        // SAFETY: openpty fills the two descriptors; the null pointers ask for no name, terminal
        // settings or window size.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both descriptors were just made for us.
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                OwnedFd::from_raw_fd(slave),
            )
        };
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        Pty { master, slave }
    }

    /// Everything the adapter has written so far, waiting up to `secs` for `needle` to show.
    fn read_until(&mut self, needle: &str, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut seen = Vec::new();
        loop {
            let mut buf = [0u8; 4096];
            match self.master.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => seen.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                // the slave side is closed once the adapter and our copy are gone
                Err(_) => break,
            }
            let text = String::from_utf8_lossy(&seen).into_owned();
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    fn type_line(&mut self, text: &str) {
        self.master
            .write_all(format!("{text}\n").as_bytes())
            .unwrap();
    }
}

struct Rig {
    kit: Kit,
    daemon: DaemonProc,
    project: PathBuf,
    baseline: usize,
}

/// What a test may change about the adapter it installs.
struct Setup {
    script: Value,
    verbs: Vec<Value>,
    perms: Value,
    manifest: Vec<(&'static str, Value)>,
    env: Vec<(&'static str, &'static str)>,
    /// The adapter may start programs and reach the daemon's home (and so its socket).
    reaches_daemon: bool,
}

impl Setup {
    fn new(script: Value) -> Setup {
        Setup {
            script,
            verbs: vec![
                verb("watch", "session", "safe"),
                verb("look", "relayed", "safe"),
            ],
            perms: json!({}),
            manifest: Vec::new(),
            env: Vec::new(),
            reaches_daemon: false,
        }
    }
    fn perms(mut self, perms: Value) -> Setup {
        self.perms = perms;
        self
    }
    fn manifest(mut self, key: &'static str, value: Value) -> Setup {
        self.manifest.push((key, value));
        self
    }
    fn reaching_the_daemon(mut self) -> Setup {
        self.reaches_daemon = true;
        self
    }
    fn env(mut self, key: &'static str, value: &'static str) -> Setup {
        self.env.push((key, value));
        self
    }
}

impl Rig {
    fn new(setup: Setup) -> Rig {
        let kit = Kit::new();
        let io = kit.io_dir();
        let io_text = io.display().to_string();
        let mut p = json!({
            "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
            "fs": { "read": ["$STATE", io_text.clone(), "$CWD"], "write": ["$STATE", io_text, "$CWD"] }
        });
        for (k, v) in setup.perms.as_object().unwrap() {
            p[k.as_str()] = v.clone();
        }
        if setup.reaches_daemon {
            // a write path is how Landlock lets a process connect to a pathname socket
            p["fs"]["write"]
                .as_array_mut()
                .unwrap()
                .push(json!(kit.home.display().to_string()));
            p["spawn"] = json!({ "mode": "any" });
        }
        let mut pkg = Pkg::tool(NAME, "0.1.0")
            .perms(p)
            .with("provides", json!({ "verbs": setup.verbs }));
        for (key, value) in setup.manifest {
            pkg = pkg.with(key, value);
        }
        let pkg = pkg.package();
        kit.publish(&[&pkg], &IndexOpts::default());
        kit.write_script(NAME, setup.script);
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
        all.extend_from_slice(&setup.env);
        let daemon = kit.daemon_with(&all);
        daemon.wait_for_boot();
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

    fn starts(&self) -> usize {
        self.report()
            .iter()
            .filter(|r| r["what"] == "start")
            .count()
    }

    fn started_since(&self) -> usize {
        self.starts() - self.baseline
    }

    fn sessions_dir(&self) -> PathBuf {
        self.kit.home.join("adapters").join("sessions")
    }

    /// Ask for a session on the main socket, as `claim` if given.
    fn open_as(&self, claim: Option<&str>, verb: &str) -> Value {
        let mut envelope = json!({
            "protocol": 2,
            "request_id": "sess-1",
            "request": { "type": "adapter", "op": "session_open", "verb": verb, "argv": ["a"], "cwd": self.project.display().to_string() }
        });
        if let Some(claim) = claim {
            envelope["actor"] = json!(claim);
        }
        exchange(&self.daemon.socket(), &envelope)
    }

    fn open(&self) -> SessionOffer {
        offer_of(&self.open_as(None, "watch"))
    }

    /// The pid the fixture recorded when `command.invoke` reached it.
    fn invoked_pid(&self) -> Option<u32> {
        self.report()
            .into_iter()
            .find(|r| {
                r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "command.invoke"
            })
            .map(|r| r["pid"].as_u64().unwrap() as u32)
    }

    fn wait_invoked(&self) -> u32 {
        let mut pid = None;
        assert!(
            wait_for(20, || {
                pid = self.invoked_pid();
                pid.is_some()
            }),
            "command.invoke never reached the adapter: {:?}",
            self.report()
        );
        pid.unwrap()
    }
}

fn exchange(socket: &Path, envelope: &Value) -> Value {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut line = envelope.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    serde_json::from_str(reply.trim()).unwrap_or_else(|e| panic!("{e}: {reply:?}"))
}

fn offer_of(reply: &Value) -> SessionOffer {
    assert_eq!(
        reply["response"]["kind"], "session",
        "not a session offer: {reply}"
    );
    serde_json::from_value(reply["response"].clone()).unwrap_or_else(|e| panic!("{e}: {reply}"))
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

async fn attach(offer: &SessionOffer, pty: &Pty) -> (SessionEvents, SessionControl) {
    let fd = pty.slave.as_raw_fd();
    SessionClient::attach(offer, true, fd, fd, Duration::from_secs(20))
        .await
        .expect("attach")
        .split()
}

async fn closing_line(events: &mut SessionEvents, secs: u64) -> Option<ServerLine> {
    tokio::time::timeout(Duration::from_secs(secs), events.next())
        .await
        .expect("the daemon sent its closing line in time")
        .expect("read the closing line")
}

fn code_of(reply: &Value) -> &str {
    reply["response"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("not an error: {reply}"))
}

/// The security and the main audit files.
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

/// A script whose session verb reports its terminal, echoes one line and finishes with `code`.
fn echo_script(code: i32) -> Value {
    json!([{ "on": "command.invoke", "do": [
        "terminal_report",
        { "stderr_text": { "text": "log line for the adapter's own log" } },
        { "terminal_echo": { "lines": 1 } },
        { "reply": { "result": { "exit_code": code } } }
    ] }])
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

// ---- the happy path --------------------------------------------------------------------------

#[tokio::test]
async fn a_session_adapter_receives_a_private_terminal() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let pty = Pty::new();
    let (mut events, control) = attach(&rig.open(), &pty).await;
    let pid = rig.wait_invoked();
    let terminal = std::fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd())).unwrap();
    let input = std::fs::read_link(format!("/proc/{pid}/fd/0")).unwrap();
    let output = std::fs::read_link(format!("/proc/{pid}/fd/1")).unwrap();
    drop(control);
    let _ = closing_line(&mut events, 20).await;

    assert_ne!(input, terminal);
    assert_eq!(input, output);
}

#[tokio::test]
async fn session_runs_the_verb_on_the_terminal_the_client_handed_over() {
    let rig = Rig::new(Setup::new(echo_script(7)));
    let offer = rig.open();
    assert_eq!(
        (offer.adapter.as_str(), offer.verb.as_str()),
        (NAME, "watch")
    );
    assert_eq!(offer.expires_in_secs, 60);

    let mut pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    rig.wait_invoked();
    pty.type_line("hello");
    let shown = pty.read_until("echo:hello", 20);
    assert!(
        shown.contains("echo:hello"),
        "the adapter answered on the terminal: {shown:?}"
    );
    match closing_line(&mut events, 20).await {
        Some(ServerLine::Done { exit_code, .. }) => assert_eq!(exit_code, 7),
        other => panic!("expected the verb's exit code, got {other:?}"),
    }

    let report = rig.report();
    // the terminal is the adapter's standard input and output, and it has no controlling terminal
    let terminal = report
        .iter()
        .find(|r| r["what"] == "terminal")
        .unwrap_or_else(|| panic!("no terminal record: {report:?}"));
    assert_eq!(
        (
            terminal["stdin_tty"].as_bool(),
            terminal["stdout_tty"].as_bool()
        ),
        (Some(true), Some(true)),
        "{terminal}"
    );
    assert_eq!(
        terminal["dev_tty_openable"], false,
        "a session adapter runs outside the terminal's session: {terminal}"
    );
    assert_eq!(terminal["session_socket_env"], true);
    // the verb arrived over the session socket, with the tty flag and the person who asked
    let invoke = report
        .iter()
        .find(|r| {
            r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "command.invoke"
        })
        .unwrap();
    let params = &invoke["frame"]["params"];
    assert_eq!(
        (params["verb"].as_str(), params["tty"].as_bool()),
        (Some("watch"), Some(true)),
        "{params}"
    );
    assert_eq!(params["argv"], json!(["a"]));
    assert_eq!(params["cwd"], rig.project.display().to_string());
    assert_eq!(params["actor"]["class"], "human");
    // standard error is the adapter's log, never the terminal
    assert!(!shown.contains("log line for the adapter"), "{shown:?}");
    let info = rig.kit.run(&["adapter", "info", NAME]);
    assert!(
        info.all().contains("log line for the adapter"),
        "{}",
        info.all()
    );
    // the process of the session is gone and the socket with it
    let pid = invoke["pid"].as_u64().unwrap() as u32;
    assert!(
        wait_for(20, || !pid_alive(pid)),
        "the adapter of a finished session is ended"
    );
    assert!(
        wait_for(20, || !Path::new(&offer.socket).exists()),
        "the socket of a finished session is removed"
    );
}

#[tokio::test]
async fn session_socket_is_private_and_its_path_is_short() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let offer = rig.open();
    use std::os::unix::fs::PermissionsExt;
    let socket = Path::new(&offer.socket);
    assert!(offer.socket.len() < 100, "{}", offer.socket);
    assert_eq!(socket.parent().unwrap(), rig.sessions_dir());
    assert_eq!(
        std::fs::metadata(socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(rig.sessions_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(offer.token.len(), 64);
    assert_eq!(rig.open().token.len(), 64);
}

// ---- the tokens ------------------------------------------------------------------------------

#[tokio::test]
async fn session_token_is_single_use() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let offer = rig.open();
    let pty = Pty::new();
    let fd = pty.slave.as_raw_fd();

    // a wrong token attaches nothing and does not spend the right one
    let wrong = SessionOffer {
        token: "0".repeat(64),
        ..offer.clone()
    };
    let (mut stranger, _keep) = SessionClient::attach(&wrong, true, fd, fd, Duration::from_secs(5))
        .await
        .expect("the connection is accepted")
        .split();
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), stranger.next()).await,
            Ok(Ok(None)) | Ok(Err(_))
        ),
        "a wrong token is dropped"
    );
    assert_eq!(rig.started_since(), 0, "nothing ran for a wrong token");

    // the right one attaches
    let (_events, _control) = attach(&offer, &pty).await;
    rig.wait_invoked();

    // and once used it opens nothing again: the socket is gone while the session runs
    assert!(
        wait_for(20, || !Path::new(&offer.socket).exists()),
        "the socket is removed once both sides are connected"
    );
    let again = SessionClient::attach(&offer, true, fd, fd, Duration::from_secs(2)).await;
    assert!(again.is_err(), "a spent token cannot be presented");
    assert_eq!(rig.started_since(), 1, "one adapter process for one token");
}

#[tokio::test]
async fn session_token_expires() {
    let rig = Rig::new(Setup::new(echo_script(0)).env("OXIDEPM_TEST_SESSION_WINDOW_MS", "600"));
    let offer = rig.open();
    assert_eq!(
        offer.expires_in_secs, 1,
        "the window rounds up to whole seconds"
    );
    std::thread::sleep(Duration::from_millis(1500));
    let pty = Pty::new();
    let fd = pty.slave.as_raw_fd();
    let late = SessionClient::attach(&offer, true, fd, fd, Duration::from_secs(2)).await;
    assert!(late.is_err(), "the socket is gone after the window");
    assert!(!Path::new(&offer.socket).exists());
    assert_eq!(
        rig.started_since(),
        0,
        "no adapter was started for a token that ran out"
    );
    // and the place it held is free
    let again = rig.open_as(None, "watch");
    assert_eq!(again["response"]["kind"], "session", "{again}");
}

#[tokio::test]
async fn session_ends_when_the_adapter_dies() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": { "crash": { "code": 3 } } }]),
    ));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    match closing_line(&mut events, 30).await {
        Some(ServerLine::Error { code, .. }) => assert_eq!(code, "E_ADAPTER_CRASHED"),
        other => panic!("expected the crash to be reported, got {other:?}"),
    }
    let pid = rig.wait_invoked();
    assert!(wait_for(20, || !pid_alive(pid)));
    assert!(wait_for(20, || !Path::new(&offer.socket).exists()));
}

#[tokio::test]
async fn session_limit_is_four_at_once() {
    let rig = Rig::new(Setup::new(json!([])).env("OXIDEPM_TEST_SESSION_WINDOW_MS", "4000"));
    let offers: Vec<_> = (0..4).map(|_| rig.open_as(None, "watch")).collect();
    assert!(
        offers.iter().all(|o| o["response"]["kind"] == "session"),
        "{offers:?}"
    );
    let fifth = rig.open_as(None, "watch");
    assert_eq!(code_of(&fifth), "E_ADAPTER_BUSY", "{fifth}");
    // unattached sessions give up their place when their tokens run out
    assert!(
        wait_for(20, || rig.open_as(None, "watch")["response"]["kind"]
            == "session"),
        "a place frees up"
    );
}

// ---- the end of a session --------------------------------------------------------------------

#[tokio::test]
async fn session_ends_when_the_connection_closes() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let offer = rig.open();
    let pty = Pty::new();
    let (events, control) = attach(&offer, &pty).await;
    let pid = rig.wait_invoked();
    assert!(pid_alive(pid));
    // the client goes away
    drop((events, control));
    assert!(
        wait_for(30, || !pid_alive(pid)),
        "the adapter of a session whose client left is killed"
    );
    assert!(
        wait_for(20, || !Path::new(&offer.socket).exists()),
        "the socket is gone"
    );
    // a place is free again: four more can be opened
    let more: Vec<_> = (0..4).map(|_| rig.open_as(None, "watch")).collect();
    assert!(
        more.iter().all(|o| o["response"]["kind"] == "session"),
        "{more:?}"
    );
}

#[tokio::test]
async fn session_ends_when_the_daemon_stops() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    let pid = rig.wait_invoked();
    rig.daemon.signal(nix::sys::signal::Signal::SIGTERM);
    match closing_line(&mut events, 30).await {
        Some(ServerLine::Error { code, .. }) => assert!(!code.is_empty()),
        None => {}
        other => panic!("{other:?}"),
    }
    assert!(
        wait_for(30, || !pid_alive(pid)),
        "the daemon's shutdown ends the session's adapter"
    );
}

#[tokio::test]
async fn session_ends_at_its_time_limit() {
    let rig = Rig::new(
        Setup::new(json!([{ "on": "command.invoke", "do": "hang" }]))
            .env("OXIDEPM_TEST_SESSION_MAX_MS", "1500"),
    );
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    let pid = rig.wait_invoked();
    match closing_line(&mut events, 30).await {
        Some(ServerLine::Error { message, .. }) => assert!(message.contains("limit"), "{message}"),
        other => panic!("expected the limit to end it, got {other:?}"),
    }
    assert!(wait_for(30, || !pid_alive(pid)));
}

#[tokio::test]
async fn winch_from_the_client_reaches_the_adapter() {
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        "watch_winch",
        { "sleep": { "ms": 1500 } },
        "report_winch",
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, mut control) = attach(&offer, &pty).await;
    assert!(wait_for(20, || rig
        .report()
        .iter()
        .any(|r| r["what"] == "watch_winch")));
    control.winch().await.unwrap();
    control.winch().await.unwrap();
    assert!(matches!(
        closing_line(&mut events, 30).await,
        Some(ServerLine::Done { exit_code: 0, .. })
    ));
    let counted = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "winch")
        .expect("the count was reported");
    assert!(
        counted["count"].as_u64().unwrap() >= 1,
        "SIGWINCH reached the adapter: {counted}"
    );
}

// ---- who is the session -----------------------------------------------------------------------

#[tokio::test]
async fn session_requests_are_labelled_with_the_adapter() {
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        { "host_request": { "request": { "type": "show", "selector": { "by": "name", "value": "nothing" } } } },
        { "sleep": { "ms": 300 } },
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    assert!(matches!(
        closing_line(&mut events, 30).await,
        Some(ServerLine::Done { .. })
    ));
    let answered = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "host_answer")
        .expect("the daemon answered the adapter");
    assert!(answered["frame"].get("result").is_some(), "{answered}");
    // the audit log names the adapter and the way in
    assert!(
        wait_for(20, || audit_records(&rig).iter().any(|r| r["actor"]
            ["class"]
            == "adapter"
            && r["actor"]["label"] == NAME
            && r["actor"]["via"] == "adapter_session")),
        "{:?}",
        audit_records(&rig)
    );
}

#[tokio::test]
async fn session_descendants_are_labelled_with_the_adapter() {
    // a process the session adapter starts and that dials the main socket is the adapter's, not a
    // person's, whatever it claims: it gets the adapter's grant and cannot open a session
    // an adapter that may start programs cannot declare a safe verb
    let mut setup = Setup::new(json!([])).reaching_the_daemon();
    setup.verbs = vec![verb("watch", "session", "caution")];
    let rig = Rig::new(setup);
    let socket = rig.daemon.socket().display().to_string();
    let restart = json!({ "protocol": 2, "request_id": "d-1", "actor": "human:cli", "request": { "type": "restart", "selector": { "by": "name", "value": "victim" } } }).to_string();
    let open = json!({
        "protocol": 2, "request_id": "d-2", "actor": "human:cli",
        "request": { "type": "adapter", "op": "session_open", "verb": "watch", "argv": [], "cwd": rig.project.display().to_string() }
    })
    .to_string();
    rig.kit.write_script(
        NAME,
        json!([{ "on": "command.invoke", "do": [
            { "dial_from_child": { "path": socket.clone(), "send": restart } },
            { "dial_from_child": { "path": socket, "send": open } },
            { "sleep": { "ms": 1200 } },
            { "reply": { "result": { "exit_code": 0 } } }
        ] }]),
    );
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    let mut dials = Vec::new();
    assert!(
        wait_for(20, || {
            dials = rig
                .report()
                .into_iter()
                .filter(|r| r["what"] == "socket")
                .collect();
            dials.len() >= 2
        }),
        "{:?}",
        rig.report()
    );
    for dialed in &dials {
        assert_eq!(dialed["from"], "child", "{dialed}");
        let answer: Value =
            serde_json::from_str(dialed["answer"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
        assert_eq!(answer["response"]["code"], "E_POLICY_DENIED", "{dialed}");
        assert!(
            answer["response"]["message"]
                .as_str()
                .unwrap_or("")
                .contains(&format!("adapter:{NAME}")),
            "{dialed}"
        );
    }
    assert!(matches!(
        closing_line(&mut events, 30).await,
        Some(ServerLine::Done { .. })
    ));
    assert_eq!(
        std::fs::read_dir(rig.sessions_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "no second session was opened"
    );
}

// ---- policy ----------------------------------------------------------------------------------

#[tokio::test]
async fn ai_claim_opens_only_ai_driven_verbs() {
    // not marked ai_driven: refused, and nothing ran
    let plain = Rig::new(Setup::new(json!([])));
    let refused = plain.open_as(Some("ai:example-agent"), "watch");
    assert_eq!(code_of(&refused), "E_POLICY_DENIED", "{refused}");
    assert!(
        refused["response"]["message"]
            .as_str()
            .unwrap()
            .contains("ai_driven"),
        "{refused}"
    );
    assert_eq!(plain.started_since(), 0);
    // through the agent socket too (that socket is an ai by its own rule)
    let agent = exchange(
        &plain.daemon.agent_socket(),
        &json!({ "protocol": 2, "request_id": "a-1", "request": { "type": "adapter", "op": "session_open", "verb": "watch", "argv": [], "cwd": plain.project.display().to_string() } }),
    );
    assert_eq!(code_of(&agent), "E_POLICY_DENIED", "{agent}");

    // marked ai_driven: an ai claim may open the session, a person always can
    let driven = Rig::new(
        Setup::new(json!([{ "on": "command.invoke", "do": "hang" }]))
            .manifest("ai_driven", json!(true)),
    );
    let opened = driven.open_as(Some("ai:example-agent"), "watch");
    assert_eq!(opened["response"]["kind"], "session", "{opened}");
    assert_eq!(
        driven.open_as(Some("human:cli"), "watch")["response"]["kind"],
        "session"
    );
    // but only the interactive verbs of it: a relayed verb is not opened as a session
    let relayed = driven.open_as(Some("ai:example-agent"), "look");
    assert_eq!(code_of(&relayed), "E_UNSUPPORTED", "{relayed}");

    // the refusals are written down, the claim as the actor
    assert!(
        wait_for(20, || audit_records(&plain)
            .iter()
            .any(|r| r["decision"] == "deny"
                && r["op"] == "adapter session"
                && r["actor"]["class"] == "ai")),
        "{:?}",
        audit_records(&plain)
    );
}

#[tokio::test]
async fn ai_claim_cannot_open_the_session_of_an_adapter_above_read() {
    let rig = Rig::new(
        Setup::new(json!([]))
            .manifest("ai_driven", json!(true))
            .perms(json!({ "host": { "level": "lifecycle", "scope": "all" } })),
    );
    let refused = rig.open_as(Some("ai:example-agent"), "watch");
    assert_eq!(code_of(&refused), "E_POLICY_DENIED", "{refused}");
    assert!(
        refused["response"]["message"]
            .as_str()
            .unwrap()
            .contains("lifecycle"),
        "{refused}"
    );
    assert_eq!(
        rig.open_as(None, "watch")["response"]["kind"],
        "session",
        "a person may"
    );
}

#[tokio::test]
async fn session_opened_by_an_ai_claim_writes_only_to_the_adapters_own_directory() {
    need_landlock!();
    // the same verb, the same grant ($CWD is writable): what an ai claim opens gets less than a person
    let project_script = |dir: &Path| {
        json!([{ "on": "command.invoke", "do": [
            { "touch": { "path": dir.join("written-by-the-session").display().to_string() } },
            { "reply": { "result": { "exit_code": 0 } } }
        ] }])
    };
    let rig = Rig::new(Setup::new(json!([])).manifest("ai_driven", json!(true)));
    rig.kit.write_script(NAME, project_script(&rig.project));
    let target = rig.project.join("written-by-the-session");

    let ai = offer_of(&rig.open_as(Some("ai:example-agent"), "watch"));
    {
        let pty = Pty::new();
        let (mut events, _control) = attach(&ai, &pty).await;
        let line = closing_line(&mut events, 30).await;
        assert!(
            matches!(line, Some(ServerLine::Done { .. })),
            "expected Done for ai session, got {line:?}"
        );
    }
    assert!(
        !target.exists(),
        "an ai claim cannot make the session write in the working directory"
    );

    let person = rig.open();
    {
        let pty = Pty::new();
        let (mut events, _control) = attach(&person, &pty).await;
        let line = closing_line(&mut events, 30).await;
        assert!(
            matches!(line, Some(ServerLine::Done { .. })),
            "expected Done for person session, got {line:?}"
        );
    }
    assert!(
        target.exists(),
        "a person's session can: the grant allows it"
    );
}

#[tokio::test]
async fn an_adapter_cannot_open_a_session() {
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        { "host_request": { "request": { "type": "adapter", "op": "session_open", "verb": "watch", "argv": [], "cwd": "/" } } },
        { "sleep": { "ms": 300 } },
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    assert!(matches!(
        closing_line(&mut events, 30).await,
        Some(ServerLine::Done { .. })
    ));
    let answered = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "host_answer")
        .expect("answered");
    let response = &answered["frame"]["result"]["response"];
    assert_eq!(response["type"], "error", "{answered}");
    assert_eq!(response["code"], "E_POLICY_DENIED", "{answered}");
}

#[tokio::test]
async fn a_relayed_verb_has_no_session_and_a_session_verb_is_not_relayed() {
    let rig = Rig::new(Setup::new(json!([])));
    let reply = rig.open_as(None, "look");
    assert_eq!(code_of(&reply), "E_UNSUPPORTED", "{reply}");
    assert!(
        reply["response"]["message"]
            .as_str()
            .unwrap()
            .contains("not an interactive verb"),
        "{reply}"
    );
    // asked as a relayed call, the session verb says it needs a terminal
    let mut stream = UnixStream::connect(rig.daemon.socket()).unwrap();
    let line = json!({ "protocol": 2, "request_id": "i-1", "request": { "type": "adapter", "op": "invoke", "verb": "watch", "argv": [], "cwd": rig.project.display().to_string() } });
    stream.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(reply.trim()).unwrap();
    assert_eq!(code_of(&reply), "E_UNSUPPORTED", "{reply}");
    assert!(
        reply["response"]["message"]
            .as_str()
            .unwrap()
            .contains("needs a terminal"),
        "{reply}"
    );
    // a rehearsal is refused: a session cannot be rehearsed, and must not be opened for real
    let rehearsal = exchange(
        &rig.daemon.socket(),
        &json!({ "protocol": 2, "request_id": "r-1", "dry_run": true, "request": { "type": "adapter", "op": "session_open", "verb": "watch", "argv": [], "cwd": rig.project.display().to_string() } }),
    );
    assert_eq!(code_of(&rehearsal), "E_DRY_RUN_UNSUPPORTED", "{rehearsal}");
    // a directory the daemon does not allow is refused before a socket is made
    let bad = exchange(
        &rig.daemon.socket(),
        &json!({ "protocol": 2, "request_id": "c-1", "request": { "type": "adapter", "op": "session_open", "verb": "watch", "argv": [], "cwd": "/" } }),
    );
    assert_eq!(code_of(&bad), "E_POLICY_DENIED", "{bad}");
    assert_eq!(
        std::fs::read_dir(rig.sessions_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "no socket was made"
    );
}

// ---- what runs ---------------------------------------------------------------------------------

#[tokio::test]
async fn session_runs_only_the_verified_installed_executable() {
    let rig = Rig::new(Setup::new(echo_script(0)));
    // the installed file is replaced: the digest recorded at import no longer matches
    let installed = rig.kit.home.join("adapters").join("installed").join(NAME);
    let exe = std::fs::read_dir(&installed)
        .unwrap()
        .flatten()
        .flat_map(|v| {
            std::fs::read_dir(v.path())
                .unwrap()
                .flatten()
                .map(|f| f.path())
                .collect::<Vec<_>>()
        })
        .find(|p| {
            p.is_file()
                && std::fs::metadata(p)
                    .map(|m| m.len() > 10_000)
                    .unwrap_or(false)
        })
        .expect("the installed executable");
    let mut bytes = std::fs::read(&exe).unwrap();
    bytes.push(0);
    // installed files and their directories are read-only: make room, then replace the file
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        exe.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::remove_file(&exe).unwrap();
    std::fs::write(&exe, &bytes).unwrap();

    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    match closing_line(&mut events, 30).await {
        Some(ServerLine::Error { code, message }) => {
            assert_eq!(code, "E_ADAPTER_TAMPERED", "{message}");
        }
        other => panic!("a changed executable must not run: {other:?}"),
    }
    assert_eq!(rig.started_since(), 0, "the tampered file was not started");
}

#[tokio::test]
async fn session_does_not_follow_a_link_in_place_of_the_executable() {
    let rig = Rig::new(Setup::new(echo_script(0)));
    let installed = rig.kit.home.join("adapters").join("installed").join(NAME);
    let exe = std::fs::read_dir(&installed)
        .unwrap()
        .flatten()
        .flat_map(|v| {
            std::fs::read_dir(v.path())
                .unwrap()
                .flatten()
                .map(|f| f.path())
                .collect::<Vec<_>>()
        })
        .find(|p| {
            p.is_file()
                && std::fs::metadata(p)
                    .map(|m| m.len() > 10_000)
                    .unwrap_or(false)
        })
        .expect("the installed executable");
    // the very same bytes, but reached through a link kept outside installed/
    let copy = rig.kit.dir.path().join("elsewhere");
    std::fs::copy(&exe, &copy).unwrap();
    std::fs::remove_file(&exe).unwrap();
    std::os::unix::fs::symlink(&copy, &exe).unwrap();

    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    match closing_line(&mut events, 30).await {
        Some(ServerLine::Error { code, .. }) => assert!(
            code == "E_ADAPTER_MISSING" || code == "E_ADAPTER_TAMPERED",
            "{code}"
        ),
        other => panic!("a link must not be followed: {other:?}"),
    }
    assert_eq!(rig.started_since(), 0);
}

#[tokio::test]
async fn session_adapter_runs_under_the_sandbox() {
    need_landlock!();
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        "echo_status",
        { "inet_socket": {} },
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let offer = rig.open();
    let pty = Pty::new();
    let (mut events, _control) = attach(&offer, &pty).await;
    assert!(matches!(
        closing_line(&mut events, 30).await,
        Some(ServerLine::Done { .. })
    ));
    let report = rig.report();
    let status = report
        .iter()
        .find(|r| r["what"] == "status")
        .unwrap_or_else(|| panic!("{report:?}"));
    assert_eq!(status["no_new_privs"], "1", "{status}");
    assert_eq!(
        status["seccomp"], "2",
        "the launcher's filter is in place: {status}"
    );
}

#[tokio::test]
async fn stale_session_sockets_are_removed_when_the_daemon_starts() {
    let rig = Rig::new(Setup::new(json!([])));
    let offer = rig.open();
    assert!(Path::new(&offer.socket).exists());
    let (kit, home) = (&rig.kit, rig.kit.home.clone());
    drop(rig.daemon);
    // the daemon is gone; what it left behind stays until the next one starts
    let stale = home.join("adapters").join("sessions").join("deadbeef.sock");
    std::fs::write(&stale, b"").unwrap();
    let daemon = kit.daemon();
    assert!(
        wait_for(20, || !stale.exists() && !Path::new(&offer.socket).exists()),
        "old session sockets are removed at start"
    );
    drop(daemon);
}

// ---- the CLI ----------------------------------------------------------------------------------

use std::process::{Child, Command, Stdio};

impl Rig {
    /// `oxidepm watch` in the project directory, with the given standard input and output.
    fn cli_watch(&self, stdin: Stdio, stdout: Stdio, actor: Option<&str>) -> Child {
        let mut cmd: Command = self.kit.cli();
        cmd.arg("watch")
            .current_dir(&self.project)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::piped());
        if let Some(actor) = actor {
            cmd.env("OXIDEPM_ACTOR", actor);
        }
        cmd.spawn().expect("run the CLI")
    }
}

/// Wait for a child to exit; kill it and fail if it does not.
fn wait_child(child: &mut Child, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the CLI did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut text);
    }
    text
}

fn termios_of(fd: RawFd) -> libc::termios {
    // SAFETY: tcgetattr fills the struct we give it.
    unsafe {
        let mut settings: libc::termios = std::mem::zeroed();
        assert_eq!(
            libc::tcgetattr(fd, &mut settings),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        settings
    }
}

#[test]
fn cli_runs_a_session_verb_on_its_own_pipes() {
    // the shape of an MCP client: standard input and output are pipes
    let rig = Rig::new(Setup::new(echo_script(7)));
    let mut cli = rig.cli_watch(Stdio::piped(), Stdio::piped(), None);
    let mut stdin = cli.stdin.take().unwrap();
    let mut stdout = BufReader::new(cli.stdout.take().unwrap());
    stdin.write_all(b"hi\n").unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(
        line.trim(),
        "echo:hi",
        "the adapter wrote straight to the CLI's standard output"
    );
    assert_eq!(
        wait_child(&mut cli, 30).code(),
        Some(7),
        "the verb's exit code is the CLI's: {}",
        stderr_of(&mut cli)
    );
    let terminal = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "terminal")
        .expect("terminal record");
    assert_eq!(
        (
            terminal["stdin_tty"].as_bool(),
            terminal["stdout_tty"].as_bool()
        ),
        (Some(false), Some(false)),
        "{terminal}"
    );
    // the CLI told the adapter whether it had a terminal
    let invoke = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "frame" && r["frame"]["method"] == "command.invoke")
        .unwrap();
    assert_eq!(invoke["frame"]["params"]["tty"], false);
    assert_eq!(
        invoke["frame"]["params"]["cwd"],
        rig.project.display().to_string()
    );
}

#[test]
fn cli_hands_over_a_terminal_and_puts_its_settings_back() {
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        "terminal_raw",
        "terminal_report",
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let pty = Pty::new();
    let before = termios_of(pty.slave.as_raw_fd());
    let mut cli = rig.cli_watch(
        Stdio::from(pty.slave.try_clone().unwrap()),
        Stdio::from(pty.slave.try_clone().unwrap()),
        None,
    );
    assert_eq!(
        wait_child(&mut cli, 30).code(),
        Some(0),
        "{}",
        stderr_of(&mut cli)
    );
    let terminal = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "terminal")
        .expect("terminal record");
    assert_eq!(
        (
            terminal["stdin_tty"].as_bool(),
            terminal["stdout_tty"].as_bool()
        ),
        (Some(true), Some(true)),
        "{terminal}"
    );
    assert!(
        rig.report()
            .iter()
            .any(|r| r["what"] == "terminal_raw" && r["ok"] == true),
        "the adapter put the terminal in raw mode"
    );
    // the adapter left it raw; the CLI put back what it found
    let after = termios_of(pty.slave.as_raw_fd());
    assert_eq!(
        (after.c_lflag, after.c_iflag, after.c_oflag),
        (before.c_lflag, before.c_iflag, before.c_oflag),
        "the terminal is as it was"
    );
}

#[test]
fn cli_passes_window_changes_on() {
    let rig = Rig::new(Setup::new(json!([{ "on": "command.invoke", "do": [
        "watch_winch",
        { "sleep": { "ms": 2500 } },
        "report_winch",
        { "reply": { "result": { "exit_code": 0 } } }
    ] }])));
    let pty = Pty::new();
    let mut cli = rig.cli_watch(
        Stdio::from(pty.slave.try_clone().unwrap()),
        Stdio::from(pty.slave.try_clone().unwrap()),
        None,
    );
    assert!(wait_for(20, || rig
        .report()
        .iter()
        .any(|r| r["what"] == "watch_winch")));
    // the CLI installs its handler before it attaches, so by now a signal is a window change
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(cli.id() as i32),
        nix::sys::signal::Signal::SIGWINCH,
    )
    .unwrap();
    assert_eq!(
        wait_child(&mut cli, 30).code(),
        Some(0),
        "{}",
        stderr_of(&mut cli)
    );
    let counted = rig
        .report()
        .into_iter()
        .find(|r| r["what"] == "winch")
        .expect("the count was reported");
    assert!(counted["count"].as_u64().unwrap() >= 1, "{counted}");
}

#[test]
fn cli_that_is_stopped_ends_the_session() {
    let rig = Rig::new(Setup::new(
        json!([{ "on": "command.invoke", "do": "hang" }]),
    ));
    let mut cli = rig.cli_watch(Stdio::piped(), Stdio::piped(), None);
    let pid = rig.wait_invoked();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(cli.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert_eq!(wait_child(&mut cli, 30).code(), Some(143));
    assert!(
        wait_for(30, || !pid_alive(pid)),
        "the adapter does not outlive the CLI that asked for it"
    );
    // and a CLI that is killed outright leaves no session behind either
    let mut again = rig.cli_watch(Stdio::piped(), Stdio::piped(), None);
    assert!(wait_for(20, || rig.starts() >= rig.baseline + 2));
    let second = rig
        .report()
        .into_iter()
        .rfind(|r| r["what"] == "start")
        .unwrap()["pid"]
        .as_u64()
        .unwrap() as u32;
    again.kill().unwrap();
    let _ = again.wait();
    assert!(
        wait_for(30, || !pid_alive(second)),
        "a killed CLI ends its session too"
    );
}

#[test]
fn cli_will_not_hand_its_terminal_to_a_socket_others_can_write_beside() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(Setup::new(json!([])));
    // a socket of our own, in a directory every user can write to, standing in for the daemon
    let dir = rig.kit.dir.path().join("shared");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = dir.join("fake.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&received);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                counter.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });
    let mut cmd = rig.kit.cli();
    cmd.arg("watch")
        .env("OXIDEPM_SOCKET", &path)
        .current_dir(&rig.project)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cli = cmd.spawn().unwrap();
    // a CLI that talked to the stand-in would wait for an answer that never comes
    let status = wait_child(&mut cli, 20);
    assert_eq!(status.code(), Some(1));
    let text = stderr_of(&mut cli);
    assert!(
        text.contains("will not hand your terminal") && text.contains("can be written to"),
        "{text}"
    );
    assert_eq!(
        received.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was sent to it"
    );
}

#[test]
fn cli_has_no_dry_run_for_a_session_and_starts_nothing() {
    let rig = Rig::new(Setup::new(json!([])));
    let output = rig
        .kit
        .cli()
        .args(["watch", "--dry-run"])
        .current_dir(&rig.project)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no --dry-run"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(rig.started_since(), 0);
    assert_eq!(
        std::fs::read_dir(rig.sessions_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "no session was opened"
    );
}

#[test]
fn cli_without_a_daemon_says_so_and_starts_none() {
    let rig = Rig::new(Setup::new(json!([])));
    let (kit, project) = (&rig.kit, rig.project.clone());
    let socket = rig.daemon.socket();
    drop(rig.daemon);
    assert!(wait_for(20, || !socket.exists()
        || UnixStream::connect(&socket).is_err()));
    let output = kit
        .cli()
        .arg("watch")
        .current_dir(&project)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Daemon is not running"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        UnixStream::connect(&socket).is_err(),
        "the CLI did not start a daemon for a session"
    );
}

#[test]
fn cli_with_an_ai_claim_opens_only_the_session_of_an_ai_driven_adapter() {
    // the shape of an MCP client: it runs `oxidepm watch` with its claim in the environment
    let plain = Rig::new(Setup::new(echo_script(0)));
    let mut cli = plain.cli_watch(Stdio::piped(), Stdio::piped(), Some("ai:example-agent"));
    drop(cli.stdin.take());
    assert_eq!(wait_child(&mut cli, 30).code(), Some(1));
    let text = stderr_of(&mut cli);
    assert!(
        text.contains("E_POLICY_DENIED") && text.contains("ai_driven"),
        "{text}"
    );
    assert_eq!(plain.started_since(), 0);

    let driven = Rig::new(Setup::new(echo_script(0)).manifest("ai_driven", json!(true)));
    let mut cli = driven.cli_watch(Stdio::piped(), Stdio::piped(), Some("ai:example-agent"));
    let mut stdin = cli.stdin.take().unwrap();
    let mut stdout = BufReader::new(cli.stdout.take().unwrap());
    stdin.write_all(b"ping\n").unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "echo:ping");
    assert_eq!(
        wait_child(&mut cli, 30).code(),
        Some(0),
        "{}",
        stderr_of(&mut cli)
    );
    // the adapter was told who asked
    let invoke = driven
        .report()
        .into_iter()
        .find(|r| r["what"] == "frame" && r["frame"]["method"] == "command.invoke")
        .unwrap();
    assert_eq!(
        invoke["frame"]["params"]["actor"]["class"], "ai",
        "{invoke}"
    );
    // an ai claim writes nowhere but the adapter's own directory: the project is not in its profile
    assert!(driven.report().iter().any(|r| r["what"] == "terminal"));
}
