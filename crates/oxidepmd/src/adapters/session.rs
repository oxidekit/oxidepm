//! Sessions: a verb that owns the terminal, run by the daemon.
//!
//! The CLI cannot execute an adapter: the sandbox, the supervision and the audit trail are the
//! daemon's, and they must hold for the monitor as much as for any other adapter. So the daemon
//! starts the adapter itself and the CLI hands over its standard input and output as open
//! descriptors (`oxidepm_ipc::session` describes the lines). The daemon relays them through
//! private session descriptors and bridges the adapter's protocol connection separately.
//!
//! What is checked, in the order it happens: the policy (a person, or an `ai` claim for a verb of
//! an adapter marked `ai_driven`); the verb is a session verb of a tool adapter; the working
//! directory; at most [`MAX_SESSIONS`] sessions at a time. The socket lives in a directory only
//! the user can enter, under a path shorter than 100 bytes; the CLI's attach line carries a
//! single-use token valid for 60 seconds, and the adapter gets a second token in its environment
//! with the same lifetime. The path of the executable comes from the daemon's record of the
//! adapter and is checked against its digest like every start; nothing the CLI says can change
//! it. A session ends when the verb returns, when the CLI closes its connection, when the adapter
//! stops, when the adapter is disabled or the daemon shuts down, or after 12 hours.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::libc;
use oxidepm_adapter_proto::messages::{ActorView, InvokeParams};
use oxidepm_adapter_proto::Kind;
use oxidepm_core::{Error, ErrorCode, Paths, Result};
use oxidepm_ipc::session::{
    peer_uid, recv_with_fds, ClientLine, ServerLine, SessionOffer, ATTACH_FDS, MAX_LINE,
    MAX_SESSIONS, MAX_SESSION_SECS, MAX_SOCKET_PATH, TOKEN_SECS,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{unix::AsyncFd, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Interest};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::entry::{InvokeCall, SessionCall};
use super::host::AdapterHost;
use super::invoke::{check_arguments, ResolvedVerb};

/// A connection that sends a wrong token this many times ends the session.
const MAX_BAD_ATTACHES: u32 = 5;
/// How long a connection may take to send its first line.
const FIRST_LINE_WAIT: Duration = Duration::from_secs(5);
/// How long the closing line may take to reach the CLI.
const CLOSING_LINE_WAIT: Duration = Duration::from_secs(5);
/// A helper can keep the private output open. Bound any drain after the adapter ends.
const TERMINAL_DRAIN_WAIT: Duration = Duration::from_millis(250);
/// How long a cancelled session is given to end before it is abandoned.
const END_WAIT: Duration = Duration::from_secs(20);
/// The adapter's first line: `{"session":"adapter","token":"..."}`.
const ADAPTER_HELLO_MAX: usize = 256;

/// What is asked of a session, after the checks.
pub struct SessionRun {
    pub argv: Vec<String>,
    /// The checked working directory (`$CWD` of the profile).
    pub cwd: PathBuf,
    pub actor: ActorView,
    /// The process may write only to the adapter's state directory (an `ai` claim opened it).
    pub restrict_writes: bool,
}

/// Only the relay owns the CLI's descriptors. The adapter and its helpers get a private
/// pseudo-terminal for a tty and private pipes otherwise.
struct TerminalRelay {
    terminal: (OwnedFd, OwnedFd),
    input: (RelayFd, RelayFd),
    output: (RelayFd, RelayFd),
    settings: [Option<TerminalSettings>; 2],
    window: Option<TerminalWindow>,
    tty_output: bool,
}

impl TerminalRelay {
    fn new((input, output): (OwnedFd, OwnedFd)) -> io::Result<Self> {
        let settings = [
            TerminalSettings::save(&input)?,
            TerminalSettings::save(&output)?,
        ];
        let input = RelayFd::from_client(input, Interest::READABLE)?;
        let output = RelayFd::from_client(output, Interest::WRITABLE)?;
        let mut window = None;
        let mut slave = None;
        if let Some(tty) = settings.iter().flatten().next() {
            let (master, new_slave) = private_terminal(&tty.original)?;
            let terminal_window = TerminalWindow {
                source: tty.fd.try_clone()?,
                master,
            };
            terminal_window.update();
            window = Some(terminal_window);
            slave = Some(new_slave);
        }
        let (adapter_input, send_input) = match (&settings[0], &window, &slave) {
            (Some(_), Some(window), Some(slave)) => (
                slave.try_clone()?,
                RelayFd::pollable(window.master.try_clone()?.into(), Interest::WRITABLE)?,
            ),
            _ => {
                let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
                (read, RelayFd::private_pipe(write, Interest::WRITABLE)?)
            }
        };
        let (adapter_output, read_output) = match (&settings[1], &window, &slave) {
            (Some(_), Some(window), Some(slave)) => (
                slave.try_clone()?,
                RelayFd::pollable(window.master.try_clone()?.into(), Interest::READABLE)?,
            ),
            _ => {
                let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
                (write, RelayFd::private_pipe(read, Interest::READABLE)?)
            }
        };
        for tty in settings.iter().flatten() {
            tty.relay_mode()?;
        }
        Ok(Self {
            terminal: (adapter_input, adapter_output),
            input: (input, send_input),
            output: (read_output, output),
            tty_output: settings[1].is_some(),
            settings,
            window,
        })
    }
}

struct TerminalSettings {
    fd: OwnedFd,
    original: libc::termios,
}

impl TerminalSettings {
    fn save(fd: &OwnedFd) -> io::Result<Option<Self>> {
        // SAFETY: the ioctl writes into a termios struct when the descriptor is a tty.
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut original) } == 0 {
            return Ok(Some(Self {
                fd: fd.try_clone()?,
                original,
            }));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTTY) {
            Ok(None)
        } else {
            Err(error)
        }
    }

    fn relay_mode(&self) -> io::Result<()> {
        let mut settings = self.original;
        // SAFETY: settings is an initialized termios for this terminal.
        unsafe { libc::cfmakeraw(&mut settings) };
        // Keep the caller's terminal signals, including the CLI's interrupt handler.
        settings.c_lflag |= self.original.c_lflag & libc::ISIG;
        nix::errno::Errno::result(unsafe {
            libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &settings)
        })?;
        Ok(())
    }
}

