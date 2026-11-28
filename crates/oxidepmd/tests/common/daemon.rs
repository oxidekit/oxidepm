//! Runs the real `oxidepmd` binary in a private home and talks to it over its socket.
//!
//! `OXIDEPM_HOME` (and `HOME`, as a second guard) are set on the child only, never in the test
//! process, so tests stay independent of each other and of the real `~/.oxidepm`. Requests and
//! responses are raw JSON, so the same tests keep compiling when the wire types change; `rpc`
//! wraps requests in the envelope and unwraps the answer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub use nix::sys::signal::Signal;

const CAP: Duration = Duration::from_secs(10);

/// The `oxidepmd` binary staged into a private `0700` per-run directory next to `oxidepm-sandbox`.
pub fn daemon_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let _ = super::fixture::launcher_bin();
        super::fixture::stage_binary(Path::new(env!("CARGO_BIN_EXE_oxidepmd")), "oxidepmd")
    })
}

pub struct DaemonProc {
    child: Child,
    home: PathBuf,
    stderr_name: String,
}

impl DaemonProc {
    /// Start the daemon with `home` as its OxidePM home and wait until its socket accepts.
    pub fn spawn(home: &Path) -> Self {
        let mut daemon = Self::spawn_unready(home);
        daemon.wait_for_socket();
        daemon
    }

    /// Like [`spawn`](Self::spawn), with extra environment variables on the daemon process (the
    /// test process itself is not touched).
    pub fn spawn_with_env(home: &Path, env: &[(&str, &str)]) -> Self {
        let mut daemon = Self::spawn_unready_inner(daemon_bin(), home, "daemon-test.stderr", env);
        daemon.wait_for_socket();
        daemon
    }

    /// Like [`spawn_with_env`](Self::spawn_with_env), running another copy of the daemon binary
    /// (one that has nothing installed next to it).
    pub fn spawn_binary(binary: &Path, home: &Path, env: &[(&str, &str)]) -> Self {
        let mut daemon = Self::spawn_unready_inner(binary, home, "daemon-test.stderr", env);
        daemon.wait_for_socket();
        daemon
    }

    /// Start the daemon with `home` as its OxidePM home and return at once, before its socket is
    /// up (the daemon may also exit during startup: see [`try_exit`](Self::try_exit)).
    pub fn spawn_unready(home: &Path) -> Self {
        Self::spawn_unready_with_stderr(home, "daemon-test.stderr")
    }

    /// Like [`spawn_unready`](Self::spawn_unready), with the name of the stderr capture file (two
    /// daemons on one home need two).
    pub fn spawn_unready_with_stderr(home: &Path, stderr_name: &str) -> Self {
        Self::spawn_unready_inner(daemon_bin(), home, stderr_name, &[])
    }

    fn spawn_unready_inner(
        binary: &Path,
        home: &Path,
        stderr_name: &str,
        env: &[(&str, &str)],
    ) -> Self {
        std::fs::create_dir_all(home).expect("create daemon home");
        let stderr = std::fs::File::create(home.join(stderr_name)).expect("stderr file");
        // A binary that a test has just copied can be busy (ETXTBSY, 26) for a moment: another test thread
        // that forked meanwhile still holds the write descriptor until its own exec.
        let mut attempts = 0;
        let child = loop {
            let spawned = Command::new(binary)
                .envs(env.iter().copied())
                .env("OXIDEPM_HOME", home)
                .env("HOME", home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr.try_clone().expect("clone the stderr file"))
                .spawn();
            match spawned {
                Ok(child) => break child,
                Err(e) if e.raw_os_error() == Some(26) && attempts < 100 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("spawn oxidepmd: {e}"),
            }
        };
        Self {
            child,
            home: home.to_path_buf(),
            stderr_name: stderr_name.to_string(),
        }
    }

