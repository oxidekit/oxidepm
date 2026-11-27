//! Apply process limits and restrictions before running the adapter.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

use oxidepm_adapter_proto::permissions::Enforce;
use oxidepm_sandbox::{Profile, EXE_FD};

/// What the kernel enforced of the profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enforced {
    pub landlock: bool,
    pub seccomp: bool,
}

/// The items of `require` that `enforced` leaves open: filesystem and exec rules need Landlock,
/// the network rule needs the seccomp filter.
pub fn missing(profile: &Profile, enforced: Enforced) -> Vec<Enforce> {
    profile
        .require
        .iter()
        .copied()
        .filter(|item| match item {
            Enforce::Network => !enforced.seccomp,
            Enforce::FsWrite | Enforce::FsRead | Enforce::Spawn => !enforced.landlock,
        })
        .collect()
}

/// Restrict this process as the profile says. A step that goes wrong is an error unless the user
/// accepted an unenforced adapter; a rule the kernel cannot express is an error only for an item
/// the profile requires.
pub fn restrict(
    profile: &Profile,
    allow_unenforced: bool,
    with_exe: bool,
) -> Result<Enforced, String> {
    if with_exe {
        check_executable_descriptor()?;
    }
    apply_baseline(profile)?;
    let mut enforced = Enforced::default();
    match crate::landlock_fs::apply(profile, with_exe) {
        Ok(on) => enforced.landlock = on,
        Err(e) if allow_unenforced => {
            eprintln!("oxidepm-sandbox: {e} (continuing: unenforced adapters are allowed)")
        }
        Err(e) => return Err(e),
    }
    match crate::seccomp_filter::apply(profile) {
        Ok(on) => enforced.seccomp = on,
        Err(e) if allow_unenforced => {
            eprintln!("oxidepm-sandbox: {e} (continuing: unenforced adapters are allowed)")
        }
        Err(e) => return Err(e),
    }
    let open = missing(profile, enforced);
    if !open.is_empty() && !allow_unenforced {
        let names: Vec<&str> = open
            .iter()
            .map(|i| oxidepm_sandbox::enforce_name(*i))
            .collect();
        return Err(format!(
            "this system cannot enforce {} (landlock: {}, seccomp: {}); the adapter requires it. Enable it with --allow-unenforced to run it anyway",
            names.join(", "),
            if enforced.landlock { "yes" } else { "no" },
            if enforced.seccomp { "yes" } else { "no" },
        ));
    }
    Ok(enforced)
}

/// Do every step and execute the adapter. Returns only on failure, with the reason.
pub fn launch(
    profile: &Profile,
    allow_unenforced: bool,
    arg0: Option<&str>,
    args: &[String],
) -> String {
    if let Err(e) = restrict(profile, allow_unenforced, true) {
        return e;
    }
    exec_adapter(arg0, args)
}

fn errno() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Descriptor 3 must be the adapter's executable, a regular file; anything else (nothing at all,
/// a pipe, a socket) means the caller did not do its part, and what would be executed is unknown.
fn check_executable_descriptor() -> Result<(), String> {
    // SAFETY: fstat fills the struct it is given.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(EXE_FD, &mut st) } != 0 {
        return Err(format!(
            "the executable is not on descriptor {EXE_FD}: {}",
            errno()
        ));
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(format!(
            "descriptor {EXE_FD} is not a regular file, so it is not the executable"
        ));
    }
    Ok(())
}

/// Close every descriptor above the executable's (they are the daemon's business, and a
/// close-on-exec flag somewhere would be a hope).
fn close_inherited() -> Result<(), String> {
    // SAFETY: close_range closes descriptors of this process only.
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            (EXE_FD + 1) as libc::c_uint,
            libc::c_uint::MAX,
            0 as libc::c_uint,
        )
    };
    if r == 0 {
        return Ok(());
    }
    // kernels before 5.9: walk /proc/self/fd
    let mut open: Vec<i32> = Vec::new();
    let dir = std::fs::read_dir("/proc/self/fd")
        .map_err(|e| format!("cannot list the open descriptors: {e}"))?;
    for entry in dir.flatten() {
        if let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        {
            if fd > EXE_FD {
                open.push(fd);
            }
        }
    }
    for fd in open {
        // SAFETY: closing a descriptor number of this process; the directory's own is already closed or fails with EBADF.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

fn set_rlimit(resource: u32, value: u64, name: &str) -> Result<(), String> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: setrlimit reads the struct it is given.
    if unsafe { libc::setrlimit(resource as _, &limit) } != 0 {
        return Err(format!("cannot set {name}: {}", errno()));
    }
    Ok(())
}