impl Drop for TerminalSettings {
    fn drop(&mut self) {
        // SAFETY: restore the settings saved from this terminal.
        unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &self.original) };
    }
}

fn private_terminal(settings: &libc::termios) -> io::Result<(OwnedFd, OwnedFd)> {
    use nix::errno::Errno;

    // SAFETY: open a fresh PTY and use its master to obtain the slave, both close-on-exec.
    unsafe {
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        let master =
            OwnedFd::from_raw_fd(Errno::result(libc::posix_openpt(flags | libc::O_NONBLOCK))?);
        Errno::result(libc::grantpt(master.as_raw_fd()))?;
        Errno::result(libc::unlockpt(master.as_raw_fd()))?;
        let slave = OwnedFd::from_raw_fd(Errno::result(libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCGPTPEER,
            flags,
        ))?);
        Errno::result(libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, settings))?;
        Ok((master, slave))
    }
}

struct TerminalWindow {
    source: OwnedFd,
    master: OwnedFd,
}

impl TerminalWindow {
    fn update(&self) {
        // SAFETY: read the size into a winsize and copy it to the private terminal.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(self.source.as_raw_fd(), libc::TIOCGWINSZ, &mut size) == 0 {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size);
            }
        }
    }
}

enum RelayFd {
    Stream(AsyncFd<File>),
    Socket(AsyncFd<File>),
    File(tokio::fs::File),
}

impl RelayFd {
    fn from_client(fd: OwnedFd, interest: Interest) -> io::Result<Self> {
        let file = File::from(fd);
        let kind = file.metadata()?.file_type();
        if kind.is_socket() {
            // SAFETY: File owns this descriptor and the relay never replaces it.
            return Ok(Self::Socket(unsafe {
                AsyncFd::register_with_interest(file, interest)?
            }));
        }
        if kind.is_fifo() || kind.is_char_device() {
            // A new open file description keeps O_NONBLOCK local to the relay.
            let file = OpenOptions::new()
                .read(interest.is_readable())
                .write(interest.is_writable())
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            return Self::pollable(file, interest);
        }
        Ok(Self::File(file.into()))
    }