    /// The exit status if the daemon has exited, without waiting.
    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("try_wait")
    }

    /// Wait until the socket accepts connections (panics if the daemon exits first).
    pub fn wait_ready(&mut self) {
        self.wait_for_socket();
    }

    /// Wait for the daemon to finish loading adapters and restoring saved applications.
    pub fn wait_for_boot(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let hello = self.rpc(serde_json::json!({"type": "hello", "client": "test"}));
            if hello["phase"]["state"] == "ready" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon boot did not finish: {hello}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("daemon.sock")
    }

    /// The agent socket (every connection on it is class ai).
    pub fn agent_socket(&self) -> PathBuf {
        self.home.join("agent.sock")
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_for_socket(&mut self) {
        let deadline = Instant::now() + CAP;
        loop {
            if UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "oxidepmd exited early ({status}); stderr: {}",
                    self.stderr()
                );
            }
            if Instant::now() >= deadline {
                panic!("oxidepmd socket did not come up; stderr: {}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Everything the daemon wrote to stderr so far.
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.home.join(&self.stderr_name)).unwrap_or_default()
    }

    /// Send one request (wrapped in an envelope) on a fresh connection and return the
    /// response inside the answering envelope.
    pub fn rpc(&self, request: serde_json::Value) -> serde_json::Value {
        self.rpc_with(request, false)
    }

    /// Like [`rpc`](Self::rpc), with the envelope's `reveal` flag.
    pub fn rpc_with(&self, request: serde_json::Value, reveal: bool) -> serde_json::Value {
        let envelope = self.rpc_envelope(serde_json::json!({
            "protocol": 2,
            "request_id": "test",
            "reveal": reveal,
            "request": request,
        }));
        assert_eq!(
            envelope["request_id"], "test",
            "the request id is echoed: {envelope}"
        );
        assert_eq!(envelope["protocol"], 2, "{envelope}");
        envelope["response"].clone()
    }

    /// Send `request` with request id `id` and the given actor claim on the socket at `socket`;
    /// returns the whole answering envelope.
    pub fn rpc_on(
        &self,
        socket: &Path,
        id: &str,
        claim: Option<&str>,
        request: serde_json::Value,
    ) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket).expect("connect to the socket");
        stream
            .set_read_timeout(Some(CAP))
            .expect("set read timeout");
        let mut envelope = serde_json::json!({
            "protocol": 2,
            "request_id": id,
            "request": request,
        });
        if let Some(claim) = claim {
            envelope["actor"] = serde_json::json!(claim);
        }
        let mut line = envelope.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("write request");
        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .expect("read response");
        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("daemon sent non-JSON {response:?}: {e}"))
    }

    /// Like [`rpc_on`](Self::rpc_on), with the envelope's `dry_run` flag set.
    pub fn rpc_dry_on(
        &self,
        socket: &Path,
        id: &str,
        claim: Option<&str>,
        request: serde_json::Value,
    ) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket).expect("connect to the socket");
        stream
            .set_read_timeout(Some(CAP))
            .expect("set read timeout");
        let mut envelope = serde_json::json!({
            "protocol": 2,
            "request_id": id,
            "dry_run": true,
            "request": request,
        });
        if let Some(claim) = claim {
            envelope["actor"] = serde_json::json!(claim);
        }
        let mut line = envelope.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("write request");
        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .expect("read response");
        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("daemon sent non-JSON {response:?}: {e}"))
    }

    /// Wait (bounded) until the daemon's stderr has a line that holds every needle; returns it.
    pub fn wait_stderr_line(&self, needles: &[&str]) -> String {
        let deadline = Instant::now() + CAP;
        loop {
            let text = self.stderr();
            if let Some(line) = text.lines().find(|l| needles.iter().all(|n| l.contains(n))) {
                return line.to_string();
            }
            if Instant::now() >= deadline {
                panic!("no daemon log line with {needles:?}; stderr:\n{text}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Like [`rpc`](Self::rpc), but `None` instead of a panic when the daemon cannot be reached or
    /// hangs up without answering (a daemon that is going away).
    pub fn try_rpc(&self, request: serde_json::Value) -> Option<serde_json::Value> {
        let mut stream = UnixStream::connect(self.socket()).ok()?;
        stream.set_read_timeout(Some(CAP)).ok()?;
        let mut line = serde_json::json!({
            "protocol": 2,
            "request_id": "try",
            "request": request,
        })
        .to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).ok()?;
        let mut response = String::new();
        BufReader::new(&stream).read_line(&mut response).ok()?;
        let envelope: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
        Some(envelope["response"].clone())
    }

    /// Send `request` (in an envelope) and close the connection at once, without reading the
    /// answer.
    pub fn send_and_hang_up(&self, request: serde_json::Value) {
        let mut stream = UnixStream::connect(self.socket()).expect("connect to daemon");
        let mut line = serde_json::json!({
            "protocol": 2,
            "request_id": "hangup",
            "request": request,
        })
        .to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("write request");
        // dropped here: the daemon's answer finds nobody listening
    }

    /// Send one raw JSON line on a fresh connection and return the JSON line that comes back
    /// (an envelope, or a bare response for a bare request).
    pub fn rpc_envelope(&self, line: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(self.socket()).expect("connect to daemon");
        stream
            .set_read_timeout(Some(CAP))
            .expect("set read timeout");
        let mut line = line.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("write request");
        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .expect("read response");
        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("daemon sent non-JSON {response:?}: {e}"))
    }

    /// Write `bytes` (in one go, errors ignored: the daemon may hang up on a flood) on a fresh
    /// connection and read up to `lines` response lines. Returns the lines that arrived and
    /// whether the daemon then closed the connection (EOF or reset within the read timeout).
    pub fn raw_exchange(&self, bytes: Vec<u8>, lines: usize) -> (Vec<serde_json::Value>, bool) {
        self.raw_exchange_on(&self.socket(), bytes, lines)
    }

    /// Like [`raw_exchange`](Self::raw_exchange), on the socket at `socket`.
    pub fn raw_exchange_on(
        &self,
        socket: &Path,
        bytes: Vec<u8>,
        lines: usize,
    ) -> (Vec<serde_json::Value>, bool) {
        let stream = UnixStream::connect(socket).expect("connect to daemon");
        stream
            .set_read_timeout(Some(CAP))
            .expect("set read timeout");
        let writer = stream.try_clone().expect("clone stream");
        let writer = std::thread::spawn(move || {
            let mut writer = writer;
            let _ = writer.write_all(&bytes);
            writer // keep the stream open until the reader is done
        });
        let mut reader = BufReader::new(&stream);
        let mut out = Vec::new();
        for _ in 0..lines {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => out.push(
                    serde_json::from_str(line.trim())
                        .unwrap_or_else(|e| panic!("daemon sent non-JSON {line:?}: {e}")),
                ),
            }
        }
        let mut rest = String::new();
        let closed = matches!(reader.read_line(&mut rest), Ok(0) | Err(_));
        drop(reader);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let _ = writer.join();
        (out, closed)
    }

    pub fn signal(&self, sig: Signal) {
        use nix::unistd::Pid;
        nix::sys::signal::kill(Pid::from_raw(self.child.id() as i32), sig).expect("signal daemon");
    }

    pub fn sigkill(&self) {
        self.signal(Signal::SIGKILL);
    }

    /// Wait (bounded) for the daemon to exit.
    pub fn wait_exit(&mut self) -> ExitStatus {
        self.wait_exit_for(CAP)
    }

    /// Wait up to `cap` for the daemon to exit.
    pub fn wait_exit_for(&mut self, cap: Duration) -> ExitStatus {
        let deadline = Instant::now() + cap;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("oxidepmd did not exit; stderr: {}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for DaemonProc {
    /// Kills the daemon this struct started and, as a leak guard for failing tests, any process
    /// whose working directory is under the test's private temp directory (the parent of the
    /// daemon home). Nothing else is ever signalled.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(root) = self.home.parent().and_then(|p| p.canonicalize().ok()) {
            if root.starts_with(std::env::temp_dir()) && root != std::env::temp_dir() {
                kill_processes_under(&root);
            }
        }
    }
}

/// SIGKILL every process whose working directory is under `root`.
fn kill_processes_under(root: &Path) {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            if cwd.starts_with(root) {
                let _ =
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
        }
    }
}
