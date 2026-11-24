//! The seccomp filter of an adapter: a deny list that is always on, the socket
//! domains of `network = none`, and the process-creating calls of `spawn = none`. Everything not
//! named is allowed; a denied call fails with `EPERM`.

use std::collections::BTreeMap;

use oxidepm_adapter_proto::permissions::{NetworkMode, SpawnMode};
use oxidepm_sandbox::Profile;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// Calls an adapter has no business making, on any profile (`ptrace` and kin, `io_uring`, the
/// mount family, kexec, bpf, perf, userfaultfd, keyrings, namespaces, chroot, reboot, modules).
fn always_denied() -> Vec<i64> {
    vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_mount_setattr,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_chroot,
        libc::SYS_pivot_root,
        libc::SYS_reboot,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
    ]
}

/// Process creation that is not a thread. `fork` and `vfork` do not exist on aarch64.
fn fork_calls() -> Vec<i64> {
    #[cfg(target_arch = "x86_64")]
    {
        vec![libc::SYS_fork, libc::SYS_vfork]
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Vec::new()
    }
}

fn arch() -> Option<TargetArch> {
    match std::env::consts::ARCH {
        "x86_64" => Some(TargetArch::x86_64),
        "aarch64" => Some(TargetArch::aarch64),
        _ => None,
    }
}

fn cond(
    index: u8,
    len: SeccompCmpArgLen,
    op: SeccompCmpOp,
    value: u64,
) -> Result<SeccompCondition, String> {
    SeccompCondition::new(index, len, op, value).map_err(|e| format!("seccomp: {e}"))
}

/// The filter of a profile, as rules by system call number. An empty rule list means "always".
fn rules(profile: &Profile) -> Result<BTreeMap<i64, Vec<SeccompRule>>, String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in always_denied() {
        rules.insert(nr, Vec::new());
    }
    if profile.network == NetworkMode::None {
        // no IP networking, no raw packets, no netlink; AF_UNIX stays
        let mut sockets = Vec::new();
        for domain in [
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_PACKET,
            libc::AF_NETLINK,
        ] {
            sockets.push(
                SeccompRule::new(vec![cond(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    domain as u64,
                )?])
                .map_err(|e| format!("seccomp: {e}"))?,
            );
        }
        rules.insert(libc::SYS_socket, sockets);
    }
    if profile.spawn == SpawnMode::None {
        for nr in fork_calls() {
            rules.insert(nr, Vec::new());
        }
        // clone without CLONE_THREAD makes a process (clone3 cannot be told apart: its flags are in memory)
        let process = SeccompRule::new(vec![cond(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
            0,
        )?])
        .map_err(|e| format!("seccomp: {e}"))?;
        rules.insert(libc::SYS_clone, vec![process]);
    }
    Ok(rules)
}

/// Install the filter. `Ok(true)` when it is in place, `Ok(false)` on an architecture this build
/// has no filter for.
pub fn apply(profile: &Profile) -> Result<bool, String> {
    let Some(arch) = arch() else { return Ok(false) };
    let filter = SeccompFilter::new(
        rules(profile)?,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("seccomp: cannot build the filter: {e}"))?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| format!("seccomp: cannot compile the filter: {e}"))?;
    seccompiler::apply_filter(&program)
        .map_err(|e| format!("seccomp: cannot install the filter: {e}"))?;
    Ok(true)
}
