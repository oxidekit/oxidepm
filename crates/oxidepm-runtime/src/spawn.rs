//! Process groups: every app process and every build command runs as the leader of its own
//! process group, so that a stop (or a dropped build) reaches the whole tree and not only the
//! direct child.
//!
//! The wrappers launch `npm run`, `go run` and `sh -c ...`, which fork the real program, and
//! signalling only the leader leaves the program running, still holding its port.

use crate::traits::LaunchPlan;
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use oxidepm_core::{Error, Result};
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

/// How often `terminate_group` looks at the group.
const POLL: Duration = Duration::from_millis(25);

/// How long `terminate_group` waits for the leader to be reaped after the SIGKILL.
const REAP_WAIT: Duration = Duration::from_secs(5);

/// A command that will run as the leader of a new process group, with stdin closed and
/// stdout/stderr piped. tokio's `kill_on_drop` is off: it only kills the direct child, so
/// [`GroupChild`] kills the group instead.
pub fn group_command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    cmd
}

/// The environment an app process gets: the daemon's own, or a clean one, plus `vars`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    /// Start from an empty environment instead of the daemon's.
    pub clear: bool,
    /// Set after the base environment, in order (a later entry wins).
    pub vars: Vec<(String, String)>,
}

impl ChildEnv {
    /// The value of `key` in this environment: the last entry of `vars`, else (unless the
    /// environment starts clean) the daemon's own.
    pub fn get(&self, key: &str) -> Option<OsString> {
        if let Some((_, value)) = self.vars.iter().rev().find(|(k, _)| k == key) {
            return Some(OsString::from(value));
        }
        if self.clear {
            None
        } else {
            std::env::var_os(key)
        }
    }

    /// The daemon's whole environment plus `vars`.
    pub fn inherit<I: IntoIterator<Item = (String, String)>>(vars: I) -> Self {
        Self {
            clear: false,
            vars: vars.into_iter().collect(),
        }
    }
}

/// Spawn the process of a plan as the leader of a new process group. The one place an app
/// process is created. stdin is closed, stdout and stderr are piped (the caller hands them to
/// the log capture).
pub fn spawn_group(plan: &LaunchPlan, env: &ChildEnv) -> Result<GroupChild> {
    let mut cmd = group_command(&plan.program);
    cmd.args(&plan.args).current_dir(&plan.cwd);
    if env.clear {
        cmd.env_clear();
    }
    cmd.envs(env.vars.iter().map(|(k, v)| (k, v)));
    GroupChild::spawn(&mut cmd).map_err(|e| {
        Error::ProcessStartFailed(format!(
            "Failed to start '{}': {}",
            plan.program.display(),
            e
        ))
    })
}

/// The sandbox launcher that stands between the daemon and an adapter.
pub struct SandboxLaunch<'a> {
    /// The opened and verified launcher binary, `oxidepm-sandbox`.
    pub launcher: &'a std::fs::File,
    /// The profile it applies, as JSON.
    pub profile_json: &'a str,
    /// Go on when the kernel cannot enforce what the profile requires (the user accepted that).
    pub allow_unenforced: bool,
}

/// How an adapter process is started.
pub struct AdapterLaunch<'a> {
    /// The executable, already opened and hashed by the caller. The process that runs is this open
    /// file, not whatever the path names later.
    pub exe: &'a std::fs::File,
    /// `argv[0]`: the adapter's name.
    pub arg0: &'a str,
    pub args: &'a [String],
    /// The whole environment. Nothing of the daemon's own is inherited.
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    /// The launcher, when the adapter is to be sandboxed. Without it the adapter is executed
    /// directly (no OS restriction at all; only the daemon-side checks hold).
    pub sandbox: Option<SandboxLaunch<'a>>,
    /// A terminal (or a pipe) to run the adapter on. The adapter's standard input and output are
    /// duplicates of these two descriptors, so they cannot carry the protocol and the child has
    /// no pipe of its own on them (`stdin` and `stdout` of the returned child are `None`).
    /// Standard error is still a pipe: it is the adapter's log, never the terminal.
    pub terminal: Option<(&'a std::os::fd::OwnedFd, &'a std::os::fd::OwnedFd)>,
}