    fn private_pipe(fd: OwnedFd, interest: Interest) -> io::Result<Self> {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};

        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        Self::pollable(File::from(fd), interest)
    }

    fn pollable(file: File, interest: Interest) -> io::Result<Self> {
        // SAFETY: File owns this descriptor and the relay never replaces it.
        match unsafe { AsyncFd::register_with_interest(file, interest) } {
            Ok(fd) => Ok(Self::Stream(fd)),
            Err(e) => {
                let (file, e) = e.into_parts();
                // Devices such as /dev/null don't support epoll.
                if e.raw_os_error() == Some(libc::EPERM) {
                    Ok(Self::File(file.into()))
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stream(fd) => {
                fd.async_io(Interest::READABLE, |mut file| file.read(buf))
                    .await
            }
            Self::Socket(fd) => {
                fd.async_io(Interest::READABLE, |file| {
                    // SAFETY: buf covers the requested bytes. MSG_DONTWAIT preserves the client's flags.
                    let n = unsafe {
                        libc::recv(
                            file.as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                            libc::MSG_DONTWAIT,
                        )
                    };
                    Ok(nix::errno::Errno::result(n)? as usize)
                })
                .await
            }
            Self::File(file) => file.read(buf).await,
        }
    }

    async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let len = match self {
                Self::Stream(fd) => {
                    fd.async_io(Interest::WRITABLE, |mut file| file.write(buf))
                        .await?
                }
                Self::Socket(fd) => {
                    fd.async_io(Interest::WRITABLE, |file| {
                        // SAFETY: buf covers the requested bytes.
                        let n = unsafe {
                            libc::send(
                                file.as_raw_fd(),
                                buf.as_ptr().cast(),
                                buf.len(),
                                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                            )
                        };
                        Ok(nix::errno::Errno::result(n)? as usize)
                    })
                    .await?
                }
                Self::File(file) => file.write(buf).await?,
            };
            if len == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[len..];
        }
        if let Self::File(file) = self {
            file.flush().await?;
        }
        Ok(())
    }
}

async fn relay_terminal(
    (mut reader, mut writer): (RelayFd, RelayFd),
    tty_reader: bool,
) -> io::Result<()> {
    let mut buf = [0; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if tty_reader && e.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..n]).await?;
    }
}

/// `n` random bytes as lowercase hex.
fn random_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Does a socket named `<id>.sock` in `dir` fit the limit?
fn fits(dir: &Path) -> bool {
    dir.as_os_str().len() + "/12345678.sock".len() < MAX_SOCKET_PATH
}

/// The directory for session sockets: `adapters/sessions` of the home if the paths in it stay
/// short, otherwise a short directory under `runtime_dir` (`$XDG_RUNTIME_DIR`).
pub fn session_dir(paths: &Paths, runtime_dir: Option<&Path>) -> Result<PathBuf> {
    let primary = paths.adapter_sessions();
    if fits(&primary) {
        paths.ensure_adapter_sessions_dir()?;
        return Ok(primary);
    }
    let fallback = fallback_dir(paths, runtime_dir).ok_or_else(|| {
        Error::Unsupported(format!(
            "the home {} is too deep for a session socket (the path must stay under {MAX_SOCKET_PATH} bytes) and XDG_RUNTIME_DIR is not usable. Use a shorter OXIDEPM_HOME",
            paths.home().display()
        ))
    })?;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::symlink_metadata(&fallback) {
        Ok(meta) => {
            let mine = nix::unistd::geteuid().as_raw();
            if !meta.is_dir() || meta.uid() != mine || meta.mode() & 0o077 != 0 {
                return Err(Error::Unsupported(format!(
                    "{} is not a private directory of this user; remove it",
                    fallback.display()
                )));
            }
        }
        Err(_) => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&fallback)
            .map_err(Error::IoError)?,
    }
    Ok(fallback)
}

/// `<runtime dir>/oxidepm-<12 hex of the home's digest>`, if it is short enough.
fn fallback_dir(paths: &Paths, runtime_dir: Option<&Path>) -> Option<PathBuf> {
    let runtime = runtime_dir.filter(|p| p.is_absolute())?;
    let digest = Sha256::digest(paths.home().as_os_str().as_encoded_bytes());
    let tag: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let dir = runtime.join(format!("oxidepm-{tag}"));
    fits(&dir).then_some(dir)
}

