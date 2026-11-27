//! IPC Server - Unix socket server for daemon

use oxidepm_core::actor::{ActorClass, ActorId, Via};
use oxidepm_core::{Error, Result};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use crate::protocol::{
    Envelope, Request, Response, ResponseEnvelope, PROTOCOL_CROSS_VERSION_MIN, PROTOCOL_MIN,
    PROTOCOL_VERSION,
};
use oxidepm_core::ErrorCode;

/// Longest accepted request line on the main socket (10 MiB, without the newline).
pub const MAX_LINE: usize = 10 * 1024 * 1024;

/// A connection on the main socket that sends nothing for this long is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest accepted request line on the agent socket (1 MiB).
pub const AGENT_MAX_LINE: usize = 1024 * 1024;

/// Concurrent connections the agent socket serves; the next one is closed at once.
pub const AGENT_MAX_CONNECTIONS: usize = 32;

/// An agent connection that sends nothing for this long is closed.
pub const AGENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a write to an agent connection may take.
pub const AGENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What one listener allows its connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// Longest request line, without the newline.
    pub max_line: usize,
    /// Concurrent connections; `None` is no cap.
    pub max_connections: Option<usize>,
    /// A connection that sends nothing for this long is closed.
    pub idle_timeout: Duration,
    /// How long a write to a client may take; `None` waits as long as the client does.
    pub write_timeout: Option<Duration>,
}

impl ServerLimits {
    /// The main socket allows longer requests and more simultaneous callers.
    pub const MAIN: ServerLimits = ServerLimits {
        max_line: MAX_LINE,
        max_connections: Some(256),
        idle_timeout: IDLE_TIMEOUT,
        write_timeout: Some(Duration::from_secs(30)),
    };

    /// The agent socket: tighter on every count.
    pub const AGENT: ServerLimits = ServerLimits {
        max_line: AGENT_MAX_LINE,
        max_connections: Some(AGENT_MAX_CONNECTIONS),
        idle_timeout: AGENT_IDLE_TIMEOUT,
        write_timeout: Some(AGENT_WRITE_TIMEOUT),
    };
}

/// One of the connections a listener serves; frees its place when dropped.
struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How long `bind` waits for an existing socket to answer before it counts as live.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Makes the names of staging directories unique within the process.
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What `bind` finds at the socket path.
enum PathState {
    Free,
    Live,
    Stale(SocketFile),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketFile {
    dev: u64,
    ino: u64,
}

impl SocketFile {
    fn new(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    fn matches(self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.file_type().is_socket() && self == Self::new(&meta))
    }
}

fn socket_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

const SOCKET_DIR_LOCK_FLAGS: libc::c_int = libc::LOCK_EX | libc::LOCK_NB;
const SOCKET_DIR_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_DIR_LOCK_RETRY: Duration = Duration::from_millis(20);

// Serialize publication and stale-file cleanup by cooperating listeners in this directory.
// The lock is held for filesystem operations only, never across an await.
fn try_lock_socket_dir(dir: &File) -> io::Result<bool> {
    // SAFETY: dir owns the open descriptor throughout the flock.
    if unsafe { libc::flock(dir.as_raw_fd(), SOCKET_DIR_LOCK_FLAGS) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(false),
        _ => Err(error),
    }
}

async fn lock_socket_dir(path: &Path) -> io::Result<File> {
    let parent = socket_parent(path);
    let dir = File::open(parent)?;
    tokio::time::timeout(SOCKET_DIR_LOCK_TIMEOUT, async {
        loop {
            if try_lock_socket_dir(&dir)? {
                return Ok(dir);
            }
            tokio::time::sleep(SOCKET_DIR_LOCK_RETRY).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for the socket directory lock at {}",
                parent.display()
            ),
        )
    })?
}