/// no_new_privs, the limits, nice and the OOM score: always applied, on every kernel.
fn apply_baseline(profile: &Profile) -> Result<(), String> {
    close_inherited()?;
    // SAFETY: a flag of this process; it can never be cleared again.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("cannot set no_new_privs: {}", errno()));
    }
    set_rlimit(libc::RLIMIT_NOFILE as _, profile.nofile, "RLIMIT_NOFILE")?;
    set_rlimit(libc::RLIMIT_FSIZE as _, profile.fsize_bytes, "RLIMIT_FSIZE")?;
    set_rlimit(libc::RLIMIT_CORE as _, 0, "RLIMIT_CORE")?;
    if let Some(bytes) = profile.data_limit_bytes() {
        set_rlimit(libc::RLIMIT_DATA as _, bytes, "RLIMIT_DATA")?;
    }
    // SAFETY: setpriority on this process.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, profile.nice) } != 0 {
        return Err(format!("cannot set the niceness: {}", errno()));
    }
    std::fs::write(
        "/proc/self/oom_score_adj",
        profile.oom_score_adj.to_string(),
    )
    .map_err(|e| format!("cannot set oom_score_adj: {e}"))?;
    Ok(())
}

/// Replace this process with the adapter: descriptor 3, close-on-exec so that the adapter does not
/// inherit its own file, and `execveat` on it. Returns only when the exec failed.
fn exec_adapter(arg0: Option<&str>, args: &[String]) -> String {
    // SAFETY: setting a flag on a descriptor of this process.
    if unsafe { libc::fcntl(EXE_FD, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return format!("the executable is not on descriptor {EXE_FD}: {}", errno());
    }
    let mut argv: Vec<CString> = Vec::new();
    for a in std::iter::once(arg0.unwrap_or("adapter")).chain(args.iter().map(String::as_str)) {
        match CString::new(a) {
            Ok(c) => argv.push(c),
            Err(_) => return "an argument holds a NUL byte".to_string(),
        }
    }
    let mut envp: Vec<CString> = Vec::new();
    for (k, v) in std::env::vars_os() {
        let mut entry = k.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(v.as_bytes());
        if let Ok(c) = CString::new(entry) {
            envp.push(c);
        }
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|c| c.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());
    let empty = [0 as libc::c_char];
    // SAFETY: both arrays are NUL-terminated lists of valid C strings that outlive the call.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            EXE_FD,
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    format!("cannot execute the adapter: {}", errno())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::permissions::{
        FsPerm, HostLevel, HostPerm, HostScope, NetworkPerm, SpawnPerm,
    };
    use oxidepm_adapter_proto::Permissions;
    use oxidepm_sandbox::ProfileInput;
    use std::path::Path;

    fn profile(require: Vec<Enforce>) -> Profile {
        let perms = Permissions {
            host: HostPerm {
                level: HostLevel::Read,
                scope: HostScope::All,
                max_in_flight: None,
            },
            events: vec![],
            env_pass: vec![],
            connect_env_socket: vec![],
            network: NetworkPerm::default(),
            fs: FsPerm::default(),
            spawn: SpawnPerm::default(),
            memory_mb: 64,
            requires_enforcement: require,
        };
        Profile::from_permissions(
            &perms,
            &ProfileInput {
                state_dir: Path::new("/h/s"),
                daemon_home: Path::new("/h"),
                cwd: None,
                search_path: &[],
            },
        )
    }

    #[test]
    fn required_items_are_open_when_their_mechanism_is_missing() {
        let all = vec![
            Enforce::Network,
            Enforce::FsWrite,
            Enforce::FsRead,
            Enforce::Spawn,
        ];
        let p = profile(all.clone());
        assert_eq!(
            missing(
                &p,
                Enforced {
                    landlock: true,
                    seccomp: true
                }
            ),
            vec![]
        );
        assert_eq!(
            missing(
                &p,
                Enforced {
                    landlock: false,
                    seccomp: true
                }
            ),
            vec![Enforce::FsWrite, Enforce::FsRead, Enforce::Spawn]
        );
        assert_eq!(
            missing(
                &p,
                Enforced {
                    landlock: true,
                    seccomp: false
                }
            ),
            vec![Enforce::Network]
        );
        assert_eq!(missing(&p, Enforced::default()), all);
        // nothing required, nothing missing
        assert_eq!(missing(&profile(vec![]), Enforced::default()), vec![]);
    }

    /// `apply_baseline` sets no_new_privs by itself. Landlock and the seccomp filter set the flag
    /// too, so the launcher end to end cannot tell whether this step ran; this runs the step alone
    /// in a child (the test binary again, with a marker in the environment) that has neither.
    #[test]
    fn the_baseline_sets_no_new_privs_on_its_own() {
        const MARK: &str = "OXIDEPM_BASELINE_TEST_CHILD";
        let flag = || unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        if std::env::var_os(MARK).is_some() {
            let before = flag();
            let result = apply_baseline(&profile(vec![]));
            println!(
                "BASELINE before={before} after={} ok={}",
                flag(),
                result.is_ok()
            );
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "baseline::tests::the_baseline_sets_no_new_privs_on_its_own",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MARK, "1")
            .output()
            .expect("run the child");
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text
            .lines()
            .find_map(|l| l.split_once("BASELINE ").map(|(_, rest)| rest))
            .unwrap_or_else(|| {
                panic!(
                    "the child printed no result: {text} {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        assert!(line.contains("ok=true"), "{line}");
        if line.contains("before=1") {
            eprintln!(
                "skipped: this test process already had no_new_privs, so the step cannot be seen"
            );
            return;
        }
        assert!(
            line.contains("before=0") && line.contains("after=1"),
            "{line}"
        );
    }
}