/// Remove what an earlier daemon left in the session directories: sockets nobody listens on.
pub fn clean_sessions(paths: &Paths, runtime_dir: Option<&Path>) {
    for dir in [
        Some(paths.adapter_sessions()),
        fallback_dir(paths, runtime_dir),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!(
                    "Could not delete the old session socket {}: {}",
                    entry.path().display(),
                    e
                );
            }
        }
    }
}

/// Compare a token in constant time.
fn token_matches(given: &str, expected: &str) -> bool {
    given.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// The adapter's side of the socket: wait for the connection that presents `token`. Anything else
/// that connects (another user, a wrong token, a line that is not the greeting) is dropped and the
/// wait goes on.
pub(super) async fn accept_adapter(
    listener: &UnixListener,
    token: &str,
    deadline: Instant,
) -> Result<UnixStream> {
    loop {
        let accepted = tokio::time::timeout_at(deadline, listener.accept()).await;
        let stream = match accepted {
            Err(_) => {
                return Err(Error::AdapterTimeout(
                    "the adapter did not connect to its session in time".to_string(),
                ))
            }
            Ok(Err(e)) => return Err(Error::IoError(e)),
            Ok(Ok((stream, _))) => stream,
        };
        match tokio::time::timeout(FIRST_LINE_WAIT, read_adapter_hello(stream, token)).await {
            Ok(Some(stream)) => return Ok(stream),
            Ok(None) | Err(_) => debug!("a connection to a session socket was dropped: it did not present the adapter's token"),
        }
    }
}

/// Read the greeting a session adapter sends first. Byte by byte up to the newline, so nothing
/// after it is consumed.
async fn read_adapter_hello(mut stream: UnixStream, token: &str) -> Option<UnixStream> {
    if peer_uid(&stream).ok()? != oxidepm_ipc::session::my_uid() {
        return None;
    }
    let mut line = Vec::new();
    loop {
        let byte = stream.read_u8().await.ok()?;
        if byte == b'\n' {
            break;
        }
        line.push(byte);
        if line.len() > ADAPTER_HELLO_MAX {
            return None;
        }
    }
    let hello: serde_json::Value = serde_json::from_slice(&line).ok()?;
    let given = hello.get("token")?.as_str()?;
    (hello.get("session")?.as_str()? == "adapter" && token_matches(given, token)).then_some(stream)
}

/// Pass bytes both ways between the adapter's session connection and the channel the host reads.
pub(super) fn bridge(adapter: UnixStream, host: UnixStream) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (mut adapter, mut host) = (adapter, host);
        let _ = tokio::io::copy_bidirectional(&mut adapter, &mut host).await;
    })
}

/// What the CLI attached.
struct Attached {
    stream: UnixStream,
    tty: bool,
    terminal: (OwnedFd, OwnedFd),
    /// Window changes that arrived in the same chunk as the attach line.
    winches: usize,
}

/// Why waiting for the CLI ended without a session.
enum AttachFailure {
    /// Nobody attached in time, or too many wrong tokens.
    Gone(String),
    /// Somebody who knew the token sent the wrong thing; they are told.
    Refused(UnixStream, String),
}