async fn probe(path: &Path) -> Result<PathState> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PathState::Free),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_socket() {
        return Err(Error::IpcError(format!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        )));
    }
    match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_)) => Ok(PathState::Live),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Ok(PathState::Stale(SocketFile::new(&meta)))
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(PathState::Free),
        Ok(Err(e)) => Err(Error::IpcError(format!(
            "cannot tell whether {} is in use: {}",
            path.display(),
            e
        ))),
        // a listener whose backlog is full is very much alive
        Err(_) => Ok(PathState::Live),
    }
}

/// Bind in a private staging directory and link into place after chmod, without
/// replacing a file that another caller published first.
fn bind_private(path: &Path) -> io::Result<(UnixListener, SocketFile)> {
    let parent = socket_parent(path);
    let stage = loop {
        let counter = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(".{:x}-{counter:x}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&stage) {
            Ok(()) => break stage,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };

    let result = (|| {
        let staged = stage.join("s");
        let listener = match UnixListener::bind(&staged) {
            #[cfg(target_os = "linux")]
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                // The final address can fit even when the staging address is too long.
                let dir = File::open(&stage)?;
                UnixListener::bind(format!("/proc/self/fd/{}/s", dir.as_raw_fd()))?
            }
            result => result?,
        };
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
        let socket = SocketFile::new(&fs::symlink_metadata(&staged)?);
        fs::hard_link(&staged, path)?;
        Ok((listener, socket))
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

/// IPC Server for daemon
pub struct IpcServer {
    socket_path: PathBuf,
    socket_file: SocketFile,
    listener: UnixListener,
    limits: ServerLimits,
    /// Connections being served right now.
    live: Arc<AtomicUsize>,
}

impl IpcServer {
    /// Bind to a Unix socket. A socket that still answers is never removed
    /// (`Error::DaemonAlreadyRunning`); a stale one is replaced. The socket is never visible
    /// at `socket_path` with a mode other than 0600 (see `bind_private`).
    pub async fn bind(socket_path: &Path) -> Result<Self> {
        Self::bind_with(socket_path, ServerLimits::MAIN).await
    }

    /// [`bind`](Self::bind), with the limits of this listener's connections.
    pub async fn bind_with(socket_path: &Path, limits: ServerLimits) -> Result<Self> {
        std::os::unix::net::SocketAddr::from_pathname(socket_path)?;
        if let Some(parent) = socket_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }

        let (listener, socket_file) = loop {
            let state = probe(socket_path).await?;
            let _lock = lock_socket_dir(socket_path).await?;
            match state {
                PathState::Live => return Err(Error::DaemonAlreadyRunning),
                PathState::Stale(file) if file.matches(socket_path) => {
                    info!("Removing stale socket {}", socket_path.display());
                    fs::remove_file(socket_path)?;
                }
                PathState::Stale(_) => continue,
                PathState::Free => {}
            }

            match bind_private(socket_path) {
                Ok(bound) => break bound,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::IpcError(format!("Failed to bind socket: {}", e))),
            }
        };

        info!("IPC server listening on {}", socket_path.display());

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            socket_file,
            listener,
            limits,
            live: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Accept a new connection from this user. Connections of any other user are closed
    /// without a word (defence in depth: the directory is 0700 and the socket 0600), and so is
    /// every connection beyond the listener's cap.
    pub async fn accept(&self) -> Result<IpcConnection> {
        let ours = nix::unistd::geteuid().as_raw();
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| Error::IpcError(format!("Accept failed: {}", e)))?;

            match stream.peer_cred() {
                Ok(cred) if cred.uid() == ours => {
                    let slot = match self.limits.max_connections {
                        Some(max) => {
                            if self.live.fetch_add(1, Ordering::SeqCst) >= max {
                                self.live.fetch_sub(1, Ordering::SeqCst);
                                warn!("Dropping IPC connection: {} are being served already", max);
                                continue;
                            }
                            Some(ConnSlot(Arc::clone(&self.live)))
                        }
                        None => None,
                    };
                    debug!("Accepted IPC connection");
                    let mut conn = IpcConnection::with_limits(stream, Some(cred), self.limits);
                    conn.slot = slot;
                    return Ok(conn);
                }
                Ok(cred) => warn!("Dropping IPC connection from uid {}", cred.uid()),
                Err(e) => warn!("Dropping IPC connection without credentials: {}", e),
            }
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let Ok(dir) = File::open(socket_parent(&self.socket_path)) else {
            return;
        };
        // A subsequent bind can clean up the stale socket if the directory is busy here.
        if !matches!(try_lock_socket_dir(&dir), Ok(true)) {
            return;
        }
        if self.socket_file.matches(&self.socket_path) {
            if let Err(e) = fs::remove_file(&self.socket_path) {
                error!("Failed to remove socket file: {}", e);
            }
        }
    }
}

/// Why the next request could not be read. All of them end the connection.
#[derive(Debug)]
pub enum ReadError {
    /// More than the listener's line limit in bytes without a newline.
    TooLarge { limit: usize },
    /// Nothing arrived within the idle timeout.
    Idle,
    /// The socket failed.
    Io(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::TooLarge { limit } if limit % (1024 * 1024) == 0 => {
                write!(
                    f,
                    "Request too large (limit {} MiB per line)",
                    limit / (1024 * 1024)
                )
            }
            ReadError::TooLarge { limit } => {
                write!(f, "Request too large (limit {} bytes per line)", limit)
            }
            ReadError::Idle => write!(f, "Connection idle for too long"),
            ReadError::Io(e) => write!(f, "Read error: {}", e),
        }
    }
}