/// The descriptor on which the launcher receives the verified executable.
const LAUNCHER_EXE_FD: std::os::fd::RawFd = 3;

/// Spawn an adapter as the leader of its own process group, with stdin, stdout and stderr all
/// piped: stdin and stdout carry the protocol, stderr is the adapter's log. The one place an
/// adapter process is created (`spawn_group` and `group_command` are not used for adapters: they
/// close stdin).
///
/// Without a sandbox the program is executed through `/proc/self/fd/<n>` of the open file
/// `launch.exe`. With one, the child is the launcher `oxidepm-sandbox`, and the open file is handed
/// to it on descriptor 3 (the one `pre_exec` below: a `dup2` or `fcntl`, both async-signal-safe,
/// that makes the descriptor survive the exec); the launcher applies the profile and executes that
/// descriptor. Either way replacing the file on disk between the hash check and the exec changes
/// nothing. That works for an ELF executable, which is all import accepts (a `#!` script cannot be
/// executed from a descriptor).
pub fn spawn_adapter(launch: &AdapterLaunch<'_>) -> Result<GroupChild> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let fd = launch.exe.as_raw_fd();
    let launcher_fd;
    let mut cmd = match &launch.sandbox {
        None => {
            let mut cmd = Command::new(format!("/proc/self/fd/{fd}"));
            cmd.arg0(launch.arg0).args(launch.args);
            cmd
        }
        Some(sandbox) => {
            // Keep the launcher's descriptor clear of the slot reserved for the adapter.
            let raw_fd = nix::fcntl::fcntl(
                sandbox.launcher.as_raw_fd(),
                nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(LAUNCHER_EXE_FD + 1),
            )
            .map_err(|e| {
                Error::ProcessStartFailed(format!(
                    "Cannot duplicate the sandbox launcher descriptor: {e}"
                ))
            })?;
            // SAFETY: fcntl returned a new descriptor owned by this OwnedFd.
            launcher_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let mut cmd = Command::new(format!("/proc/self/fd/{}", launcher_fd.as_raw_fd()));
            cmd.arg("run").arg("--profile").arg(sandbox.profile_json);
            if sandbox.allow_unenforced {
                cmd.arg("--allow-unenforced");
            }
            cmd.arg("--arg0")
                .arg(launch.arg0)
                .arg("--")
                .args(launch.args);
            // SAFETY: the closure runs between fork and exec and calls only dup2 and fcntl, which
            // are async-signal-safe and touch no memory of the parent.
            unsafe {
                cmd.pre_exec(move || {
                    if fd == LAUNCHER_EXE_FD {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    } else if libc::dup2(fd, LAUNCHER_EXE_FD) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            cmd
        }
    };
    cmd.env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(launch.cwd)
        .process_group(0)
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    match launch.terminal {
        Some((input, output)) => {
            let duplicate = |fd: &std::os::fd::OwnedFd| {
                fd.try_clone().map_err(|e| {
                    Error::ProcessStartFailed(format!(
                        "Failed to start adapter '{}': cannot duplicate the terminal: {}",
                        launch.arg0, e
                    ))
                })
            };
            cmd.stdin(Stdio::from(duplicate(input)?))
                .stdout(Stdio::from(duplicate(output)?));
        }
        None => {
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        }
    }
    let first_extra_fd = if launch.sandbox.is_some() {
        LAUNCHER_EXE_FD + 1
    } else {
        3
    };
    GroupChild::spawn_closing_from(&mut cmd, first_extra_fd).map_err(|e| {
        Error::ProcessStartFailed(format!("Failed to start adapter '{}': {}", launch.arg0, e))
    })
}

/// Run the launcher's self-test: `oxidepm-sandbox selftest` under `profile_json`,
/// started like an adapter (own process group, scrubbed environment, no stdin) so that the
/// attempts run in the same kind of process an adapter would. Returns what the launcher printed on
/// stdout, or the reason it printed nothing: its last line on stderr, or that it ran out of time
/// (the group is killed then).
pub async fn run_launcher_selftest(
    launcher: &Path,
    profile_json: &str,
    plan_json: &str,
    allow_unenforced: bool,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let mut cmd = Command::new(launcher);
    cmd.arg("selftest")
        .arg("--profile")
        .arg(profile_json)
        .arg("--plan")
        .arg(plan_json);
    if allow_unenforced {
        cmd.arg("--allow-unenforced");
    }
    cmd.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    let mut child = GroupChild::spawn(&mut cmd)
        .map_err(|e| format!("cannot start the sandbox launcher: {e}"))?;
    let (Some(stdout), Some(stderr)) = (child.take_stdout(), child.take_stderr()) else {
        return Err("the sandbox launcher's pipes are missing".to_string());
    };
    let work = async {
        async fn read_capped(
            mut pipe: impl tokio::io::AsyncRead + Unpin,
            cap: u64,
        ) -> io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            (&mut pipe).take(cap).read_to_end(&mut bytes).await?;
            tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await?;
            Ok(bytes)
        }

        let (out, err) =
            tokio::try_join!(read_capped(stdout, 1 << 20), read_capped(stderr, 64 * 1024))
                .map_err(|e| format!("cannot read the sandbox launcher: {e}"))?;
        let status = child
            .wait()
            .await
            .map_err(|e| format!("cannot wait for the sandbox launcher: {e}"))?;
        Ok::<_, String>((status, out, err))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok((status, out, _))) if status.success() => {
            child.disarm();
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Ok(Ok((status, _, err))) => {
            child.disarm();
            let text = String::from_utf8_lossy(&err);
            let line = text.lines().last().unwrap_or("").trim().to_string();
            Err(if line.is_empty() {
                format!("the sandbox launcher failed ({status})")
            } else {
                line
            })
        }
        Ok(Err(e)) => Err(e),
        // dropping `child` kills the group
        Err(_) => Err(format!(
            "the sandbox self-test did not finish within {timeout:?}"
        )),
    }
}

/// Mark excess descriptors for closure at exec, preserving the command's exec error pipe.
fn close_on_exec_from(first: libc::c_int) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the syscall changes only descriptor flags in the forked process.
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                first as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
    }

    // On older kernels, set the same flag with async-signal-safe fcntl calls.
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes the limits when it succeeds.
    let limit = unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        limit.assume_init()
    };
    let end = limit.rlim_cur.min(libc::c_int::MAX as libc::rlim_t) as libc::c_int;
    for fd in first..end {
        // SAFETY: fcntl changes only the close-on-exec flag for this child.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// A child that leads its own process group (`pgid == pid`).
///
/// While armed, dropping it sends SIGKILL to the whole group, so a future that owns a child and
/// is dropped (or panics) leaves no process behind. [`terminate_group`] disarms it once the
/// group is gone, so a recycled group id is never signalled.
pub struct GroupChild {
    child: Child,
    pgid: i32,
    armed: bool,
}

impl GroupChild {
    /// Wrap a child that was spawned through [`group_command`]. `None` if it has no pid (it has
    /// already been reaped).
    pub fn new(child: Child) -> Option<Self> {
        let pgid = child.id()? as i32;
        Some(Self {
            child,
            pgid,
            armed: true,
        })
    }

    /// Spawn `cmd` (built by [`group_command`]) and wrap the child.
    pub fn spawn(cmd: &mut Command) -> io::Result<Self> {
        Self::spawn_closing_from(cmd, 3)
    }

    fn spawn_closing_from(cmd: &mut Command, first_extra_fd: libc::c_int) -> io::Result<Self> {
        // SAFETY: between fork and exec, this uses only descriptor and resource-limit syscalls.
        unsafe {
            cmd.pre_exec(move || close_on_exec_from(first_extra_fd));
        }
        let child = cmd.spawn()?;
        Self::new(child).ok_or_else(|| io::Error::other("process started but no PID available"))
    }

    /// Process id of the leader.
    pub fn pid(&self) -> u32 {
        self.pgid as u32
    }

    /// Process group id (equal to the leader's pid).
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Do not kill the group when this value is dropped.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Wait for the leader to exit, then kill remaining group members before reaping.
    pub(crate) async fn finish_group(&mut self) -> io::Result<ExitStatus> {
        use tokio::signal::unix::{signal, SignalKind};

        let mut exited = signal(SignalKind::child())?;
        loop {
            // SAFETY: waitid writes to the initialized siginfo. WNOWAIT leaves the leader
            // unreaped, keeping its PID reserved until the group has been signalled.
            let ready = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let result = libc::waitid(
                    libc::P_PID,
                    self.pgid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                );
                if result < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(info.si_pid() != 0)
                }
            };
            match ready {
                Ok(true) => break,
                Ok(false) => {
                    exited.recv().await;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        kill_group(self.pgid, Signal::SIGKILL)?;
        self.disarm();
        self.child.wait().await
    }

    /// SIGKILL the whole group, reap the leader (bounded) and disarm.
    pub async fn kill_and_wait(&mut self) {
        if !self.armed {
            return;
        }
        let _ = kill_group(self.pgid, Signal::SIGKILL);
        let _ = tokio::time::timeout(REAP_WAIT, self.child.wait()).await;
        self.armed = false;
    }
}

impl Deref for GroupChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for GroupChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_group(self.pgid, Signal::SIGKILL);
        }
    }
}