/// The CLI's side: wait for the connection that sends the attach line with the right token.
async fn accept_cli(
    listener: &UnixListener,
    token: &str,
    deadline: Instant,
) -> std::result::Result<Attached, AttachFailure> {
    let mut bad = 0u32;
    loop {
        let stream = match tokio::time::timeout_at(deadline, listener.accept()).await {
            Err(_) => {
                return Err(AttachFailure::Gone(
                    "the terminal was not attached in time".to_string(),
                ))
            }
            Ok(Err(e)) => {
                return Err(AttachFailure::Gone(format!(
                    "the session socket failed: {e}"
                )))
            }
            Ok(Ok((stream, _))) => stream,
        };
        if peer_uid(&stream).ok() != Some(oxidepm_ipc::session::my_uid()) {
            continue;
        }
        let received =
            match tokio::time::timeout(FIRST_LINE_WAIT, recv_with_fds(&stream, MAX_LINE)).await {
                Ok(Ok(received)) => received,
                _ => {
                    bad += 1;
                    if bad >= MAX_BAD_ATTACHES {
                        return Err(AttachFailure::Gone(
                            "too many connections that did not attach".to_string(),
                        ));
                    }
                    continue;
                }
            };
        let (bytes, fds) = received;
        let Some(end) = bytes.iter().position(|b| *b == b'\n') else {
            bad += 1;
            if bad >= MAX_BAD_ATTACHES {
                return Err(AttachFailure::Gone(
                    "too many connections that did not attach".to_string(),
                ));
            }
            continue;
        };
        let (first, rest) = (&bytes[..end], &bytes[end + 1..]);
        let Ok(ClientLine::Attach { token: given, tty }) =
            serde_json::from_slice::<ClientLine>(first)
        else {
            bad += 1;
            if bad >= MAX_BAD_ATTACHES {
                return Err(AttachFailure::Gone(
                    "too many connections that did not attach".to_string(),
                ));
            }
            continue;
        };
        if !token_matches(&given, token) {
            bad += 1;
            if bad >= MAX_BAD_ATTACHES {
                return Err(AttachFailure::Gone(
                    "too many connections that did not attach".to_string(),
                ));
            }
            continue;
        }
        // from here the token is spent: whatever is wrong with the rest ends the session
        let mut fds = fds.into_iter();
        let (Some(input), Some(output), None) = (fds.next(), fds.next(), fds.next()) else {
            return Err(AttachFailure::Refused(
                stream,
                format!("the attach line must carry exactly {ATTACH_FDS} descriptors"),
            ));
        };
        let winches = rest
            .split(|b| *b == b'\n')
            .filter(|l| {
                matches!(
                    serde_json::from_slice::<ClientLine>(l),
                    Ok(ClientLine::Winch)
                )
            })
            .count();
        return Ok(Attached {
            stream,
            tty,
            terminal: (input, output),
            winches,
        });
    }
}

/// Tell the CLI how the session ended; it has five seconds to take the line.
async fn send_closing_line(writer: &mut tokio::net::unix::OwnedWriteHalf, line: &ServerLine) {
    let Ok(mut bytes) = serde_json::to_vec(line) else {
        return;
    };
    bytes.push(b'\n');
    let _ = tokio::time::timeout(CLOSING_LINE_WAIT, async {
        let _ = writer.write_all(&bytes).await;
        let _ = writer.shutdown().await;
    })
    .await;
}

/// Read the CLI's lines after the attach: a window change goes to the adapter, the end of the
/// connection ends the session.
fn watch_cli(
    reader: tokio::net::unix::OwnedReadHalf,
    winch: mpsc::UnboundedSender<()>,
    cancel: CancellationToken,
    window: Option<TerminalWindow>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            match (&mut reader)
                .take(MAX_LINE as u64)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if matches!(
                        serde_json::from_slice::<ClientLine>(&line),
                        Ok(ClientLine::Winch)
                    ) {
                        if let Some(window) = &window {
                            window.update();
                        }
                        let _ = winch.send(());
                    }
                }
            }
        }
        cancel.cancel();
    })
}

impl AdapterHost {
    /// How many sessions may be open (pending or running) at once.
    pub fn session_capacity() -> usize {
        MAX_SESSIONS
    }