impl std::error::Error for ReadError {}

/// An answer to a line that is not a usable request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// In the old shape (no envelope), for a client that does not speak envelopes.
    Bare(Response),
    Envelope(ResponseEnvelope),
}

/// What one line on the wire turned out to be.
#[derive(Debug)]
pub enum Incoming {
    /// A request to handle.
    Request(Envelope),
    /// Not a usable request: send `reply`; close the connection afterwards if `close`.
    Reject { reply: Reply, close: bool },
}

impl Incoming {
    fn reject(request_id: &str, code: ErrorCode, message: String, close: bool) -> Self {
        Incoming::Reject {
            reply: Reply::Envelope(ResponseEnvelope::new(
                request_id,
                Response::error(code, message),
            )),
            close,
        }
    }
}

/// Classify one request line. A line without a `protocol` field is a bare request from an
/// old client and is refused in the old shape; an envelope outside the supported protocol
/// range is refused without looking at its request (a newer client may use requests this
/// build has never heard of), except that `hello`, `ping` and `kill` are accepted from an older
/// client down to [`PROTOCOL_CROSS_VERSION_MIN`].
pub fn classify_line(line: &str) -> Incoming {
    classify_line_with_limit(line, None)
}

fn classify_line_with_limit(line: &str, claim_limit: Option<usize>) -> Incoming {
    let value: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(e) => {
            return Incoming::reject(
                "",
                ErrorCode::BadRequest,
                format!("Invalid request: {}", e),
                false,
            )
        }
    };
    if let Some(limit) = claim_limit {
        let claim = value.get("actor").and_then(|actor| actor.as_str());
        if ActorId::from_connection(Via::Main, claim).class == ActorClass::Ai {
            return Incoming::reject(
                "",
                ErrorCode::RequestTooLarge,
                ReadError::TooLarge { limit }.to_string(),
                true,
            );
        }
    }
    let request_id = value
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let Some(protocol) = value.get("protocol") else {
        // Not an envelope. A well-formed old-style request gets the old-style refusal.
        if serde_json::from_value::<crate::protocol::Request>(value).is_ok() {
            return Incoming::Reject {
                reply: Reply::Bare(Response::error(
                    ErrorCode::ProtocolVersion,
                    format!(
                        "client speaks an older protocol, daemon speaks {}..{}; upgrade oxidepm / restart the daemon with `oxidepm kill`",
                        PROTOCOL_MIN, PROTOCOL_VERSION
                    ),
                )),
                close: true,
            };
        }
        return Incoming::reject(
            &request_id,
            ErrorCode::BadRequest,
            "Invalid request: not a request".to_string(),
            false,
        );
    };

    let protocol = protocol.as_u64().unwrap_or(0);
    // `hello`, `ping` and `kill` are understood across versions (a new client must be able to
    // ask an older daemon to stop); everything else needs the supported range
    let crosses = protocol >= PROTOCOL_CROSS_VERSION_MIN as u64
        && protocol < PROTOCOL_MIN as u64
        && value
            .get("request")
            .cloned()
            .and_then(|request| serde_json::from_value::<Request>(request).ok())
            .is_some_and(|request| request.crosses_versions());
    if !crosses && (protocol < PROTOCOL_MIN as u64 || protocol > PROTOCOL_VERSION as u64) {
        return Incoming::reject(
            &request_id,
            ErrorCode::ProtocolVersion,
            format!(
                "client speaks protocol {}, daemon speaks {}..{}; upgrade oxidepm / restart the daemon with `oxidepm kill`",
                protocol, PROTOCOL_MIN, PROTOCOL_VERSION
            ),
            true,
        );
    }

    match serde_json::from_value::<Envelope>(value) {
        Ok(envelope) => Incoming::Request(envelope),
        Err(e) => Incoming::reject(
            &request_id,
            ErrorCode::BadRequest,
            format!("Invalid request: {}", e),
            false,
        ),
    }
}

