//! Sessions: an adapter verb that owns the terminal (the monitor, an MCP server over stdio).
//!
//! A relayed verb streams its output through the daemon. A session verb cannot: its standard
//! input and output belong to a person's terminal or to a program that talks to it, so the
//! daemon starts the adapter itself, under the same sandbox as every other adapter, and the CLI
//! hands over its own standard input and output as open descriptors.
//!
//! The steps, all on a socket the daemon made for this one session:
//!
//! 1. The CLI asks for the session on the daemon's main socket. The answer is a [`SessionOffer`]:
//!    the path of a fresh socket and a single-use token that is valid for [`TOKEN_SECS`] seconds.
//! 2. The CLI connects to that socket and sends one line, [`ClientLine::Attach`], together with
//!    its standard input and output as `SCM_RIGHTS`. The daemon starts the adapter with them as
//!    its descriptors 0 and 1.
//! 3. The adapter connects to the same socket (it is given the path and a second token in its
//!    environment) and speaks the adapter protocol over it; its standard streams are the
//!    terminal's, so they cannot carry the protocol.
//! 4. The CLI keeps its connection open. It sends [`ClientLine::Winch`] when the window changes
//!    size, and the daemon answers with one [`ServerLine`] when the session is over. Closing the
//!    connection ends the session.
//!
//! The CLI never executes anything it was told about: the path of the executable and the sandbox
//! profile never leave the daemon. What it checks is that the socket it hands its terminal to is
//! private to the current user ([`check_private_socket`]).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// How long the tokens of a session are valid, counted from the moment it was opened.
pub const TOKEN_SECS: u64 = 60;
/// Sessions open at the same moment, pending ones included.
pub const MAX_SESSIONS: usize = 4;
/// The longest a session may last.
pub const MAX_SESSION_SECS: u64 = 12 * 60 * 60;
/// A session socket path must be shorter than this (the limit of `sockaddr_un` is 108).
pub const MAX_SOCKET_PATH: usize = 100;
/// The longest line either side sends.
pub const MAX_LINE: usize = 4096;
/// Descriptors accepted with the attach line: standard input and output.
pub const ATTACH_FDS: usize = 2;
/// The name of the environment variable that gives the adapter the path of the session socket.
pub const ENV_SOCKET: &str = "OXIDEPM_SESSION_SOCKET";
/// The name of the environment variable that gives the adapter its token.
pub const ENV_TOKEN: &str = "OXIDEPM_SESSION_TOKEN";

/// What the daemon answers to a request to open a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOffer {
    /// The adapter that provides the verb.
    pub adapter: String,
    pub verb: String,
    /// The socket to attach to.
    pub socket: String,
    /// The single-use token for the attach line.
    pub token: String,
    pub expires_in_secs: u64,
}

/// A line the CLI sends on the session socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientLine {
    /// The first line, with the descriptors attached.
    Attach {
        token: String,
        /// Standard input is a terminal.
        #[serde(default)]
        tty: bool,
    },
    /// The terminal changed size.
    Winch,
}

/// The line the daemon sends when the session is over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerLine {
    /// The verb finished with this exit code.
    Done {
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// The session could not be run or ended abnormally.
    Error { code: String, message: String },
}

/// Send `bytes` with `fds` attached (they arrive with the first byte).
pub async fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        stream.writable().await?;
        // the descriptors go with the first chunk only
        let attach: &[RawFd] = if sent == 0 { fds } else { &[] };
        let result = stream.try_io(tokio::io::Interest::WRITABLE, || {
            sendmsg_fds(stream.as_raw_fd(), &bytes[sent..], attach)
        });
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "no session bytes sent",
                ))
            }
            Ok(n) => sent += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `sendmsg` of `bytes` with `fds` as one `SCM_RIGHTS` message.