    /// Open a session for a resolved session verb: make the socket and the tokens, and start the
    /// task that waits for the CLI and the adapter. Returns what the CLI needs to attach. The
    /// caller has applied the policy; this checks what is the host's to check.
    pub fn open_session(
        self: &Arc<Self>,
        resolved: ResolvedVerb,
        run: SessionRun,
    ) -> Result<SessionOffer> {
        if resolved.entry.kind_of() != Kind::Tool {
            return Err(Error::Unsupported(format!(
                "the verb '{}' of adapter '{}' is a session verb, and only a tool adapter can run one",
                resolved.verb.name, resolved.adapter
            )));
        }
        check_arguments(&resolved.verb.name, &run.argv)?;
        let ctx = self.ctx.get().ok_or_else(|| {
            Error::Unsupported("this daemon has no adapter directories".to_string())
        })?;
        let permit = Arc::clone(&self.session_slots)
            .try_acquire_owned()
            .map_err(|_| {
                Error::AdapterBusy(format!(
                    "{} sessions are open already; close one and try again",
                    Self::session_capacity()
                ))
            })?;

        let dir = session_dir(&ctx.paths, self.cfg.runtime_dir.as_deref())?;
        let id = random_hex(4).map_err(Error::IoError)?;
        let socket = dir.join(format!("{id}.sock"));
        let listener = UnixListener::bind(&socket).map_err(Error::IoError)?;
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            {
                let _ = std::fs::remove_file(&socket);
                return Err(Error::IoError(e));
            }
        }
        let tokens = random_hex(32).and_then(|cli| random_hex(32).map(|adapter| (cli, adapter)));
        let (cli_token, adapter_token) = match tokens {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::remove_file(&socket);
                return Err(Error::IoError(e));
            }
        };

        let offer = SessionOffer {
            adapter: resolved.adapter.clone(),
            verb: resolved.verb.name.clone(),
            socket: socket.display().to_string(),
            token: cli_token.clone(),
            expires_in_secs: self.cfg.session_window.as_secs().max(1),
        };
        let slot = self
            .next_slot
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pending = Pending {
            resolved,
            run,
            listener,
            socket,
            cli_token,
            adapter_token,
            deadline: Instant::now() + self.cfg.session_window,
            max: self.cfg.session_max,
            slot,
            _permit: permit,
        };
        tokio::spawn(run_session(pending));
        Ok(offer)
    }
}