/// Single IPC connection. The stream is split once; the buffered reader lives as long as the
/// connection, so bytes that arrive behind one request line are kept for the next call.
pub struct IpcConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer: Option<tokio::net::unix::UCred>,
    limits: ServerLimits,
    /// Keeps this connection counted against the listener's cap while it lives.
    slot: Option<ConnSlot>,
}

impl IpcConnection {
    pub fn new(stream: UnixStream, peer: Option<tokio::net::unix::UCred>) -> Self {
        Self::with_limits(stream, peer, ServerLimits::MAIN)
    }

    /// A connection that holds to `limits` (the line length, the idle and the write timeout; the
    /// connection cap belongs to the listener).
    pub fn with_limits(
        stream: UnixStream,
        peer: Option<tokio::net::unix::UCred>,
        limits: ServerLimits,
    ) -> Self {
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
            peer,
            limits,
            slot: None,
        }
    }

    /// Replace the idle timeout (for tests; the default is [`IDLE_TIMEOUT`]).
    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.limits.idle_timeout = timeout;
    }

    /// Reduce the maximum line length when the daemon identifies a peer.
    pub fn restrict_line_size(&mut self, max_line: usize) {
        self.limits.max_line = self.limits.max_line.min(max_line);
    }

    /// The uid of the process on the other end, when the kernel told us.
    pub fn peer_uid(&self) -> Option<u32> {
        self.peer.as_ref().map(|c| c.uid())
    }

    /// The pid of the process on the other end, when the kernel told us.
    pub fn peer_pid(&self) -> Option<i32> {
        self.peer.as_ref().and_then(|c| c.pid())
    }

    /// Read the next line and classify it. `Ok(None)` is a closed connection.
    pub async fn read_request(&mut self) -> std::result::Result<Option<Incoming>, ReadError> {
        self.read_request_with_claim_limit(None).await
    }

    /// Read an unattributed main-socket request, with a lower limit for a non-human claim.
    /// The claim may appear anywhere in the line, so the listener limit bounds the read
    /// and the claim limit is checked before deserializing the request itself.
    pub async fn read_request_with_claim_limit(
        &mut self,
        claim_limit: Option<usize>,
    ) -> std::result::Result<Option<Incoming>, ReadError> {
        let mut buf = Vec::new();
        let read = {
            let mut limited = (&mut self.reader).take(self.limits.max_line as u64 + 1);
            tokio::time::timeout(
                self.limits.idle_timeout,
                limited.read_until(b'\n', &mut buf),
            )
            .await
        };
        match read {
            Err(_) => Err(ReadError::Idle),
            Ok(Err(e)) => Err(ReadError::Io(e.to_string())),
            Ok(Ok(0)) => Ok(None), // Connection closed
            Ok(Ok(_)) => {
                let len = buf.len() - usize::from(buf.last() == Some(&b'\n'));
                if len > self.limits.max_line {
                    return Err(ReadError::TooLarge {
                        limit: self.limits.max_line,
                    });
                }
                let incoming = classify_line_with_limit(
                    &String::from_utf8_lossy(&buf),
                    claim_limit.filter(|&limit| len > limit),
                );
                if let Incoming::Request(envelope) = &incoming {
                    debug!("Received IPC request: {}", envelope.request.name());
                }
                Ok(Some(incoming))
            }
        }
    }

    /// Send a response envelope.
    pub async fn send_response(&mut self, response: &ResponseEnvelope) -> Result<()> {
        self.send_json(serde_json::to_string(response)?).await?;
        debug!("Sent IPC response");
        Ok(())
    }

    /// Send a response in the old shape, without an envelope.
    pub async fn send_bare(&mut self, response: &Response) -> Result<()> {
        self.send_json(serde_json::to_string(response)?).await
    }

    /// Send the answer to a rejected line.
    pub async fn send_reply(&mut self, reply: &Reply) -> Result<()> {
        match reply {
            Reply::Bare(response) => self.send_bare(response).await,
            Reply::Envelope(envelope) => self.send_response(envelope).await,
        }
    }

    async fn send_json(&mut self, json: String) -> Result<()> {
        write_line(&mut self.writer, json, self.limits.write_timeout).await
    }

    /// The two halves of the connection for a streamed answer, borrowed apart: one writes the parts
    /// (each with its own write timeout), the other notices that the client has gone.
    pub fn stream_halves(&mut self) -> (PartWriter<'_>, PeerWatch<'_>) {
        (
            PartWriter {
                writer: &mut self.writer,
            },
            PeerWatch {
                reader: &mut self.reader,
            },
        )
    }
}

