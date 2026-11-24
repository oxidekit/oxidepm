//! The self-test mode of the launcher: apply the profile exactly as `run` does, then
//! try what the profile forbids and report what the kernel said. Nothing is executed on behalf of
//! an adapter and the report is the only output.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::path::Path;

use oxidepm_sandbox::report::attempt;
use oxidepm_sandbox::{Outcome, SelfTestReport, SelftestPlan};

fn connect(path: &Path) -> Outcome {
    Outcome::of(UnixStream::connect(path))
}

fn connect_abstract(name: &str) -> Outcome {
    use std::os::linux::net::SocketAddrExt;
    let result = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .and_then(|addr| UnixStream::connect_addr(&addr));
    Outcome::of(result)
}

fn write_file(path: &Path) -> Outcome {
    Outcome::of(
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path),
    )
}

fn read_file(path: &Path) -> Outcome {
    Outcome::of(std::fs::File::open(path).and_then(|mut f| {
        let mut one = [0u8; 1];
        f.read(&mut one)
    }))
}

fn inet_socket() -> Outcome {
    // SAFETY: socket(2) takes no pointers; the descriptor is closed at once.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Outcome::of::<()>(Err(std::io::Error::last_os_error()));
    }
    // SAFETY: closing the descriptor just made.
    unsafe { libc::close(fd) };
    Outcome::Allowed
}

fn io_uring_setup() -> Outcome {
    let mut params = [0u8; 120];
    // SAFETY: io_uring_setup writes at most the 120 bytes of io_uring_params.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) };
    if fd < 0 {
        return Outcome::of::<()>(Err(std::io::Error::last_os_error()));
    }
    // SAFETY: closing the descriptor just made.
    unsafe { libc::close(fd as i32) };
    Outcome::Allowed
}

/// Try a program: `Allowed` when it ran, whatever its status.
fn exec(path: &Path) -> Outcome {
    Outcome::of(
        std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
}

/// Run every attempt the plan names.
pub fn run(plan: &SelftestPlan) -> SelfTestReport {
    let mut attempts: BTreeMap<String, Outcome> = BTreeMap::new();
    let mut put = |name: &str, outcome: Outcome| {
        attempts.insert(name.to_string(), outcome);
    };
    if let Some(path) = &plan.write_outside {
        put(attempt::WRITE_OUTSIDE, write_file(path));
    }
    put(attempt::INET_SOCKET, inet_socket());
    put(attempt::IO_URING, io_uring_setup());
    if let Some(path) = &plan.exec {
        put(attempt::EXEC, exec(path));
    }
    if let Some(path) = &plan.daemon_socket {
        put(attempt::DAEMON_SOCKET, connect(path));
    }
    if let Some(path) = &plan.session_bus {
        put(attempt::SESSION_BUS, connect(path));
    }
    if let Some(name) = &plan.abstract_socket {
        put(attempt::ABSTRACT_SOCKET, connect_abstract(name));
    }
    if let Some(path) = &plan.pathname_socket {
        put(attempt::PATHNAME_SOCKET, connect(path));
    }
    if let Some(path) = &plan.read_outside {
        put(attempt::READ_OUTSIDE, read_file(path));
    }
    if let Some(path) = &plan.read_daemon_home {
        put(attempt::READ_DAEMON_HOME, read_file(path));
    }
    SelfTestReport {
        setup_error: None,
        attempts,
    }
}