/// Send `sig` to every process of group `pgid`. A group that no longer exists is not an error.
pub fn kill_group(pgid: i32, sig: Signal) -> io::Result<()> {
    if pgid <= 1 {
        // killpg(0) is the caller's own group and 1 is init's: never a group we spawned
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal process group {pgid}"),
        ));
    }
    match killpg(Pid::from_raw(pgid), sig) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(e) => Err(io::Error::from(e)),
    }
}

/// Does any process (a zombie included) still belong to group `pgid`?
pub fn group_exists(pgid: i32) -> bool {
    pgid > 1 && !matches!(killpg(Pid::from_raw(pgid), None), Err(Errno::ESRCH))
}

/// What `terminate_group` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminated {
    /// How the leader exited (`None` if it could not be reaped).
    pub status: Option<ExitStatus>,
    /// The group had to be killed because it was still there when the timeout ended.
    pub escalated: bool,
}

/// Stop a whole process group: SIGTERM to the group, wait until the leader has exited and the
/// group is empty or `timeout` is up, then SIGKILL the group and reap the leader. Members that
/// outlived the leader are killed too. The child is disarmed on return.
pub async fn terminate_group(child: &mut GroupChild, timeout: Duration) -> Terminated {
    let pgid = child.pgid;
    let _ = kill_group(pgid, Signal::SIGTERM);

    let deadline = Instant::now() + timeout;
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        if status.is_some() && !group_exists(pgid) {
            child.disarm();
            return Terminated {
                status,
                escalated: false,
            };
        }
        if Instant::now() >= deadline {
            let _ = kill_group(pgid, Signal::SIGKILL);
            child.disarm();
            break;
        }
        tokio::time::sleep(POLL).await;
    }

    if status.is_none() {
        status = match tokio::time::timeout(REAP_WAIT, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => None,
        };
    }
    Terminated {
        status,
        escalated: true,
    }
}