/// Writes the lines of a streamed answer.
pub struct PartWriter<'a> {
    writer: &'a mut OwnedWriteHalf,
}

impl PartWriter<'_> {
    /// Write one line; fails if the client does not take it within `timeout` (a stopped or
    /// suspended client must not hold the invocation for ever).
    pub async fn send(&mut self, response: &ResponseEnvelope, timeout: Duration) -> Result<()> {
        write_line(self.writer, serde_json::to_string(response)?, Some(timeout)).await
    }
}

/// Watches the read side of a connection while an answer is being produced.
pub struct PeerWatch<'a> {
    reader: &'a mut BufReader<OwnedReadHalf>,
}

impl PeerWatch<'_> {
    /// Resolves when the client has closed its side (or the socket failed). Cancel safe. A client
    /// that sends another request meanwhile keeps its bytes buffered for the next read.
    pub async fn gone(&mut self) {
        if !matches!(self.reader.fill_buf().await, Ok(bytes) if !bytes.is_empty()) {
            return;
        }
        // Readability stays set while a request is buffered. Check the independently
        // recorded close notification without consuming that request or spinning.
        loop {
            match self
                .reader
                .get_ref()
                .ready(tokio::io::Interest::READABLE)
                .await
            {
                Ok(ready) if !ready.is_read_closed() => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                _ => return,
            }
        }
    }
}

