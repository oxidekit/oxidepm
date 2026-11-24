//! IPC Client - Unix socket client for CLI

use oxidepm_core::constants::{DAEMON_LOG_FILE, EXIT_ALREADY_RUNNING};
use oxidepm_core::{Error, ErrorCode, Result};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::protocol::{
    Envelope, Request, Response, ResponseEnvelope, PROTOCOL_CROSS_VERSION_MIN, PROTOCOL_VERSION,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// How long a fast request (status, show, logs, ping, ...) may take.
pub const FAST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a slow request (start, stop, restart, delete, ...) may take.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a streamed answer (a relayed adapter verb) may stay silent before the client gives up.
/// Every line resets it, so a verb that is allowed an hour can run that long while it reports
/// progress; the daemon ends a tool that has said nothing for 120 s, so this is a little more.
pub const STREAM_INACTIVITY: Duration = Duration::from_secs(150);

const MAX_RESPONSE_LINE: usize = 16 * 1024 * 1024;
const STARTUP_LOG_BYTES: u64 = 64 * 1024;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    format!(
        "{:x}-{:x}",
        std::process::id(),
        REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Requests that can run for a long time (a start waits for a build, a stop for the kill
/// timeout); every other request is fast.
fn is_slow(request: &Request) -> bool {
    matches!(
        request,
        Request::Start { .. }
            | Request::StartBatch { .. }
            | Request::Stop { .. }
            | Request::Restart { .. }
            | Request::Reload { .. }
            | Request::Delete { .. }
            | Request::Resurrect
            | Request::Kill
    ) || matches!(request, Request::Adapter(a) if a.is_slow())
}

/// Per-call settings that travel in the request envelope.
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
    /// Ask `Show`/`Describe` for the real environment values and hook scripts.
    pub reveal: bool,
    /// The actor claim of this call (`human:<label>` or `ai:<label>`).
    pub actor: Option<String>,
    /// Ask the daemon for a plan of what the request would do (`Response::DryRun`) and change
    /// nothing. A read-only request ignores it.
    pub dry_run: bool,
}

/// Environment variable naming (by absolute path) the daemon binary to auto-start.
pub const DAEMON_BIN_ENV: &str = "OXIDEPM_DAEMON_BIN";

/// A daemon this client just spawned.
struct StartedDaemon {
    child: Child,
    log_path: Option<PathBuf>,
    /// Size of `daemon.log` before the spawn: only what the daemon wrote since is shown.
    log_offset: u64,
}

impl StartedDaemon {
    /// Message for a daemon that exited before its socket came up: the exit status and what
    /// it wrote to the log.
    fn exit_report(&self, status: ExitStatus) -> String {
        let mut text = format!("Daemon exited during startup ({})", status);
        if let Some(path) = &self.log_path {
            let written = log_tail(path, self.log_offset, 20);
            if written.is_empty() {
                text.push_str(&format!("; see {}", path.display()));
            } else {
                text.push_str(&format!(". {} says:\n{}", path.display(), written));
            }
        }
        text
    }
}

/// Open `{home}/daemon.log` for appending (mode 0600, home created 0700 if missing).
/// Returns the path, its current size, and the open file.
fn open_daemon_log(home: &Path) -> std::io::Result<(PathBuf, u64, std::fs::File)> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)?;
    let path = home.join(DAEMON_LOG_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    let offset = file.metadata()?.len();
    Ok((path, offset, file))
}

/// Read at most the most recent 64 KiB, starting no earlier than the spawn offset
/// unless the log has since been truncated.
fn log_tail(path: &Path, offset: u64, max_lines: usize) -> String {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let earliest = if offset <= len { offset } else { 0 };
        let start = earliest.max(len.saturating_sub(STARTUP_LOG_BYTES));
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(len - start).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let Ok(bytes) = read() else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(max_lines);
    lines[from..].join("\n")
}

async fn read_response_line(reader: &mut BufReader<UnixStream>) -> Result<String> {
    use std::io::ErrorKind;

    let mut bytes = Vec::new();
    match reader
        .take(MAX_RESPONSE_LINE as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await
    {
        Ok(_) => {}
        // A hangup is reported the same way as an empty answer.
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
            ) =>
        {
            return Ok(String::new());
        }
        Err(e) => return Err(Error::IpcError(format!("Read error: {}", e))),
    }
    if bytes.len() > MAX_RESPONSE_LINE && bytes.last() != Some(&b'\n') {
        return Err(Error::IpcError(format!(
            "Response line exceeds {MAX_RESPONSE_LINE} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|e| Error::IpcError(format!("Read error: {}", e)))
}

/// Turn one response line into a `Response`.
fn parse_response(line: &str, request_id: &str) -> Result<Response> {
    parse_part(line, request_id).map(|(response, _)| response)
}

/// Turn one response line into a `Response` and whether more lines follow it.
fn parse_part(line: &str, request_id: &str) -> Result<(Response, bool)> {
    if line.trim().is_empty() {
        return Err(Error::IpcError(
            "The daemon closed the connection without answering. It may be an older oxidepm \
             (restart it with `oxidepm kill`) or it may have crashed (see daemon.log)"
                .to_string(),
        ));
    }
    match serde_json::from_str::<ResponseEnvelope>(line.trim()) {
        Ok(envelope) => {
            if !envelope.request_id.is_empty() && envelope.request_id != request_id {
                return Err(Error::IpcError(format!(
                    "The daemon answered request {} instead of {}",
                    envelope.request_id, request_id
                )));
            }
            Ok((envelope.response, envelope.more))
        }
        // A daemon that refuses an envelope (or cannot parse one) answers in the old shape.
        Err(envelope_error) => serde_json::from_str::<Response>(line.trim())
            .map(|response| (response, false))
            .map_err(|_| Error::IpcError(format!("Invalid response: {}", envelope_error))),
    }
}

/// IPC Client for CLI communication with daemon
pub struct IpcClient {
    socket_path: PathBuf,
    daemon_bin: Option<PathBuf>,
    /// The actor claim sent when a call does not set its own.
    actor: Option<String>,
    fast_timeout: Duration,
    slow_timeout: Duration,
    stream_inactivity: Duration,
}

impl IpcClient {
    /// Create a new IPC client
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            daemon_bin: None,
            actor: None,
            fast_timeout: FAST_TIMEOUT,
            slow_timeout: SLOW_TIMEOUT,
            stream_inactivity: STREAM_INACTIVITY,
        }
    }

    /// Replace the inactivity timeout of a streamed answer ([`send_streaming`](Self::send_streaming)).
    pub fn with_stream_inactivity(mut self, timeout: Duration) -> Self {
        self.stream_inactivity = timeout;
        self
    }

    /// Replace the request timeouts (fast: status, show, logs, ...; slow: start, stop, ...).
    /// A request that is not answered in time fails with `Error::Timeout` (`E_TIMEOUT`).
    pub fn with_timeouts(mut self, fast: Duration, slow: Duration) -> Self {
        self.fast_timeout = fast;
        self.slow_timeout = slow;
        self
    }

    /// Start this `oxidepmd` binary when the daemon has to be auto-started, instead of
    /// looking next to the current executable.
    pub fn with_daemon_bin(mut self, path: impl Into<PathBuf>) -> Self {
        self.daemon_bin = Some(path.into());
        self
    }

    /// Send `claim` (`human:<label>` or `ai:<label>`) as the actor of every request that does not
    /// carry its own. It is only a claim: the daemon decides who is asking.
    pub fn with_actor(mut self, claim: Option<String>) -> Self {
        self.actor = claim;
        self
    }

    /// Whether a daemon answers on the socket right now. A socket file that nobody listens
    /// on (a daemon that died) does not count.
    pub fn is_daemon_running(&self) -> bool {
        std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok()
    }

    /// Connect to daemon (without auto-start)
    pub async fn connect(&self) -> Result<UnixStream> {
        if !self.socket_path.exists() {
            return Err(Error::DaemonNotRunning);
        }

        UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    Error::DaemonNotRunning
                }
                _ => Error::IpcConnectionFailed(e.to_string()),
            })
    }

    /// Connect to daemon, starting it if necessary (PM2 behavior)
    pub async fn connect_or_start(&self) -> Result<UnixStream> {
        match self.connect().await {
            Ok(stream) => Ok(stream),
            Err(Error::DaemonNotRunning) => {
                info!("Daemon not running, starting...");
                let mut daemon = self.start_daemon()?;

                // Wait for daemon to be ready (up to 5 seconds), noticing an early exit
                for i in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if let Ok(stream) = self.connect().await {
                        info!("Connected to daemon after {}ms", (i + 1) * 100);
                        return Ok(stream);
                    }
                    if let Ok(Some(status)) = daemon.child.try_wait() {
                        // Another daemon may have won a start race: if one answers, use it.
                        if let Ok(stream) = self.connect().await {
                            return Ok(stream);
                        }
                        // The daemon we started found the winner's lock: it is on its way up,
                        // keep waiting for its socket.
                        if status.code() != Some(EXIT_ALREADY_RUNNING) {
                            return Err(Error::IpcError(daemon.exit_report(status)));
                        }
                    }
                }

                Err(Error::IpcError(
                    "Daemon failed to start within timeout".to_string(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// Start the daemon process. Its stderr goes to `daemon.log` next to the socket, so a
    /// fatal startup error is not lost.
    fn start_daemon(&self) -> Result<StartedDaemon> {
        let daemon_exe = self.daemon_executable()?;
        info!("Starting daemon: {}", daemon_exe.display());

        let home = self
            .socket_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from);
        let (log_path, log_offset, stdout, stderr) = match &home {
            Some(home) => match open_daemon_log(home).and_then(|(path, offset, file)| {
                let second = file.try_clone()?;
                Ok((path, offset, file, second))
            }) {
                Ok((path, offset, out, err)) => {
                    (Some(path), offset, Stdio::from(out), Stdio::from(err))
                }
                Err(e) => {
                    warn!("Cannot open the daemon log in {}: {}", home.display(), e);
                    (None, 0, Stdio::null(), Stdio::null())
                }
            },
            None => (None, 0, Stdio::null(), Stdio::null()),
        };

        let mut command = Command::new(&daemon_exe);
        command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        if let Some(home) = &home {
            // The daemon must serve the socket this client connects to.
            command.env("OXIDEPM_HOME", home);
        }
        // Detach: a new session (and with it a new process group) so that Ctrl-C at this
        // terminal, or closing it, never reaches the daemon and its apps.
        // SAFETY: the closure only calls setsid(2), which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(std::io::Error::from)
            });
        }
        let child = command.spawn().map_err(|e| {
            Error::IpcError(format!(
                "Failed to start daemon {}: {}",
                daemon_exe.display(),
                e
            ))
        })?;

        Ok(StartedDaemon {
            child,
            log_path,
            log_offset,
        })
    }

    /// The daemon binary: the one given with [`with_daemon_bin`](Self::with_daemon_bin), else
    /// the absolute path in `OXIDEPM_DAEMON_BIN`, else `oxidepmd` next to this executable.
    /// `PATH` is never searched.
    fn daemon_executable(&self) -> Result<PathBuf> {
        if let Some(path) = &self.daemon_bin {
            return Ok(path.clone());
        }

        if let Some(value) = std::env::var_os(DAEMON_BIN_ENV).filter(|v| !v.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(Error::IpcError(format!(
                    "{} must be an absolute path, got {}",
                    DAEMON_BIN_ENV,
                    path.display()
                )));
            }
            return Ok(path);
        }

        let exe = std::env::current_exe()?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| Error::IpcError("Cannot determine executable directory".to_string()))?;
        let daemon_path = exe_dir.join("oxidepmd");
        if !daemon_path.exists() {
            return Err(Error::IpcError(format!(
                "oxidepmd not found at {}; install it next to oxidepm or set {} to its absolute path",
                daemon_path.display(),
                DAEMON_BIN_ENV
            )));
        }
        Ok(daemon_path)
    }

    /// Send a request and receive the response, starting the daemon first if it is not
    /// running. Only for commands that cannot do anything without a daemon (start,
    /// resurrect); everything else uses [`send`](Self::send).
    pub async fn send_autostart(&self, request: &Request) -> Result<Response> {
        let stream = self.connect_or_start().await?;
        self.exchange(stream, request, &CallOptions::default())
            .await
    }

    /// Send a request and receive the response. Never starts the daemon: returns
    /// `Error::DaemonNotRunning` when nothing is listening.
    pub async fn send(&self, request: &Request) -> Result<Response> {
        self.send_with(request, &CallOptions::default()).await
    }

    /// Like [`send`](Self::send), with envelope options (`reveal`, `actor`).
    pub async fn send_with(&self, request: &Request, options: &CallOptions) -> Result<Response> {
        let stream = self.connect().await?;
        self.exchange(stream, request, options).await
    }

    /// Send a request whose answer is a stream (a relayed adapter verb): the daemon writes lines
    /// with `more` set, then one without. `on_part` sees each part as it arrives; the last
    /// response is returned. The timeout is one of inactivity that every line resets, not a limit
    /// on the whole call, so a long verb that keeps reporting finishes. Never starts the daemon.
    pub async fn send_streaming(
        &self,
        request: &Request,
        options: &CallOptions,
        mut on_part: impl FnMut(&Response),
    ) -> Result<Response> {
        let mut stream = self.connect().await?;
        let inactivity = self.stream_inactivity;
        let silent = || {
            Error::Timeout(format!(
                "the daemon sent nothing for {} s",
                inactivity.as_secs_f64()
            ))
        };
        let request_id = tokio::time::timeout(
            inactivity,
            self.write_envelope(&mut stream, request, options, PROTOCOL_VERSION),
        )
        .await
        .map_err(|_| silent())??;

        let mut reader = BufReader::new(stream);
        loop {
            let line = tokio::time::timeout(inactivity, read_response_line(&mut reader))
                .await
                .map_err(|_| silent())??;
            let (response, more) = parse_part(&line, &request_id)?;
            if !more {
                return Ok(response);
            }
            on_part(&response);
        }
    }

    async fn exchange(
        &self,
        stream: UnixStream,
        request: &Request,
        options: &CallOptions,
    ) -> Result<Response> {
        let limit = if is_slow(request) {
            self.slow_timeout
        } else {
            self.fast_timeout
        };
        let timed_out = || {
            Error::Timeout(format!(
                "the daemon did not answer within {} s",
                limit.as_secs_f64()
            ))
        };
        let first = tokio::time::timeout(
            limit,
            self.round_trip(stream, request, options, PROTOCOL_VERSION),
        )
        .await
        .map_err(|_| timed_out())??;
        // A daemon of an older protocol refuses the number we send. `hello`, `ping` and `kill`
        // are the requests that must work across versions (a new CLI has to be able to stop an
        // old daemon after an update): ask again, as an older client would, on a new connection
        // (the daemon closes the one it refused on).
        if request.crosses_versions()
            && !options.dry_run
            && matches!(&first, Response::Error { code, .. } if *code == ErrorCode::ProtocolVersion)
        {
            debug!(
                "The daemon refused protocol {}; asking again with {}",
                PROTOCOL_VERSION, PROTOCOL_CROSS_VERSION_MIN
            );
            let stream = self.connect().await?;
            return tokio::time::timeout(
                limit,
                self.round_trip(stream, request, options, PROTOCOL_CROSS_VERSION_MIN),
            )
            .await
            .map_err(|_| timed_out())?;
        }
        Ok(first)
    }

    async fn round_trip(
        &self,
        mut stream: UnixStream,
        request: &Request,
        options: &CallOptions,
        protocol: u32,
    ) -> Result<Response> {
        let request_id = self
            .write_envelope(&mut stream, request, options, protocol)
            .await?;
        let mut reader = BufReader::new(stream);
        let line = read_response_line(&mut reader).await?;
        let response = parse_response(&line, &request_id)?;
        debug!("Received response: {:?}", response);
        Ok(response)
    }

    async fn write_envelope(
        &self,
        stream: &mut UnixStream,
        request: &Request,
        options: &CallOptions,
        protocol: u32,
    ) -> Result<String> {
        let mut envelope = Envelope::new(next_request_id(), request.clone());
        envelope.protocol = protocol;
        envelope.reveal = options.reveal;
        envelope.dry_run = options.dry_run;
        envelope.actor = options.actor.clone().or_else(|| self.actor.clone());

        let mut json = serde_json::to_string(&envelope)?;
        json.push('\n');

        stream
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::IpcError(format!("Write error: {}", e)))?;

        stream
            .flush()
            .await
            .map_err(|e| Error::IpcError(format!("Flush error: {}", e)))?;

        debug!("Sent request: {:?}", request);

        Ok(envelope.request_id)
    }

    /// Ping the daemon
    pub async fn ping(&self) -> Result<bool> {
        match self.send(&Request::Ping).await {
            Ok(Response::Pong) => Ok(true),
            Ok(_) => Ok(false),
            Err(Error::DaemonNotRunning) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn startup_log_tail_is_bounded_and_respects_the_offset() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        let recent = "recent";
        let long = "x".repeat(70 * 1024);
        std::fs::write(&log, format!("old\n{long}\n{recent}\n")).unwrap();
        let tail = log_tail(&log, 4, 20);
        assert!(tail.len() <= 64 * 1024);
        assert!(tail.ends_with(recent));
        assert_eq!(log_tail(&log, (5 + long.len()) as u64, 20), recent);
    }

    #[tokio::test]
    async fn both_client_modes_reject_an_oversized_response_line() {
        use tokio::net::UnixListener;
        let dir = tempdir().unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let chunk = [b' '; 64 * 1024];
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream.read_line(&mut request).await.unwrap();
                for _ in 0..=MAX_RESPONSE_LINE / chunk.len() {
                    if stream.get_mut().write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            }
        });

        let client = IpcClient::new(path);
        let one = client.send(&Request::Ping).await.unwrap_err();
        let streamed = client
            .send_streaming(&Request::Ping, &CallOptions::default(), |_| {})
            .await
            .unwrap_err();
        peer.await.unwrap();
        for err in [one, streamed] {
            assert!(err.to_string().contains("Response line exceeds"), "{err}");
        }
    }

    #[test]
    fn test_client_creation() {
        let client = IpcClient::new(PathBuf::from("/tmp/test.sock"));
        assert_eq!(client.socket_path, PathBuf::from("/tmp/test.sock"));
    }

    #[tokio::test]
    async fn test_send_without_daemon_does_not_start_one() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("nonexistent.sock");

        let client = IpcClient::new(socket_path.clone());
        let result = client.send(&Request::Ping).await;

        assert!(matches!(result, Err(Error::DaemonNotRunning)));
        assert!(!socket_path.exists());
        assert!(!client.ping().await.unwrap());
    }

    #[test]
    fn log_tail_shows_only_new_lines_and_caps_them() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "old line\n").unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        let mut text = std::fs::read_to_string(&path).unwrap();
        for i in 0..30 {
            text.push_str(&format!("new {i}\n\n"));
        }
        std::fs::write(&path, text).unwrap();

        let tail = log_tail(&path, offset, 3);
        assert_eq!(tail, "new 27\nnew 28\nnew 29");
        assert!(!log_tail(&path, offset, 100).contains("old line"));
        // a truncated log is read from the start
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(log_tail(&path, offset + 1000, 5), "fresh");
        assert_eq!(log_tail(&dir.path().join("missing"), 0, 5), "");
    }

    #[test]
    fn open_daemon_log_creates_a_private_log_and_reports_its_size() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let (path, offset, _file) = open_daemon_log(&home).unwrap();
        assert_eq!((path.clone(), offset), (home.join("daemon.log"), 0));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::write(&path, "abc").unwrap();
        assert_eq!(open_daemon_log(&home).unwrap().1, 3);
    }

    #[tokio::test]
    async fn test_connect_no_daemon() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("nonexistent.sock");

        let client = IpcClient::new(socket_path);
        let result = client.connect().await;

        assert!(matches!(result, Err(Error::DaemonNotRunning)));
    }
}