/// A session between the offer and its end.
struct Pending {
    resolved: ResolvedVerb,
    run: SessionRun,
    listener: UnixListener,
    socket: PathBuf,
    cli_token: String,
    adapter_token: String,
    /// The tokens are valid until this moment.
    deadline: Instant,
    /// The session ends this long after the adapter started.
    max: Duration,
    slot: u32,
    /// Held until the session is over.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Wait for the CLI, run the verb, tell the CLI how it ended, and take the socket away.
async fn run_session(pending: Pending) {
    let Pending {
        resolved,
        run,
        listener,
        socket,
        cli_token,
        adapter_token,
        deadline,
        max,
        slot,
        _permit,
    } = pending;
    let name = format!("{}/{}", resolved.adapter, resolved.verb.name);

    let attached = match accept_cli(&listener, &cli_token, deadline).await {
        Ok(attached) => attached,
        Err(AttachFailure::Gone(why)) => {
            info!("Session {name} was not started: {why}");
            let _ = std::fs::remove_file(&socket);
            return;
        }
        Err(AttachFailure::Refused(stream, why)) => {
            info!("Session {name} was not started: {why}");
            let (_, mut writer) = stream.into_split();
            send_closing_line(
                &mut writer,
                &ServerLine::Error {
                    code: ErrorCode::InvalidSpec.as_str().to_string(),
                    message: why,
                },
            )
            .await;
            let _ = std::fs::remove_file(&socket);
            return;
        }
    };
    let Attached {
        stream,
        tty,
        terminal,
        winches,
    } = attached;
    let (reader, mut writer) = stream.into_split();
    let TerminalRelay {
        terminal,
        input,
        output,
        settings,
        window,
        tty_output,
    } = match TerminalRelay::new(terminal) {
        Ok(relay) => relay,
        Err(e) => {
            let error = Error::IoError(e);
            send_closing_line(
                &mut writer,
                &ServerLine::Error {
                    code: error.code().as_str().to_string(),
                    message: format!("cannot relay the session terminal: {error}"),
                },
            )
            .await;
            let _ = std::fs::remove_file(&socket);
            return;
        }
    };
    let (winch_tx, winch_rx) = mpsc::unbounded_channel();
    for _ in 0..winches {
        let _ = winch_tx.send(());
    }
    let cancel = CancellationToken::new();
    let watcher = watch_cli(reader, winch_tx, cancel.clone(), window);

    let call = SessionCall {
        call: InvokeCall {
            params: InvokeParams {
                verb: resolved.verb.name.clone(),
                argv: run.argv,
                cwd: run.cwd.display().to_string(),
                dry_run: false,
                actor: run.actor,
                tty,
            },
            max_runtime: max,
            restrict_writes: run.restrict_writes,
            slot,
        },
        terminal,
        listener,
        socket: socket.clone(),
        token: adapter_token,
        deadline,
        winch: winch_rx,
    };
    info!("Session {name} started");
    let started = std::time::Instant::now();
    let result = {
        let session = async {
            let mut run = Box::pin(resolved.entry.run_session(call, cancel.clone()));
            tokio::select! {
                r = &mut run => r,
                _ = tokio::time::sleep(max) => {
                    cancel.cancel();
                    match tokio::time::timeout(END_WAIT, &mut run).await {
                        Ok(_) => Err(Error::Cancelled(format!("the session reached its limit of {} hours", MAX_SESSION_SECS / 3600))),
                        Err(_) => Err(Error::Cancelled("the session reached its limit and did not stop in time".to_string())),
                    }
                }
            }
        };
        let mut input = Box::pin(relay_terminal(input, tty));
        let mut output = Box::pin(relay_terminal(output, tty_output));
        tokio::pin!(session);
        let mut input_done = false;
        let mut output_done = false;
        let result = loop {
            tokio::select! {
                result = &mut session => break result,
                _ = &mut input, if !input_done => input_done = true,
                result = &mut output, if !output_done => {
                    output_done = true;
                    if result.is_err() {
                        cancel.cancel();
                    }
                }
            }
        };
        drop(input);
        if !output_done {
            let _ = tokio::time::timeout(TERMINAL_DRAIN_WAIT, &mut output).await;
        }
        result
    };
    watcher.abort();
    let _ = watcher.await;
    drop(settings);

    let line = match &result {
        Ok(done) => ServerLine::Done {
            exit_code: done.exit_code,
            message: None,
        },
        Err(e) => ServerLine::Error {
            code: e.code().as_str().to_string(),
            message: e.to_string(),
        },
    };
    info!(
        "Session {name} ended after {} s ({})",
        started.elapsed().as_secs(),
        match &result {
            Ok(done) => format!("exit code {}", done.exit_code),
            Err(e) => e.to_string(),
        }
    );
    send_closing_line(&mut writer, &line).await;
    let _ = std::fs::remove_file(&socket);
}

/// The window in which the tokens are valid, by default.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(TOKEN_SECS);
/// The longest session, by default.
pub const DEFAULT_MAX: Duration = Duration::from_secs(MAX_SESSION_SECS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_whole() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("", "abc"));
    }

    #[test]
    fn random_tokens_differ_and_have_the_requested_length() {
        let (a, b) = (random_hex(32).unwrap(), random_hex(32).unwrap());
        assert_eq!((a.len(), b.len()), (64, 64));
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_short_home_keeps_its_sockets_and_a_deep_one_moves_them_to_the_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let short = Paths::new(dir.path().join("h"));
        let runtime = dir.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        let chosen = session_dir(&short, Some(&runtime)).unwrap();
        assert_eq!(chosen, short.adapter_sessions());

        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 120 {
            deep = deep.join("a-fairly-long-directory-name");
        }
        let deep = Paths::new(deep);
        let chosen = session_dir(&deep, Some(&runtime)).unwrap();
        assert!(
            chosen.starts_with(&runtime) && fits(&chosen),
            "{}",
            chosen.display()
        );
        assert!(chosen.as_os_str().len() + "/12345678.sock".len() < MAX_SOCKET_PATH);
        // the same home always maps to the same directory
        assert_eq!(session_dir(&deep, Some(&runtime)).unwrap(), chosen);
        // without a runtime directory a deep home cannot have sessions, and says why
        let err = session_dir(&deep, None).unwrap_err();
        assert!(err.to_string().contains("too deep"), "{err}");
        // a runtime directory that is itself too long does not help either
        let long_runtime = runtime.join("x".repeat(100));
        assert!(session_dir(&deep, Some(&long_runtime)).is_err());
    }

    #[test]
    fn a_fallback_directory_that_is_not_private_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 120 {
            deep = deep.join("a-fairly-long-directory-name");
        }
        let paths = Paths::new(deep);
        let chosen = session_dir(&paths, Some(&runtime)).unwrap();
        std::fs::set_permissions(&chosen, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(session_dir(&paths, Some(&runtime))
            .unwrap_err()
            .to_string()
            .contains("not a private directory"));
    }

    #[test]
    fn old_session_sockets_are_removed_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("h"));
        let sessions = session_dir(&paths, None).unwrap();
        std::fs::write(sessions.join("deadbeef.sock"), b"").unwrap();
        clean_sessions(&paths, None);
        assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), 0);
    }
}
