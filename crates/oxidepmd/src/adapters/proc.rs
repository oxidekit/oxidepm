//! One adapter process: the integrity check, the spawn, the stderr log, the shutdown
//! sequence and the kill walk.
//!
//! With a [`SandboxSetup`] the child is the launcher `oxidepm-sandbox`, which restricts itself and
//! executes the verified adapter; without one the adapter is started directly with a
//! scrubbed environment and only the daemon-side checks hold. `oxidepm_runtime::spawn_adapter` is
//! the one place a process is created either way.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::fcntl::{openat, renameat, AtFlags, Flock, FlockArg, OFlag};
use nix::sys::signal::Signal;
use nix::sys::stat::{fstatat, mkdirat, Mode, SFlag};
use oxidepm_adapter_proto::SUPPORTED_PROTOCOLS;
use oxidepm_runtime::{
    group_exists, kill_group, spawn_adapter, AdapterLaunch, GroupChild, SandboxLaunch,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;
use tracing::warn;

use super::conn::{AdapterConn, ConnConfig, Inbound, TokenBucket};
use crate::supervisor::{start_ticks, PidGuard, PidRecord, PidTable};

/// Longest stderr line kept; the rest of a longer line is dropped.
pub const STDERR_LINE_MAX: usize = 8 * 1024;
/// Stderr is read at most this fast; lines past it are dropped.
pub const STDERR_BYTES_PER_SEC: f64 = 1024.0 * 1024.0;
/// The adapter's own log: 1 MiB per file, 5 files.
const STDERR_LOG_SIZE: u64 = 1024 * 1024;
const STDERR_LOG_FILES: usize = 5;
/// Lines of stderr kept in memory for `adapter info`.
pub const STDERR_TAIL_LINES: usize = 20;
/// At most this many descendants are visited by the kill walk.
const WALK_MAX: usize = 256;
/// How long the leader may take to be reaped after the SIGKILL.
const REAP_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// Why an adapter process could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    /// The executable is not there (`needs_reinstall`).
    Missing(String),
    /// The executable does not match the digest recorded at import (`E_ADAPTER_TAMPERED`).
    Tampered(String),
    /// The sandbox profile could not be prepared.
    Sandbox(String),
    /// The process could not be created.
    Spawn(String),
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcError::Missing(s)
            | ProcError::Tampered(s)
            | ProcError::Sandbox(s)
            | ProcError::Spawn(s) => f.write_str(s),
        }
    }
}

/// How the shutdown sequence is timed: the host asks politely and closes stdin, waits
/// `stdin_phase` in all, then sends SIGTERM to the process group and waits `term_phase`, then
/// kills. The total grace is 5 s. Tests inject shorter intervals.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownTimings {
    pub stdin_phase: Duration,
    pub term_phase: Duration,
}

impl ShutdownTimings {
    pub const STANDARD: ShutdownTimings = ShutdownTimings {
        stdin_phase: Duration::from_secs(2),
        term_phase: Duration::from_secs(3),
    };

    pub fn total(&self) -> Duration {
        self.stdin_phase + self.term_phase
    }
}

/// How a shutdown ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownEnd {
    /// The adapter and everything in its group were gone before SIGTERM.
    Graceful,
    /// SIGTERM was needed.
    Terminated,
    /// SIGKILL was needed.
    Killed,
}

/// Where the values of an adapter's `env_pass` names come from.
pub type EnvSource = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The launcher and the profile it applies.
#[derive(Clone)]
pub struct SandboxSetup {
    /// The `oxidepm-sandbox` binary.
    pub launcher: PathBuf,
    /// The profile, as the JSON the launcher is given.
    pub profile_json: String,
    /// The user accepted that the OS may enforce less than the manifest requires.
    pub allow_unenforced: bool,
}