async fn write_line(
    writer: &mut OwnedWriteHalf,
    mut json: String,
    timeout: Option<Duration>,
) -> Result<()> {
    json.push('\n');

    let write = async {
        writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::IpcError(format!("Write error: {}", e)))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::IpcError(format!("Flush error: {}", e)))
    };
    match timeout {
        None => write.await,
        Some(limit) => tokio::time::timeout(limit, write)
            .await
            .unwrap_or_else(|_| {
                Err(Error::IpcError(format!(
                    "Write error: the client did not read the answer within {} s",
                    limit.as_secs_f64()
                )))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{Flock, FlockArg};
    use tempfile::tempdir;

    fn hold_socket_directory_lock(dir: &Path) -> Flock<File> {
        Flock::lock(File::open(dir).unwrap(), FlockArg::LockExclusive).unwrap()
    }

    #[tokio::test]
    async fn bind_times_out_when_the_socket_directory_is_locked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s");
        let lock = hold_socket_directory_lock(dir.path());

        let runtime = tokio::runtime::Handle::current();
        let mut bind =
            tokio::task::spawn_blocking(move || runtime.block_on(IpcServer::bind(&path)));
        let bound = SOCKET_DIR_LOCK_TIMEOUT + Duration::from_secs(2);
        let outcome = tokio::time::timeout(bound, &mut bind).await;
        drop(lock);
        if outcome.is_err() {
            // Release the lock and join even if bind waits for a blocking flock.
            drop(bind.await.unwrap());
        }
        let result = outcome.expect("bind exceeded the time limit").unwrap();
        assert!(matches!(result, Err(Error::IoError(e)) if e.kind() == io::ErrorKind::TimedOut));
    }

    #[tokio::test]
    async fn dropping_the_server_returns_while_the_socket_directory_is_locked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s");
        let server = IpcServer::bind(&path).await.unwrap();
        let lock = hold_socket_directory_lock(dir.path());

        let mut cleanup = tokio::task::spawn_blocking(move || drop(server));
        let outcome = tokio::time::timeout(SOCKET_DIR_LOCK_TIMEOUT, &mut cleanup).await;
        drop(lock);
        if outcome.is_err() {
            cleanup.await.unwrap();
        }
        outcome
            .expect("socket cleanup exceeded the time limit")
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_bind_cannot_replace_an_entry_created_before_publication() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s");
        std::fs::write(&path, "existing entry").unwrap();

        assert!(bind_private(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing entry");
    }

    #[tokio::test]
    async fn dropping_a_listener_leaves_a_replacement_socket_in_place() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s");
        let first = IpcServer::bind(&path).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let next = IpcServer::bind(&path).await.unwrap();
        drop(first);

        let _client = UnixStream::connect(&path).await.unwrap();
        let _connection = next.accept().await.unwrap();
    }

    #[tokio::test]
    async fn binding_creates_a_private_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let parent = dir.path().join("home");
        let _server = IpcServer::bind(&parent.join("s")).await.unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn binding_works_when_only_the_final_socket_path_fits() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir().unwrap();
        let length = 104 - dir.path().as_os_str().as_bytes().len() - 1;
        let path = dir.path().join("x".repeat(length)).join("s");
        let server = IpcServer::bind(&path).await.unwrap();
        let _client = UnixStream::connect(&path).await.unwrap();
        let _connection = server.accept().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn binding_rejects_a_final_address_that_cannot_be_connected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x".repeat(110)).join("s");
        assert!(IpcServer::bind(&path).await.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_main_listener_has_connection_and_write_limits() {
        assert_eq!(ServerLimits::MAIN.max_connections, Some(256));
        assert_eq!(
            ServerLimits::MAIN.write_timeout,
            Some(Duration::from_secs(30))
        );
    }

    #[tokio::test]
    async fn peer_watch_notices_hangup_behind_buffered_input() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = IpcConnection::new(server, None);
        let request = serde_json::to_string(&Envelope::new("ping", Request::Ping)).unwrap() + "\n";
        client.write_all(request.as_bytes()).await.unwrap();

        let (_, mut watch) = server.stream_halves();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), watch.gone())
                .await
                .is_err()
        );
        assert!(matches!(
            server.read_request().await.unwrap(),
            Some(Incoming::Request(_))
        ));

        client.write_all(request.as_bytes()).await.unwrap();
        let (_, mut watch) = server.stream_halves();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), watch.gone())
                .await
                .is_err()
        );
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), watch.gone())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_server_bind() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        assert!(socket_path.exists());

        drop(server);
        assert!(!socket_path.exists());
    }
}
