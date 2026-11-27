//! The seccomp filter of an adapter: a deny list that is always on, the socket
//! domains of `network = none`, and the process-creating calls of `spawn = none`.

use std::collections::BTreeMap;

use oxidepm_adapter_proto::permissions::{NetworkMode, SpawnMode};
use oxidepm_sandbox::Profile;
use seccompiler::{
    sock_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

const X32_SYSCALL_BIT: u32 = 0x4000_0000;

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
        // Only clone calls that create a thread are allowed.
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

fn deny_syscall_if(comparison: u32, value: u32, errno: i32) -> [sock_filter; 2] {
    [
        sock_filter {
            code: (libc::BPF_JMP | comparison | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: value,
        },
        sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | errno as u32,
        },
    ]
}

fn compile(profile: &Profile, arch: TargetArch) -> Result<BpfProgram, String> {
    let filter = SeccompFilter::new(
        rules(profile)?,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("seccomp: cannot build the filter: {e}"))?;
    let mut program: BpfProgram = filter
        .try_into()
        .map_err(|e| format!("seccomp: cannot compile the filter: {e}"))?;

    let mut checks = Vec::new();
    if arch == TargetArch::x86_64 {
        // x32 shares the audit architecture of x86-64, but is outside the native ABI.
        checks.extend(deny_syscall_if(
            libc::BPF_JSET,
            X32_SYSCALL_BIT,
            libc::EPERM,
        ));
    }
    if profile.spawn == SpawnMode::None {
        // Seccomp cannot inspect clone3's flags. ENOSYS lets glibc use clone for threads.
        checks.extend(deny_syscall_if(
            libc::BPF_JEQ,
            libc::SYS_clone3 as u32,
            libc::ENOSYS,
        ));
    }
    if !checks.is_empty() {
        // Insert after the architecture check, where the compiler loads the syscall number.
        let load_word = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        let nr_offset = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
        let nr_load = program
            .iter()
            .position(|instruction| instruction.code == load_word && instruction.k == nr_offset)
            .ok_or("seccomp: compiled filter does not load the syscall number")?;
        program.splice(nr_load + 1..nr_load + 1, checks);
    }
    Ok(program)
}

/// Install the filter. `Ok(true)` when it is in place, `Ok(false)` on an architecture this build
/// has no filter for.
pub fn apply(profile: &Profile) -> Result<bool, String> {
    let Some(arch) = arch() else { return Ok(false) };
    let program = compile(profile, arch)?;
    seccompiler::apply_filter(&program)
        .map_err(|e| format!("seccomp: cannot install the filter: {e}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_sandbox::ProfileInput;
    use std::path::Path;

    fn profile() -> Profile {
        Profile::from_permissions(
            &serde_json::from_str(r#"{"host":{"level":"read","scope":"all"}}"#).unwrap(),
            &ProfileInput {
                state_dir: Path::new("/state"),
                daemon_home: Path::new("/home"),
                cwd: None,
                search_path: &[],
            },
        )
    }

    // Evaluate the generated filter with synthetic seccomp data.
    fn action(program: &BpfProgram, audit_arch: u32, syscall: i64, arg0: u64) -> u32 {
        use libc::{
            BPF_ABS, BPF_ALU, BPF_AND, BPF_JA, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET,
            BPF_W,
        };

        const LOAD_WORD: u32 = BPF_LD | BPF_W | BPF_ABS;
        const AND: u32 = BPF_ALU | BPF_AND | BPF_K;
        const JUMP: u32 = BPF_JMP | BPF_JA;
        const EQUAL: u32 = BPF_JMP | BPF_JEQ | BPF_K;
        const BIT_SET: u32 = BPF_JMP | BPF_JSET | BPF_K;
        const RETURN: u32 = BPF_RET | BPF_K;

        let mut data = [0u32; 6];
        data[0] = syscall as u32;
        data[1] = audit_arch;
        data[4] = arg0 as u32;
        data[5] = (arg0 >> 32) as u32;
        let mut accumulator = 0;
        let mut pc = 0;
        loop {
            let instruction = &program[pc];
            pc += 1;
            match u32::from(instruction.code) {
                LOAD_WORD => accumulator = data[instruction.k as usize / 4],
                AND => accumulator &= instruction.k,
                JUMP => pc += instruction.k as usize,
                EQUAL | BIT_SET => {
                    let matches = if u32::from(instruction.code) == EQUAL {
                        accumulator == instruction.k
                    } else {
                        accumulator & instruction.k != 0
                    };
                    pc += usize::from(if matches {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                }
                RETURN => return instruction.k,
                _ => panic!("unsupported BPF instruction: {instruction:?}"),
            }
        }
    }

    fn native_arch() -> (TargetArch, u32) {
        let arch = arch().unwrap();
        // The native ELF machine ID plus the 64-bit and little-endian audit flags.
        let machine = match arch {
            TargetArch::x86_64 => libc::EM_X86_64,
            TargetArch::aarch64 => libc::EM_AARCH64,
            _ => unreachable!(),
        };
        (arch, 0xc000_0000 | u32::from(machine))
    }

    #[test]
    fn spawn_none_keeps_the_glibc_thread_fallback() {
        let (arch, audit_arch) = native_arch();
        let mut profile = profile();
        profile.spawn = SpawnMode::None;
        let program = compile(&profile, arch).unwrap();
        assert_eq!(
            action(&program, audit_arch, libc::SYS_clone3, 0),
            libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32
        );
        assert_eq!(
            action(
                &program,
                audit_arch,
                libc::SYS_clone,
                libc::CLONE_THREAD as u64
            ),
            libc::SECCOMP_RET_ALLOW
        );
        assert_eq!(
            action(&program, audit_arch, libc::SYS_clone, 0),
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
        );
        for spawn in [SpawnMode::Named, SpawnMode::Any] {
            profile.spawn = spawn;
            let program = compile(&profile, arch).unwrap();
            assert_eq!(
                action(&program, audit_arch, libc::SYS_clone3, 0),
                libc::SECCOMP_RET_ALLOW
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_filter_accepts_only_the_native_syscall_abi() {
        let (arch, audit_arch) = native_arch();
        let mut profile = profile();
        profile.spawn = SpawnMode::Any;
        let program = compile(&profile, arch).unwrap();
        assert_eq!(
            action(&program, audit_arch, libc::SYS_getpid, 0),
            libc::SECCOMP_RET_ALLOW
        );
        assert_eq!(
            action(
                &program,
                audit_arch,
                libc::SYS_getpid | i64::from(X32_SYSCALL_BIT),
                0
            ),
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
        );
        assert_eq!(
            action(&program, 0, libc::SYS_getpid, 0),
            libc::SECCOMP_RET_KILL_PROCESS
        );
    }
}