/// A shell script to run: a lifecycle hook or a script health probe.
#[derive(Debug, Clone)]
pub struct ScriptSpec {
    /// The text handed to `sh -c`.
    pub script: String,
    /// Working directory; `None` is the daemon's own.
    pub cwd: Option<PathBuf>,
    /// The environment the script gets.
    pub env: ChildEnv,
    /// The script and everything it started in its group is killed after this long.
    pub timeout: Duration,
    /// File that gets stdout and stderr (opened for append, created with mode 0600). The file
    /// itself is the child's stdio, so no pipe can fill and a background process that outlives
    /// the script cannot block the caller. `None` sends both to `/dev/null`.
    pub output: Option<PathBuf>,
}

/// How a script ended.
#[derive(Debug)]
pub enum ScriptOutcome {
    /// The shell exited (the script's own exit status).
    Exited(ExitStatus),
    /// The timeout was up: the whole process group was killed and the leader reaped.
    TimedOut,
    /// The shell could not be started, or its output file could not be opened.
    SpawnFailed(String),
}

impl ScriptOutcome {
    /// Did the script exit with status 0?
    pub fn success(&self) -> bool {
        matches!(self, ScriptOutcome::Exited(status) if status.success())
    }
}

/// Run a script with `sh -c` in its own process group and wait for it.
///
/// On timeout the group is SIGKILLed (the script and everything it started, such as a
/// `sleep 1000 &`) and the leader is reaped. A script that exits by itself is not followed up:
/// what it left running in the background stays. If the returned future is dropped the group is
/// killed.
pub async fn run_script(spec: ScriptSpec) -> ScriptOutcome {
    run_script_with(spec, |_| {}).await
}