/// What it takes to start one adapter process.
#[derive(Clone)]
pub struct Launch {
    pub name: String,
    pub version: String,
    /// The installed executable, an absolute path.
    pub exec: PathBuf,
    /// The digest recorded at import, 64 lowercase hex digits.
    pub exe_sha256: String,
    /// The adapter's own writable directory (also its `HOME`).
    pub state_dir: PathBuf,
    pub daemon_home: PathBuf,
    pub log_dir: PathBuf,
    /// Names of the daemon's environment variables the manifest asks to receive.
    pub env_pass: Vec<String>,
    /// Passed variables that may name sockets for the launcher to allow.
    pub connect_env_socket: Vec<String>,
    /// Where the values of `env_pass` come from: the daemon's environment in production.
    pub env_source: EnvSource,
    /// The sandbox launcher, if the adapter is to be restricted by the OS.
    pub sandbox: Option<SandboxSetup>,
    /// The resident memory the adapter's whole process tree may use, in MiB; zero is no limit.
    pub memory_mb: u32,
}

/// The sha256 of an open file, as lowercase hex.
fn sha256_hex(file: &mut File) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Open the executable (never through a symbolic link), hash the open descriptor and compare the
/// digest in constant time. The file returned is the one that was hashed; the process is started
/// from it. Blocking: run it on a blocking thread.
pub fn open_verified(path: &Path, expected_hex: &str) -> Result<File, ProcError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                ProcError::Missing(format!("{} is missing", path.display()))
            }
            _ => ProcError::Missing(format!("cannot open {}: {e}", path.display())),
        })?;
    let meta = file
        .metadata()
        .map_err(|e| ProcError::Missing(format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(ProcError::Tampered(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let digest = sha256_hex(&mut file)
        .map_err(|e| ProcError::Missing(format!("cannot read {}: {e}", path.display())))?;
    let equal: bool = digest
        .as_bytes()
        .ct_eq(expected_hex.to_ascii_lowercase().as_bytes())
        .into();
    if !equal {
        return Err(ProcError::Tampered(format!(
            "the executable {} does not match the digest recorded when the adapter was imported",
            path.display()
        )));
    }
    Ok(file)
}

fn libc_o_nofollow() -> i32 {
    nix::fcntl::OFlag::O_NOFOLLOW.bits()
}

/// The environment of an adapter: nothing of the daemon's own, a short base, `HOME` set to
/// the adapter's state directory unless the manifest passes `HOME`, the names the manifest lists
/// in `env_pass`, and the protocol list.
pub fn adapter_env(
    state_dir: &Path,
    env_pass: &[String],
    source: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ];
    if !env_pass.iter().any(|k| k == "HOME") {
        env.push(("HOME".into(), state_dir.display().to_string()));
    }
    for key in env_pass {
        if let Some(value) = source(key) {
            env.retain(|(k, _)| k != key);
            env.push((key.clone(), value));
        }
    }
    let protocols = SUPPORTED_PROTOCOLS
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    env.retain(|(k, _)| k != "OXIDEPM_ADAPTER_PROTOCOLS");
    env.push(("OXIDEPM_ADAPTER_PROTOCOLS".into(), protocols));
    env
}

fn resolve_env_sockets(
    daemon_home: &Path,
    names: &[String],
    env: &[(String, String)],
) -> Result<Vec<PathBuf>, ProcError> {
    let mut sockets = Vec::new();
    let uid = nix::unistd::geteuid().as_raw();
    for (name, value) in env.iter().filter(|(name, _)| names.contains(name)) {
        let invalid = |why: &str| {
            ProcError::Sandbox(format!(
                "{name} must name a UNIX socket owned by the daemon user ({why})"
            ))
        };
        let path = Path::new(value);
        if !path.is_absolute() {
            return Err(invalid("expected an absolute path"));
        }
        let path = path.canonicalize().map_err(|e| invalid(&e.to_string()))?;
        let home = daemon_home
            .canonicalize()
            .map_err(|e| ProcError::Sandbox(format!("Cannot resolve the daemon home: {e}")))?;
        if path.starts_with(home) {
            return Err(invalid("inside the daemon home"));
        }
        let meta = path.metadata().map_err(|e| invalid(&e.to_string()))?;
        if !meta.file_type().is_socket() || meta.uid() != uid {
            return Err(invalid("wrong file type or owner"));
        }
        if path.to_str().is_none() {
            return Err(invalid("the resolved path is not UTF-8"));
        }
        sockets.push(path);
    }
    sockets.sort();
    sockets.dedup();
    Ok(sockets)
}

/// What makes a process a session's: the terminal it runs on, the values that tell it where
/// to connect, and the daemon's end of the channel its connection is bridged onto.
pub struct SessionLaunch {
    /// Standard input and output of the adapter.
    pub terminal: (std::os::fd::OwnedFd, std::os::fd::OwnedFd),
    /// Added to its environment.
    pub env: Vec<(String, String)>,
    /// The protocol runs over this stream.
    pub channel: tokio::net::UnixStream,
}

/// A running adapter process and its connection.
pub struct AdapterProc {
    child: GroupChild,
    pub conn: Arc<AdapterConn>,
    pid: u32,
    pub started: Instant,
    /// The last lines of stderr, not yet scrubbed (they are scrubbed when shown).
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Stderr lines dropped for exceeding the rate.
    pub stderr_dropped: Arc<AtomicU64>,
    stderr_task: JoinHandle<()>,
    _pid_guard: PidGuard,
}

impl AdapterProc {
    /// Verify the executable and start it. `slot` identifies the adapter in `adapter-pids.json`.
    pub async fn start(
        launch: &Launch,
        pids: &Arc<PidTable>,
        slot: u32,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
    ) -> Result<AdapterProc, ProcError> {
        Self::start_named(launch, pids, slot, cfg, inbound, "stderr.log").await
    }

    /// Like [`start`](Self::start), writing the process's stderr to `log_name` in the state
    /// directory (a process started for one call keeps its own log).
    pub async fn start_named(
        launch: &Launch,
        pids: &Arc<PidTable>,
        slot: u32,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
        log_name: &str,
    ) -> Result<AdapterProc, ProcError> {
        Self::start_with(launch, pids, slot, cfg, inbound, log_name, None).await
    }

    /// Start the process of a session: its standard input and output are the terminal, and the
    /// protocol runs over `session.channel` instead of its pipes.
    pub async fn start_session(
        launch: &Launch,
        pids: &Arc<PidTable>,
        slot: u32,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
        log_name: &str,
        session: SessionLaunch,
    ) -> Result<AdapterProc, ProcError> {
        Self::start_with(launch, pids, slot, cfg, inbound, log_name, Some(session)).await
    }

    async fn start_with(
        launch: &Launch,
        pids: &Arc<PidTable>,
        slot: u32,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
        log_name: &str,
        session: Option<SessionLaunch>,
    ) -> Result<AdapterProc, ProcError> {
        let exec = launch.exec.clone();
        let expected = launch.exe_sha256.clone();
        let exe = tokio::task::spawn_blocking(move || open_verified(&exec, &expected))
            .await
            .map_err(|e| ProcError::Spawn(format!("the integrity check failed to run: {e}")))??;

        ensure_state_dir(&launch.state_dir).map_err(|e| {
            ProcError::Spawn(format!(
                "the state directory {}: {e}",
                launch.state_dir.display()
            ))
        })?;
        let mut env = adapter_env(
            &launch.state_dir,
            &launch.env_pass,
            launch.env_source.as_ref(),
        );
        let socket_profile = if launch.connect_env_socket.is_empty() {
            None
        } else {
            let sockets =
                resolve_env_sockets(&launch.daemon_home, &launch.connect_env_socket, &env)?;
            match &launch.sandbox {
                Some(sandbox) if !sockets.is_empty() => {
                    let mut profile = oxidepm_sandbox::Profile::from_json(&sandbox.profile_json)
                        .map_err(ProcError::Sandbox)?;
                    profile.connect_unix = sockets;
                    Some(profile.to_json())
                }
                _ => None,
            }
        };
        if let Some(session) = &session {
            // after the manifest's names: the session's own values cannot be replaced by env_pass
            for (key, value) in &session.env {
                env.retain(|(k, _)| k != key);
                env.push((key.clone(), value.clone()));
            }
        }
        let mut child = spawn_adapter(&AdapterLaunch {
            exe: &exe,
            arg0: &launch.name,
            args: &[],
            env: &env,
            cwd: &launch.state_dir,
            sandbox: launch.sandbox.as_ref().map(|s| SandboxLaunch {
                launcher: &s.launcher,
                profile_json: socket_profile.as_deref().unwrap_or(&s.profile_json),
                allow_unenforced: s.allow_unenforced,
            }),
            terminal: session.as_ref().map(|s| (&s.terminal.0, &s.terminal.1)),
        })
        .map_err(|e| ProcError::Spawn(e.to_string()))?;
        drop(exe);

        let pid = child.pid();
        let pid_guard = pids.add(PidRecord {
            app_id: slot,
            name: launch.name.clone(),
            pid,
            pgid: child.pgid(),
            start_ticks: start_ticks(pid),
        });
        if let Some(session) = session {
            let Some(stderr) = child.take_stderr() else {
                child.kill_and_wait().await;
                return Err(ProcError::Spawn("the adapter's pipes are missing".into()));
            };
            // the terminal descriptors were duplicated into the child; ours can go
            let (reader, writer) = session.channel.into_split();
            drop(session.terminal);
            let conn = AdapterConn::start_handshaking(reader, writer, cfg, inbound);
            return Ok(Self::finish_start(
                child, conn, pid, stderr, launch, log_name, pid_guard,
            ));
        }
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.take_stdout(), child.take_stderr())
        else {
            child.kill_and_wait().await;
            return Err(ProcError::Spawn("the adapter's pipes are missing".into()));
        };
        let conn = AdapterConn::start_handshaking(stdout, stdin, cfg, inbound);
        Ok(Self::finish_start(
            child, conn, pid, stderr, launch, log_name, pid_guard,
        ))
    }

    fn finish_start(
        child: GroupChild,
        conn: Arc<AdapterConn>,
        pid: u32,
        stderr: tokio::process::ChildStderr,
        launch: &Launch,
        log_name: &str,
        pid_guard: PidGuard,
    ) -> AdapterProc {
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let dropped = Arc::new(AtomicU64::new(0));
        let stderr_task = tokio::spawn(read_stderr(
            stderr,
            launch.log_dir.join(log_name),
            Arc::clone(&tail),
            Arc::clone(&dropped),
        ));
        AdapterProc {
            child,
            conn,
            pid,
            started: Instant::now(),
            stderr_tail: tail,
            stderr_dropped: dropped,
            stderr_task,
            _pid_guard: pid_guard,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn pgid(&self) -> i32 {
        self.child.pgid()
    }

    /// The exit status if the leader has exited, without waiting.
    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Wait for the leader to exit. Cancel safe.
    pub async fn wait_exit(&mut self) -> Option<ExitStatus> {
        self.child.wait().await.ok()
    }

    /// Does anything of this adapter's process group still exist?
    pub fn group_alive(&self) -> bool {
        group_exists(self.child.pgid())
    }

    /// The polite end, within `t.total()`: `shutdown`, then stdin closed, then SIGTERM to
    /// the group, then the kill walk. Whatever the adapter does, every process of its group is
    /// gone when this returns.
    pub async fn shutdown(&mut self, t: ShutdownTimings) -> ShutdownEnd {
        let phase1 = Instant::now() + t.stdin_phase;
        // the request is a courtesy: the answer, the silence and a refusal all end the same way
        let _ = self
            .conn
            .call("shutdown", &serde_json::json!({}), t.stdin_phase)
            .await;
        self.conn.close_input();
        if self.wait_gone(phase1).await {
            self.finish().await;
            return ShutdownEnd::Graceful;
        }

        let _ = kill_group(self.child.pgid(), Signal::SIGTERM);
        if self.wait_gone(phase1 + t.term_phase).await {
            self.finish().await;
            return ShutdownEnd::Terminated;
        }
        self.kill().await;
        ShutdownEnd::Killed
    }

    /// Wait until the leader has exited and the group is empty, or `until`.
    async fn wait_gone(&mut self, until: Instant) -> bool {
        loop {
            let exited = self.child.try_wait().ok().flatten().is_some();
            if exited && !group_exists(self.child.pgid()) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait (at most `max`) until the stderr reader has seen the end of the stream, so the last
    /// lines of an adapter that has exited are in the tail before `kill` stops the reader.
    pub async fn drain_stderr(&mut self, max: Duration) {
        let _ = tokio::time::timeout(max, &mut self.stderr_task).await;
    }

    /// Kill at once, without grace (a violation, a missed ping, a tool at shutdown): kill descendants,
    /// kill the process group, then reap the leader.
    pub async fn kill(&mut self) {
        let pid = self.child.pid() as i32;
        kill_walk(pid, self.child.pgid());
        self.conn.close();
        self.finish().await;
    }

    /// Reap the leader, make sure nothing of the group is left and close the books.
    async fn finish(&mut self) {
        self.conn.close();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = tokio::time::timeout(REAP_WAIT, self.child.wait()).await;
        }
        // members that outlived the leader (a grandchild in the group)
        if group_exists(self.child.pgid()) {
            let _ = kill_group(self.child.pgid(), Signal::SIGKILL);
        }
        self.child.disarm();
        self.stderr_task.abort();
    }

    /// SIGKILL the group without waiting, for the second signal of a shutdown (`kill_all_now`).
    pub fn kill_group_now(&self) {
        kill_walk(self.child.pid() as i32, self.child.pgid());
    }
}

impl Drop for AdapterProc {
    fn drop(&mut self) {
        // GroupChild kills the group while armed; the connection and the log reader go with us
        self.conn.close();
        self.stderr_task.abort();
    }
}

/// The bytes held by the files under `dir`, visiting at most `max_entries` entries. A
/// directory that cannot be read counts as empty; links are not followed.
pub fn dir_usage(dir: &Path, max_entries: usize) -> u64 {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > max_entries {
                return total;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Create the adapter's state directory (0700) if it is missing.
fn ensure_state_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

// ---------------------------------------------------------------------------------------------
// The kill walk
// ---------------------------------------------------------------------------------------------

/// `(pid, ppid)` of every process, from `/proc`. A process that vanishes while the directory is
/// read is skipped.
fn process_parents() -> Vec<(i32, i32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // the command name may hold spaces and parentheses: count from the last ')'
            let rest = &stat[stat.rfind(')')? + 1..];
            let mut fields = rest.split_whitespace();
            let _state = fields.next()?;
            let ppid = fields.next()?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

/// The descendants of `root` by parent pid (at most [`WALK_MAX`]).
fn descendants(root: i32) -> Vec<i32> {
    let table = process_parents();
    let mut found: Vec<i32> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &table {
            if *ppid == parent && *pid != root && !found.contains(pid) {
                found.push(*pid);
                frontier.push(*pid);
                if found.len() >= WALK_MAX {
                    return found;
                }
            }
        }
    }
    found
}

/// Kill an adapter and everything under it: SIGSTOP the leader so it can start nothing
/// more, SIGKILL each descendant found by parent pid (this reaches helpers that left the process
/// group with `setsid` while their parent is alive), then SIGKILL the group. Helpers that were
/// already orphaned cannot be found through the leader.
pub fn kill_walk(pid: i32, pgid: i32) {
    if pid <= 1 {
        return;
    }
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), Signal::SIGSTOP);
    for d in descendants(pid) {
        if d > 1 {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(d), Signal::SIGKILL);
        }
    }
    let _ = kill_group(pgid, Signal::SIGKILL);
    // a stopped leader still dies of SIGKILL, but make sure the stop does not hold it
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
}

// ---------------------------------------------------------------------------------------------
// stderr
// ---------------------------------------------------------------------------------------------

/// Read the adapter's stderr line by line (8 KiB per line, 1 MiB per second, the rest dropped),
/// keep the last lines in memory and write the rest to its own rotated log.
async fn read_stderr(
    stderr: tokio::process::ChildStderr,
    log_path: PathBuf,
    tail: Arc<Mutex<VecDeque<String>>>,
    dropped: Arc<AtomicU64>,
) {
    let sink = open_log_thread(&log_path);
    let mut reader = BufReader::new(stderr);
    let mut bucket = TokenBucket::new(
        STDERR_BYTES_PER_SEC,
        STDERR_BYTES_PER_SEC,
        tokio::time::Instant::now(),
    );
    let mut line: Vec<u8> = Vec::new();
    let mut overlong = false;

    let mut emit = |line: &mut Vec<u8>, overlong: &mut bool| {
        let text = String::from_utf8_lossy(line)
            .trim_end_matches('\r')
            .to_string();
        let cost = line.len() as f64 + 1.0;
        line.clear();
        *overlong = false;
        if !bucket.try_take(cost, tokio::time::Instant::now()) {
            dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        {
            let mut tail = tail.lock();
            if tail.len() >= STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(text.clone());
        }
        if let Some(tx) = &sink {
            let _ = tx.try_send(text);
        }
    };

    loop {
        let buf = match reader.fill_buf().await {
            Ok(b) if !b.is_empty() => b,
            _ => break,
        };
        let (take, newline) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if !overlong {
            let room = STDERR_LINE_MAX.saturating_sub(line.len());
            line.extend_from_slice(&buf[..take.min(room)]);
            if take > room {
                overlong = true;
            }
        }
        reader.consume(take + usize::from(newline));
        if newline {
            emit(&mut line, &mut overlong);
        }
    }
    if !line.is_empty() {
        emit(&mut line, &mut overlong);
    }
}

/// A thread that writes lines to the adapter's rotated stderr log, so a slow disk never holds the
/// reader (and with it the adapter). `None` when the log cannot be opened: stderr is still read.
fn open_log_thread(path: &Path) -> Option<SyncSender<String>> {
    let (tx, rx) = sync_channel::<String>(1024);
    let private_path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("adapter-stderr".into())
        .spawn(move || {
            let result = (|| -> io::Result<()> {
                let dir = open_log_dir(&private_path)?;
                let name = private_path
                    .file_name()
                    .ok_or_else(|| io::Error::other("missing log filename"))?;
                for line in rx {
                    write_log_line(&dir, name, &line)?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                warn!(
                    "Cannot write the adapter log {}: {e}",
                    private_path.display()
                );
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            warn!("Cannot start the adapter log thread: {e}");
            None
        }
    }
}

fn open_log_dir(path: &Path) -> io::Result<File> {
    let invalid = || io::Error::other("missing log directory");
    let dir = path.parent().ok_or_else(invalid)?;
    let root = dir.parent().ok_or_else(invalid)?;
    let home = root.parent().ok_or_else(invalid)?;
    let flags = OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits())
        .open(home)?;
    for part in [root, dir] {
        let name = part.file_name().ok_or_else(invalid)?;
        match mkdirat(Some(parent.as_raw_fd()), name, Mode::S_IRWXU) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => return Err(e.into()),
        }
        let fd = openat(Some(parent.as_raw_fd()), name, flags, Mode::empty())?;
        // openat returned a new descriptor owned by this thread.
        parent = unsafe { File::from_raw_fd(fd) };
        if parent.metadata()?.permissions().mode() & 0o777 != 0o700 {
            parent.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(parent)
}

fn log_file(dir: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let flags = OFlag::O_WRONLY
        | OFlag::O_APPEND
        | OFlag::O_CREAT
        | OFlag::O_NOFOLLOW
        | OFlag::O_NONBLOCK
        | OFlag::O_CLOEXEC;
    let fd = openat(
        Some(dir.as_raw_fd()),
        name,
        flags,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    // The open descriptor is the file we validate and subsequently write.
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("the log is not a regular file"));
    }
    if meta.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn write_log_line(dir: &File, name: &std::ffi::OsStr, line: &str) -> io::Result<()> {
    // Tool invocations and sessions may write to the same log concurrently.
    let dir = Flock::lock(dir.try_clone()?, FlockArg::LockExclusive)
        .map_err(|(_, e)| io::Error::from(e))?;
    let mut file = log_file(&dir, name)?;
    if file.metadata()?.len() + line.len() as u64 + 1 > STDERR_LOG_SIZE {
        let fd = Some(dir.as_raw_fd());
        let archive_name = |i: usize| {
            let mut part = name.to_os_string();
            part.push(format!(".{i}"));
            part
        };
        for i in (1..STDERR_LOG_FILES - 1).rev() {
            let from = archive_name(i);
            match fstatat(fd, from.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) if SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFREG => {
                    renameat(fd, from.as_os_str(), fd, archive_name(i + 1).as_os_str())?;
                }
                Err(nix::errno::Errno::ENOENT) => {}
                Ok(_) => return Err(io::Error::other("a rotated log is not a regular file")),
                Err(e) => return Err(e.into()),
            }
        }
        renameat(fd, name, fd, archive_name(1).as_os_str())?;
        file = log_file(&dir, name)?;
    }
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_log_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("adapter-logs");
        let outside = home.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &root).unwrap();
        let log = root.join("adapter/stderr.log");
        assert!(open_log_dir(&log).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());

        std::fs::remove_file(&root).unwrap();
        let dir = open_log_dir(&log).unwrap();
        let target = outside.join("keep");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, &log).unwrap();
        assert!(write_log_line(&dir, log.file_name().unwrap(), "log line").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn stderr_log_rejects_a_fifo_with_a_reader() {
        use std::os::unix::fs::FileTypeExt;

        let home = tempfile::tempdir().unwrap();
        let log = home.path().join("adapter-logs/adapter/stderr.log");
        let dir = open_log_dir(&log).unwrap();
        nix::unistd::mkfifo(&log, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(&log)
            .unwrap();
        let error = write_log_line(&dir, log.file_name().unwrap(), "log line").unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert!(std::fs::symlink_metadata(&log)
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[test]
    fn dir_usage_sums_files_below_the_directory_and_stops_at_the_entry_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 2000]).unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("link")).unwrap();
        // the link counts as its own few bytes; /etc is not walked
        assert_eq!(dir_usage(dir.path(), 100), 3000 + "/etc".len() as u64);
        // the cap bounds the walk: three entries below, a cap of one visits one of them
        assert!(dir_usage(dir.path(), 1) <= 2000);
        assert_eq!(dir_usage(&dir.path().join("missing"), 10), 0);
    }

    #[test]
    fn the_environment_holds_only_what_the_manifest_passes() {
        let source = |k: &str| match k {
            "FIXTURE_SCRIPT" => Some("/x/script.json".to_string()),
            "SECRET_TOKEN" => Some("hunter2".to_string()),
            _ => None,
        };
        let env = adapter_env(
            Path::new("/h/state/a"),
            &["FIXTURE_SCRIPT".into(), "MISSING".into()],
            &source,
        );
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            keys.contains(&"FIXTURE_SCRIPT") && keys.contains(&"PATH") && keys.contains(&"HOME")
        );
        assert!(!keys.contains(&"SECRET_TOKEN") && !keys.contains(&"MISSING"));
        assert!(env.contains(&("HOME".to_string(), "/h/state/a".to_string())));
        assert!(env.contains(&("OXIDEPM_ADAPTER_PROTOCOLS".to_string(), "1".to_string())));
        // a manifest that passes HOME gets the daemon's
        let source = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
        let env = adapter_env(Path::new("/h/state/a"), &["HOME".into()], &source);
        assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);
        assert!(env.contains(&("HOME".to_string(), "/home/u".to_string())));
    }

    #[test]
    fn open_verified_checks_the_digest_of_the_open_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("adapter");
        std::fs::write(&path, b"abc").unwrap();
        // sha256("abc")
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(open_verified(&path, good).is_ok());
        assert!(
            open_verified(&path, &good.to_uppercase()).is_ok(),
            "hex case does not matter"
        );
        let bad = "0".repeat(64);
        assert!(matches!(
            open_verified(&path, &bad),
            Err(ProcError::Tampered(_))
        ));
        assert!(matches!(
            open_verified(&tmp.path().join("nope"), good),
            Err(ProcError::Missing(_))
        ));
        // a symbolic link is not followed
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_verified(&link, good).is_err());
        // a directory is not an executable
        assert!(open_verified(tmp.path(), good).is_err());
    }

    #[test]
    fn the_walk_finds_children_and_grandchildren_but_not_itself() {
        use std::process::{Command, Stdio};
        // sh -c 'sleep 33.7 & wait' : one child (sleep) under the shell
        let mut sh = Command::new("sh")
            .args(["-c", "sleep 33.7123 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = sh.id() as i32;
        let mut found = Vec::new();
        for _ in 0..100 {
            found = descendants(pid);
            if !found.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!found.is_empty() && !found.contains(&pid), "{found:?}");
        // the children were started by us: kill exactly what the walk found, then the shell
        for d in &found {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(*d), Signal::SIGKILL);
        }
        let _ = sh.kill();
        let _ = sh.wait();
    }
}
