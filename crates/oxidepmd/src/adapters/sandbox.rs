//! The daemon's side of the sandbox self-test: what the OS enforced on this machine
//! for a given profile, measured by the launcher's `selftest` mode under that exact profile.
//!
//! The host makes everything the helper tries: the places it may not write, the files it may not
//! read, the sockets it may or may not connect to (it listens on a pathname socket in a private
//! temporary directory and on an abstract socket, so "reachable" is observed and not assumed).
//! `enable` stores the result as the adapter's `enforced_json`.

use std::mem::{offset_of, size_of};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::libc::sockaddr_un;
use oxidepm_sandbox::{Profile, SelfTestReport, SelftestPlan};

/// How long the helper may take: seven file and socket attempts and one exec.
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(20);
/// The pathname occupies the trailing `sun_path` array, with one byte for its NUL.
const MAX_SOCKET_PATH_BYTES: usize =
    size_of::<sockaddr_un>() - offset_of!(sockaddr_un, sun_path) - 1;

/// What the daemon knows that the helper needs.
pub struct SelftestEnv {
    /// The daemon's main socket.
    pub daemon_socket: PathBuf,
    /// A file inside the daemon home (`adapters.db`); the helper tries to read it.
    pub home_file: PathBuf,
    /// Where the private directory of the test is made; `None` uses the system's temporary
    /// directory, falling back to `/tmp` if the socket path is too long.
    pub scratch_base: Option<PathBuf>,
}

/// The session bus socket, if there is one.
fn session_bus() -> Option<PathBuf> {
    let from_env = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .ok()
        .and_then(|a| {
            let path = a
                .split(',')
                .find_map(|part| part.strip_prefix("unix:path="))?;
            Some(PathBuf::from(path))
        });
    from_env.or_else(|| {
        let uid = nix::unistd::getuid().as_raw();
        Some(PathBuf::from(format!("/run/user/{uid}/bus")))
    })
}

fn true_program() -> Option<PathBuf> {
    ["/usr/bin/true", "/bin/true"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// A private directory that is removed again.
struct Scratch(PathBuf);

impl Scratch {
    fn new(base: Option<&Path>) -> Result<(Self, PathBuf), String> {
        let name = "oxidepm-selftest-XXXXXX";
        let mut template = base
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join(name);
        let mut socket = template.join("probe.sock");
        if base.is_none() && socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
            template = Path::new("/tmp").join(name);
            socket = template.join("probe.sock");
        }
        let length = socket.as_os_str().len();
        if length > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "the self-test socket path '{}' is {length} bytes (limit {MAX_SOCKET_PATH_BYTES} bytes); set TMPDIR to a shorter directory",
                socket.display()
            ));
        }
        let path = nix::unistd::mkdtemp(&template)
            .map_err(|e| format!("cannot make a directory for the self-test: {e}"))?;
        let socket = path.join("probe.sock");
        Ok((Scratch(path), socket))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Accept connections on a socket until told to stop; the helper only connects.
fn listen(listener: UnixListener, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    })
}

/// Run the launcher's self-test under `profile` and return the report.
pub async fn selftest(
    launcher: &Path,
    profile: &Profile,
    env: &SelftestEnv,
    allow_unenforced: bool,
) -> Result<SelfTestReport, String> {
    let (scratch, socket_path) = Scratch::new(env.scratch_base.as_deref())?;
    let outside_dir = scratch.0.join("outside");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&outside_dir)
        .map_err(|e| format!("cannot make a directory for the self-test: {e}"))?;
    let read_outside = scratch.0.join("readable.txt");
    std::fs::write(&read_outside, b"x")
        .map_err(|e| format!("cannot make a file for the self-test: {e}"))?;

    // the services the helper tries to reach
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    let pathname = UnixListener::bind(&socket_path)
        .map_err(|e| format!("cannot listen for the self-test: {e}"))?;
    threads.push(listen(pathname, Arc::clone(&stop)));
    let abstract_name = {
        use std::os::linux::net::SocketAddrExt;
        let name = format!(
            "oxidepm-selftest-{}-{}",
            std::process::id(),
            scratch
                .0
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("x")
        );
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .map_err(|e| format!("cannot name the abstract socket: {e}"))?;
        let listener = UnixListener::bind_addr(&addr)
            .map_err(|e| format!("cannot listen for the self-test: {e}"))?;
        threads.push(listen(listener, Arc::clone(&stop)));
        name
    };

    let plan = SelftestPlan {
        write_outside: Some(outside_dir.join("written")),
        read_outside: Some(read_outside),
        read_daemon_home: Some(env.home_file.clone()),
        daemon_socket: Some(env.daemon_socket.clone()),
        session_bus: session_bus(),
        abstract_socket: Some(abstract_name),
        pathname_socket: Some(socket_path),
        exec: true_program(),
    };
    let plan_json = serde_json::to_string(&plan)
        .map_err(|e| format!("cannot write the self-test plan: {e}"))?;
    let result = oxidepm_runtime::run_launcher_selftest(
        launcher,
        &profile.to_json(),
        &plan_json,
        allow_unenforced,
        SELFTEST_TIMEOUT,
    )
    .await;

    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    let text = result?;
    serde_json::from_str(&text)
        .map_err(|e| format!("the sandbox self-test printed something that is not a report: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn scratch_directories_have_a_random_suffix_and_private_mode() {
        let base = tempfile::tempdir().unwrap();
        let (first, socket) = Scratch::new(Some(base.path())).unwrap();
        let (second, _) = Scratch::new(Some(base.path())).unwrap();
        let name = first.0.file_name().unwrap().to_str().unwrap();
        let suffix = name.strip_prefix("oxidepm-selftest-").unwrap();
        assert_eq!(suffix.len(), 6);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        assert_ne!(first.0, second.0);
        assert_eq!(socket.parent(), Some(first.0.as_path()));
        assert_eq!(
            std::fs::metadata(&first.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(first);
        assert!(!socket.parent().unwrap().exists());
    }
}