/// [`run_script`], telling `on_spawn` the process group id as soon as the shell is running
/// (the caller can then kill the group from outside, for example at shutdown).
pub async fn run_script_with(spec: ScriptSpec, on_spawn: impl FnOnce(i32)) -> ScriptOutcome {
    let mut cmd = group_command("sh");
    cmd.arg("-c").arg(&spec.script);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    if spec.env.clear {
        cmd.env_clear();
    }
    cmd.envs(spec.env.vars.iter().map(|(k, v)| (k, v)));
    match &spec.output {
        Some(path) => match open_append(path).and_then(|f| Ok((f.try_clone()?, f))) {
            Ok((out, err)) => {
                cmd.stdout(out).stderr(err);
            }
            Err(e) => {
                return ScriptOutcome::SpawnFailed(format!(
                    "cannot open script output {}: {e}",
                    path.display()
                ))
            }
        },
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    let mut child = match GroupChild::spawn(&mut cmd) {
        Ok(child) => child,
        Err(e) => return ScriptOutcome::SpawnFailed(format!("cannot run sh: {e}")),
    };
    on_spawn(child.pgid());

    match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => {
            child.disarm();
            ScriptOutcome::Exited(status)
        }
        Ok(Err(e)) => {
            child.kill_and_wait().await;
            ScriptOutcome::SpawnFailed(format!("waiting for the script failed: {e}"))
        }
        Err(_) => {
            child.kill_and_wait().await;
            ScriptOutcome::TimedOut
        }
    }
}