fn sendmsg_fds(socket: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let payload = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(payload as u32) } as usize;
    // a buffer of u64 keeps the alignment the control message header needs
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: an all-zero msghdr is valid; the fields that matter are set below.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    if !fds.is_empty() {
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as _;
        // SAFETY: the control buffer is `space` bytes, enough for one header and `payload` bytes
        // of data; the pointers come from the CMSG macros for exactly that buffer.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(payload as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                payload,
            );
        }
    }
    // SAFETY: `header` points at live buffers for the duration of the call.
    let n = unsafe { libc::sendmsg(socket, &header, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Read what is there (at most `max` bytes) with the descriptors attached to it. An empty
/// buffer means the peer closed the connection. More than [`ATTACH_FDS`] descriptors, or control
/// data that did not fit, is an error (what arrived is closed).
pub async fn recv_with_fds(stream: &UnixStream, max: usize) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    loop {
        stream.readable().await?;
        match stream.try_io(tokio::io::Interest::READABLE, || {
            recvmsg_fds(stream.as_raw_fd(), max)
        }) {
            Ok(received) => return Ok(received),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
}

fn recvmsg_fds(socket: RawFd, max: usize) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut buf = vec![0u8; max];
    // Linux discards descriptors that do not fit in the control buffer.
    // SAFETY: CMSG_SPACE only computes a size.
    let space =
        unsafe { libc::CMSG_SPACE((ATTACH_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero msghdr is valid; the fields that matter are set below.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = space as _;
    // SAFETY: `header` points at live buffers for the duration of the call.
    let n = unsafe { libc::recvmsg(socket, &mut header, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut truncated = header.msg_flags & libc::MSG_CTRUNC != 0;
    // SAFETY: the CMSG macros walk the kernel's control buffer. Each descriptor read
    // is bounded by both its message length and the bytes actually copied out.
    unsafe {
        let control_end = header.msg_control as usize + header.msg_controllen as usize;
        let data_offset = libc::CMSG_LEN(0) as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            let remaining = control_end - cmsg as usize;
            let len = (*cmsg).cmsg_len as usize;
            if len < data_offset {
                truncated = true;
                break;
            }
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let bytes = len.min(remaining).saturating_sub(data_offset);
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    // these descriptors were just made for this process; nothing else owns them
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            if len > remaining {
                truncated = true;
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }
    if truncated || fds.len() > ATTACH_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many descriptors were sent",
        ));
    }
    buf.truncate(n as usize);
    Ok((buf, fds))
}

/// Is `path` a socket this user can trust with a terminal: a socket, owned by the current user,
/// in a directory that belongs to the current user and that no group or other can write to. The
/// environment can point the CLI at any socket, so the CLI looks before it sends anything.
pub fn check_private_socket(path: &Path) -> Result<(), String> {
    let me = my_uid();
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot look at {}: {e}", path.display()))?;
    if !meta.file_type().is_socket() {
        return Err(format!("{} is not a socket", path.display()));
    }
    if meta.uid() != me {
        return Err(format!("{} belongs to another user", path.display()));
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let dir_meta =
        std::fs::metadata(dir).map_err(|e| format!("cannot look at {}: {e}", dir.display()))?;
    if dir_meta.uid() != me {
        return Err(format!(
            "the directory {} belongs to another user",
            dir.display()
        ));
    }
    if dir_meta.mode() & 0o022 != 0 {
        return Err(format!(
            "the directory {} can be written to by other users",
            dir.display()
        ));
    }
    Ok(())
}

/// The user id of the process at the other end of `stream`.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    stream.peer_cred().map(|c| c.uid())
}

/// The user id of this process.
pub fn my_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// The CLI's end of a session: attached to the daemon's socket, with the terminal handed over.
pub struct SessionClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl SessionClient {
    /// Connect to the offered socket and attach `stdin` and `stdout`. The socket must be private
    /// to this user and the daemon behind it must run as this user; otherwise nothing is sent.
    pub async fn attach(
        offer: &SessionOffer,
        tty: bool,
        stdin: RawFd,
        stdout: RawFd,
        timeout: Duration,
    ) -> Result<SessionClient, String> {
        let path = Path::new(&offer.socket);
        check_private_socket(path).map_err(|why| format!("the session socket is not private to you ({why}); the terminal was not handed over"))?;
        let stream = tokio::time::timeout(timeout, UnixStream::connect(path))
            .await
            .map_err(|_| "the session socket did not answer".to_string())?
            .map_err(|e| format!("cannot connect to the session socket: {e}"))?;
        match peer_uid(&stream) {
            Ok(uid) if uid == my_uid() => {}
            Ok(_) => return Err(
                "the session socket is served by another user; the terminal was not handed over"
                    .to_string(),
            ),
            Err(e) => return Err(format!("cannot tell who serves the session socket: {e}")),
        }
        let mut line = serde_json::to_vec(&ClientLine::Attach {
            token: offer.token.clone(),
            tty,
        })
        .map_err(|e| e.to_string())?;
        line.push(b'\n');
        send_with_fds(&stream, &line, &[stdin, stdout])
            .await
            .map_err(|e| format!("cannot attach to the session: {e}"))?;
        let (reader, writer) = stream.into_split();
        Ok(SessionClient {
            reader: BufReader::new(reader),
            writer,
        })
    }

    /// Split into the part that waits for the end and the part that sends window changes.
    pub fn split(self) -> (SessionEvents, SessionControl) {
        (
            SessionEvents {
                reader: self.reader,
            },
            SessionControl {
                writer: self.writer,
            },
        )
    }
}

/// Waits for the daemon's closing line.
pub struct SessionEvents {
    reader: BufReader<OwnedReadHalf>,
}

impl SessionEvents {
    /// The daemon's line, or `None` when it closed the connection without one.
    pub async fn next(&mut self) -> io::Result<Option<ServerLine>> {
        let mut line = Vec::new();
        let n = (&mut self.reader)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        serde_json::from_slice(&line).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the daemon sent a line that is not understood: {e}"),
            )
        })
    }
}

/// Sends window changes. Dropping it closes the connection, which ends the session.
pub struct SessionControl {
    writer: OwnedWriteHalf,
}

impl SessionControl {
    /// Tell the daemon the terminal changed size.
    pub async fn winch(&mut self) -> io::Result<()> {
        let mut line = serde_json::to_vec(&ClientLine::Winch).map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn pipe_pair() -> (std::fs::File, std::fs::File) {
        let (r, w) = nix::unistd::pipe().unwrap();
        (std::fs::File::from(r), std::fs::File::from(w))
    }

    #[test]
    fn lines_have_a_stable_shape() {
        let attach = serde_json::to_string(&ClientLine::Attach {
            token: "t".into(),
            tty: true,
        })
        .unwrap();
        assert_eq!(attach, r#"{"op":"attach","token":"t","tty":true}"#);
        assert_eq!(
            serde_json::to_string(&ClientLine::Winch).unwrap(),
            r#"{"op":"winch"}"#
        );
        let done = serde_json::to_string(&ServerLine::Done {
            exit_code: 3,
            message: None,
        })
        .unwrap();
        assert_eq!(done, r#"{"event":"done","exit_code":3}"#);
        let parsed: ClientLine = serde_json::from_str(r#"{"op":"attach","token":"x"}"#).unwrap();
        assert_eq!(
            parsed,
            ClientLine::Attach {
                token: "x".into(),
                tty: false
            }
        );
    }

    #[tokio::test]
    async fn descriptors_travel_with_the_first_line() {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};

        let (a, b) = UnixStream::pair().unwrap();
        let (mut r, w) = pipe_pair();
        send_with_fds(&a, b"hello\n", &[w.as_raw_fd(), w.as_raw_fd()])
            .await
            .unwrap();
        let (bytes, fds) = recv_with_fds(&b, 64).await.unwrap();
        assert_eq!(bytes, b"hello\n");
        assert_eq!(fds.len(), 2);
        for fd in &fds {
            let flags = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
            assert_ne!(flags & FdFlag::FD_CLOEXEC.bits(), 0);
        }
        // the received descriptor is the pipe's write end
        let mut sent = std::fs::File::from(fds.into_iter().next().unwrap());
        std::io::Write::write_all(&mut sent, b"x").unwrap();
        drop((sent, w));
        let mut got = String::new();
        std::io::Read::read_to_string(&mut r, &mut got).unwrap();
        assert_eq!(got, "x");
    }

    #[tokio::test]
    async fn more_descriptors_than_expected_are_refused() {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        use std::io::Read;

        let (a, b) = UnixStream::pair().unwrap();
        let (mut r, w) = pipe_pair();
        let fd = w.as_raw_fd();
        send_with_fds(&a, b"x\n", &[fd, fd, fd, fd, fd])
            .await
            .unwrap();
        drop(w);
        let err = recv_with_fds(&b, 64).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        fcntl(r.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        assert_eq!(r.read(&mut [0]).unwrap(), 0, "all pipe writers were closed");
    }

    #[tokio::test]
    async fn a_closed_peer_reads_as_an_empty_buffer() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        let (bytes, fds) = recv_with_fds(&b, 64).await.unwrap();
        assert!(bytes.is_empty() && fds.is_empty());
    }

    #[test]
    fn only_a_private_socket_in_a_private_directory_is_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("s");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = sub.join("d.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(check_private_socket(&path), Ok(()));

        // a directory others can write to is not trusted, even for our own socket
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(check_private_socket(&path)
            .unwrap_err()
            .contains("can be written to"));
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o702)).unwrap();
        assert!(check_private_socket(&path).is_err());
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();

        // a file that is not a socket, a link, and a path that is not there
        let plain = sub.join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(check_private_socket(&plain)
            .unwrap_err()
            .contains("not a socket"));
        let link = sub.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            check_private_socket(&link).is_err(),
            "a link is not followed"
        );
        assert!(check_private_socket(&sub.join("missing")).is_err());
    }
}
