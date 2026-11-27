//! What this machine's kernel offers.
//!
//! The probe only asks: it changes nothing about the calling process (the daemon). That a feature
//! exists is not the same as the feature working for an adapter; the self-test (`report.rs`) is
//! what turns "exists" into "enforced".

use serde::{Deserialize, Serialize};

/// The first Landlock ABI that can restrict connecting to pathname UNIX sockets.
pub const LANDLOCK_ABI_UNIX_CONNECT: u32 = 9;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxCaps {
    /// The highest Landlock ABI the kernel speaks, `None` without Landlock (or with it off).
    pub landlock_abi: Option<u32>,
    /// Landlock can restrict connecting to pathname sockets (ABI 9 and later).
    pub landlock_unix_connect: bool,
    /// seccomp filters exist.
    pub seccomp: bool,
    /// `no_new_privs` exists.
    pub no_new_privs: bool,
    /// The Yama `ptrace_scope` (0: any same-user process may attach to another).
    pub ptrace_scope: Option<u8>,
    /// A cgroup v2 directory that this user can create children in, with `pids` and `memory`
    /// already delegated to children.
    pub cgroup_delegated: bool,
}

impl SandboxCaps {
    /// One line for `status --daemon`.
    pub fn summary(&self) -> String {
        let landlock = match self.landlock_abi {
            Some(abi) => format!(
                "landlock abi {abi}{}",
                if self.landlock_unix_connect {
                    " (restricts unix connect)"
                } else {
                    ""
                }
            ),
            None => "no landlock".to_string(),
        };
        let seccomp = if self.seccomp {
            "seccomp"
        } else {
            "no seccomp"
        };
        let ptrace = match self.ptrace_scope {
            Some(0) => ", ptrace_scope 0 (any same-user process can attach to another)".to_string(),
            Some(n) => format!(", ptrace_scope {n}"),
            None => String::new(),
        };
        let cgroup = if self.cgroup_delegated {
            ", cgroup delegated"
        } else {
            ""
        };
        format!("{landlock}, {seccomp}{ptrace}{cgroup}")
    }
}

/// The Landlock ABI version, if the kernel has Landlock enabled.
pub fn landlock_abi() -> Option<u32> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_ulong = 1;
    // SAFETY: a version query takes no pointer and changes nothing.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(v).ok().filter(|v| *v >= 1)
}

fn seccomp_available() -> bool {
    // SAFETY: PR_GET_SECCOMP reads the calling thread's mode; EINVAL means the kernel has no seccomp.
    unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) >= 0 }
}

fn no_new_privs_available() -> bool {
    // SAFETY: a read of a flag of the calling thread.
    unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) >= 0 }
}

fn ptrace_scope() -> Option<u8> {
    std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The cgroup v2 directory of this process, when cgroup v2 is mounted.
fn own_cgroup_dir() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    let dir = std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    dir.is_dir().then_some(dir)
}

/// Is there a cgroup this user may create children in, with the controllers adapters need already
/// enabled for them? Nothing is created and nothing is moved: this daemon's own placement is
/// never changed, so a machine where the answer needs a change reports `false`.
fn cgroup_delegated() -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(dir) = own_cgroup_dir() else {
        return false;
    };
    let writable = |name: &str| {
        let path = dir.join(name);
        std::ffi::CString::new(path.as_os_str().as_bytes()).is_ok_and(|c| {
            // SAFETY: access(2) on a NUL-terminated path.
            unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
        })
    };
    let enabled = std::fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap_or_default();
    let have = |c: &str| enabled.split_whitespace().any(|x| x == c);
    writable("cgroup.procs") && writable("cgroup.subtree_control") && have("pids") && have("memory")
}

/// Ask the kernel what it offers. Cheap and free of side effects; the daemon calls it at start.
pub fn probe() -> SandboxCaps {
    let abi = landlock_abi();
    SandboxCaps {
        landlock_abi: abi,
        landlock_unix_connect: abi.is_some_and(|a| a >= LANDLOCK_ABI_UNIX_CONNECT),
        seccomp: seccomp_available(),
        no_new_privs: no_new_privs_available(),
        ptrace_scope: ptrace_scope(),
        cgroup_delegated: cgroup_delegated(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_consistent_and_does_not_change_the_process() {
        let before = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        let a = probe();
        let b = probe();
        let after = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        assert_eq!(a, b);
        assert_eq!(
            before, after,
            "the probe must not set no_new_privs on the daemon"
        );
        assert_eq!(
            a.landlock_unix_connect,
            a.landlock_abi.is_some_and(|v| v >= 9)
        );
        assert!(!a.summary().is_empty());
    }

    #[test]
    fn summary_names_the_weak_spots() {
        let caps = SandboxCaps {
            landlock_abi: None,
            landlock_unix_connect: false,
            seccomp: false,
            no_new_privs: true,
            ptrace_scope: Some(0),
            cgroup_delegated: false,
        };
        let text = caps.summary();
        assert!(
            text.contains("no landlock")
                && text.contains("no seccomp")
                && text.contains("ptrace_scope 0"),
            "{text}"
        );
    }
}