fn open_append(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn launcher_selftest_drains_output_after_the_capture_limit() {
        let launcher =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verbose-launcher.sh");
        let report =
            run_launcher_selftest(&launcher, "{}", "{}", false, Duration::from_secs(5)).await;
        assert_eq!(report.unwrap(), "report");
    }

    fn pgid_of(pid: u32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(2)?.parse().ok()
    }

    /// Live (non-zombie) `sleep <marker>` processes (the command line is exactly that, so the
    /// shell whose script mentions the marker does not count).
    fn procs_with(marker: &str) -> Vec<u32> {
        let wanted = format!("sleep {marker}");
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| {
                    s[s.rfind(')').unwrap_or(0) + 1..]
                        .trim_start()
                        .starts_with('Z')
                })
                .unwrap_or(true);
            if cmdline.trim() == wanted && !zombie {
                found.push(pid);
            }
        }
        found
    }

    async fn gone(marker: &str) -> bool {
        for _ in 0..100 {
            if procs_with(marker).is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn sh(script: &str, cwd: &Path) -> Command {
        let mut cmd = group_command("sh");
        cmd.arg("-c").arg(script).current_dir(cwd);
        cmd
    }

    /// A marker no other process on the machine carries.
    fn marker(tag: u32) -> String {
        format!("301.{:05}{:03}", std::process::id() % 100_000, tag)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_children_do_not_inherit_extra_descriptors() {
        use std::os::fd::AsRawFd;

        let file = tempfile::tempfile().unwrap();
        let fd = file.as_raw_fd();
        let cwd = tempfile::tempdir().unwrap();
        let mut cmd = sh(&format!("test ! -e /proc/self/fd/{fd}"), cwd.path());
        // Simulate a descriptor inherited from the process that launched the daemon.
        // SAFETY: only the forked child's copy of this descriptor is changed.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = GroupChild::spawn(&mut cmd).unwrap();
        let exit = child.wait().await.unwrap();
        child.disarm();
        assert!(
            exit.success(),
            "the managed process inherited descriptor {fd}"
        );
    }

    #[tokio::test]
    async fn spawn_group_sets_pgid_to_pid() {
        let dir = tempfile::tempdir().unwrap();
        let m = marker(1);
        let mut child = GroupChild::spawn(&mut sh(&format!("exec sleep {m}"), dir.path())).unwrap();
        let pid = child.pid();
        assert_eq!(child.pgid(), pid as i32);
        assert_eq!(pgid_of(pid), Some(pid as i32), "the kernel agrees");
        let ours = nix::unistd::getpgrp().as_raw();
        assert_ne!(pid as i32, ours, "not the group of the test process");
        child.kill_and_wait().await;
    }

    #[tokio::test]
    async fn kill_group_kills_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let m = marker(2);
        let mut child =
            GroupChild::spawn(&mut sh(&format!("sleep {m} & wait"), dir.path())).unwrap();
        for _ in 0..100 {
            if !procs_with(&m).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(procs_with(&m).len(), 1, "the grandchild runs");

        kill_group(child.pgid(), Signal::SIGKILL).unwrap();
        child.wait().await.unwrap();
        child.disarm();
        assert!(gone(&m).await, "the grandchild died with the group");
    }

    #[tokio::test]
    async fn kill_group_esrch_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = GroupChild::spawn(&mut sh("exit 0", dir.path())).unwrap();
        let pgid = child.pgid();
        child.wait().await.unwrap();
        child.disarm();
        assert!(!group_exists(pgid));
        kill_group(pgid, Signal::SIGTERM).expect("a group that is gone is not an error");
        kill_group(pgid, Signal::SIGKILL).expect("a group that is gone is not an error");
    }

    #[tokio::test]
    async fn kill_group_refuses_group_zero_and_one() {
        assert!(kill_group(0, Signal::SIGTERM).is_err());
        assert!(kill_group(1, Signal::SIGTERM).is_err());
        assert!(kill_group(-5, Signal::SIGTERM).is_err());
    }

    #[tokio::test]
    async fn terminate_group_term_then_kill_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let m = marker(4);
        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; (trap '' TERM; exec sleep {m}) & touch '{}'; wait",
            ready.display()
        );
        let mut child = GroupChild::spawn(&mut sh(&script, dir.path())).unwrap();
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready.exists());
        // give the subshell a moment to exec
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = Instant::now();
        let done = terminate_group(&mut child, Duration::from_millis(400)).await;
        assert!(done.escalated, "TERM was ignored, so the group was killed");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(gone(&m).await, "the member died too");
        assert!(!group_exists(child.pgid()));
    }

    #[tokio::test]
    async fn terminate_group_graceful_exit_is_not_escalated() {
        let dir = tempfile::tempdir().unwrap();
        let m = marker(5);
        let ready = dir.path().join("ready");
        let script = format!(
            "trap 'exit 7' TERM; touch '{}'; sleep {m} & wait",
            ready.display()
        );
        let mut child = GroupChild::spawn(&mut sh(&script, dir.path())).unwrap();
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let done = terminate_group(&mut child, Duration::from_secs(5)).await;
        assert!(!done.escalated);
        assert_eq!(done.status.and_then(|s| s.code()), Some(7));
        assert!(gone(&m).await, "the sleeping member got the TERM too");
    }

    fn script(text: &str, timeout: Duration, output: Option<PathBuf>) -> ScriptSpec {
        ScriptSpec {
            script: text.to_string(),
            cwd: None,
            env: ChildEnv::inherit(Vec::new()),
            timeout,
            output,
        }
    }

    #[tokio::test]
    async fn run_script_kills_group_on_timeout() {
        let m = marker(10);
        let started = Instant::now();
        let outcome = run_script(script(
            &format!("sleep {m} & wait"),
            Duration::from_millis(400),
            None,
        ))
        .await;
        assert!(matches!(outcome, ScriptOutcome::TimedOut), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            gone(&m).await,
            "the script's background child died with the group"
        );
    }

    #[tokio::test]
    async fn run_script_output_goes_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hooks.log");
        std::fs::write(&log, "earlier\n").unwrap();
        let outcome = run_script(script(
            "echo to-out; echo to-err >&2; exit 3",
            Duration::from_secs(10),
            Some(log.clone()),
        ))
        .await;
        match outcome {
            ScriptOutcome::Exited(status) => assert_eq!(status.code(), Some(3)),
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.starts_with("earlier\n"),
            "appended, not truncated: {text:?}"
        );
        assert!(
            text.contains("to-out") && text.contains("to-err"),
            "{text:?}"
        );

        let again = run_script(script(
            "echo second",
            Duration::from_secs(10),
            Some(log.clone()),
        ))
        .await;
        assert!(again.success());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.contains("to-out") && text.contains("second"),
            "{text:?}"
        );
    }

    #[tokio::test]
    async fn run_script_output_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hooks.log");
        run_script(script("echo x", Duration::from_secs(10), Some(log.clone()))).await;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn run_script_does_not_wait_for_a_background_child_that_holds_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hooks.log");
        let m = marker(11);
        let mut pgid = None;
        let started = Instant::now();
        let outcome = run_script_with(
            script(
                &format!("sleep {m} & echo started"),
                Duration::from_secs(10),
                Some(log),
            ),
            |id| pgid = Some(id),
        )
        .await;
        let elapsed = started.elapsed();
        // The shell can exit before its child has called exec and shows up as sleep.
        let background = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pids = procs_with(&m);
                if !pids.is_empty() {
                    break pids;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        kill_group(pgid.expect("the script started"), Signal::SIGKILL).unwrap();

        assert!(outcome.success(), "{outcome:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(background.expect("the background child survived").len(), 1);
        assert!(gone(&m).await);
    }

    #[tokio::test]
    async fn run_script_reports_the_group_to_the_caller() {
        let m = marker(12);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_script_with(
            script(&format!("sleep {m} & wait"), Duration::from_secs(30), None),
            move |pgid| {
                let _ = tx.send(pgid);
            },
        ));
        let pgid = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("pgid reported in time")
            .unwrap();
        assert!(pgid > 1 && group_exists(pgid));
        kill_group(pgid, Signal::SIGKILL).unwrap();
        let outcome = task.await.unwrap();
        assert!(
            matches!(outcome, ScriptOutcome::Exited(s) if !s.success()),
            "{outcome:?}"
        );
        assert!(gone(&m).await);
    }

    #[tokio::test]
    async fn run_script_unopenable_output_is_a_spawn_failure() {
        let outcome = run_script(script(
            "echo x",
            Duration::from_secs(10),
            Some(PathBuf::from("/nonexistent-dir-for-oxidepm-test/hooks.log")),
        ))
        .await;
        assert!(
            matches!(outcome, ScriptOutcome::SpawnFailed(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn run_script_env_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("env.out");
        // HOME and PATH are set in the test process, so a cleared environment must lose them
        assert!(std::env::var_os("HOME").is_some() && std::env::var_os("PATH").is_some());
        let mut spec = script("export -p", Duration::from_secs(10), Some(log.clone()));
        spec.env = ChildEnv {
            clear: true,
            vars: vec![("FROM_SPEC".to_string(), "yes".to_string())],
        };
        assert!(run_script(spec).await.success());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("FROM_SPEC"), "{text}");
        assert!(
            !text.contains("HOME=") && !text.contains("PATH="),
            "the daemon's environment leaked:\n{text}"
        );

        // the same script without a clean environment sees the test process's
        let inheriting = dir.path().join("inherit.out");
        assert!(run_script(script(
            "export -p",
            Duration::from_secs(10),
            Some(inheriting.clone())
        ))
        .await
        .success());
        assert!(std::fs::read_to_string(&inheriting)
            .unwrap()
            .contains("HOME="));
    }
}
