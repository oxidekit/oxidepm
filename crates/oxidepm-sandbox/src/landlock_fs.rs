//! The Landlock ruleset of an adapter: filesystem paths, program execution,
//! TCP bind ports and the two scopes. Built here, in the launcher, so `/proc/self` and every path
//! resolve in the adapter's own process.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, AccessNet, BitFlags, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};
use oxidepm_adapter_proto::permissions::SpawnMode;
use oxidepm_sandbox::profile::LOADER_READ;
use oxidepm_sandbox::{Profile, EXE_FD};

use crate::paths::{elf_interpreter, read_any_paths};

/// The newest ABI this build knows. Rights a kernel lacks are dropped, and the caller learns that
/// from the status. The launcher checks required enforcement after applying the rules.
const ABI_NEWEST: ABI = ABI::V9;

fn read_rights() -> BitFlags<AccessFs> {
    AccessFs::ReadFile | AccessFs::ReadDir
}

fn write_rights() -> BitFlags<AccessFs> {
    AccessFs::from_write(ABI_NEWEST) | read_rights()
}

fn exec_rights() -> BitFlags<AccessFs> {
    AccessFs::Execute | AccessFs::ReadFile
}

/// The path rules of a profile, each with what it allows, merged per path.
fn path_rules(
    profile: &Profile,
    exe: Option<&File>,
    handles_execute: bool,
) -> BTreeMap<PathBuf, BitFlags<AccessFs>> {
    // a map: `fs.read = any` lists every entry of the directories around the daemon home, and a
    // directory like /tmp can hold tens of thousands
    let mut rules: BTreeMap<PathBuf, BitFlags<AccessFs>> = BTreeMap::new();
    let mut add = |path: PathBuf, access: BitFlags<AccessFs>| {
        *rules.entry(path).or_insert_with(BitFlags::empty) |= access;
    };

    if profile.read_any {
        let home = std::fs::canonicalize(&profile.daemon_home)
            .unwrap_or_else(|_| profile.daemon_home.clone());
        for path in read_any_paths(Path::new("/"), Path::new("/proc"), &home) {
            add(path, read_rights());
        }
        // the one corner of the daemon home an adapter owns
        add(profile.state_dir.clone(), read_rights());
    } else {
        for path in LOADER_READ {
            add(PathBuf::from(path), read_rights());
        }
        add(PathBuf::from("/proc/self"), read_rights());
        for path in &profile.read {
            add(path.clone(), read_rights());
        }
    }
    for path in &profile.write {
        add(path.clone(), write_rights());
    }
    // a program opens /dev/null for writing as often as it reads it
    add(
        PathBuf::from("/dev/null"),
        AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate | AccessFs::IoctlDev,
    );

    // The adapter's own file and the loader the kernel opens for it. The kernel opens both for
    // reading as well as for execution, so they are needed even when `EXECUTE` is not handled.
    if let Some(exe) = exe {
        add(
            PathBuf::from(format!("/proc/self/fd/{EXE_FD}")),
            exec_rights(),
        );
        if let Some(interpreter) = elf_interpreter(exe) {
            let real = std::fs::canonicalize(&interpreter).unwrap_or(interpreter);
            add(real, exec_rights());
        }
    }
    if handles_execute && profile.spawn == SpawnMode::Named {
        for program in &profile.programs {
            add(program.clone(), exec_rights());
        }
    }
    rules
}

/// Restrict this process. `Ok(true)` when the kernel enforces the ruleset (fully or in part),
/// `Ok(false)` when it has no Landlock. `with_exe`: descriptor 3 holds the adapter's executable
/// (the self-test has none).
pub fn apply(profile: &Profile, with_exe: bool) -> Result<bool, String> {
    if !with_exe {
        return apply_with(profile, None);
    }
    // SAFETY: descriptor 3 is the executable the daemon handed over; the File only reads it and
    // is leaked below so that the descriptor stays open for the exec.
    let exe = unsafe { File::from_raw_fd(EXE_FD) };
    let result = apply_with(profile, Some(&exe));
    std::mem::forget(exe);
    result
}

fn apply_with(profile: &Profile, exe: Option<&File>) -> Result<bool, String> {
    let mut handled = AccessFs::from_all(ABI_NEWEST);
    let handles_execute = profile.spawn != SpawnMode::Any;
    if !handles_execute {
        handled.remove(AccessFs::Execute);
    }
    let rules = path_rules(profile, exe, handles_execute);

    let err = |what: &str, e: &dyn std::fmt::Display| format!("landlock: {what}: {e}");
    let mut ruleset = Ruleset::default()
        .handle_access(handled)
        .map_err(|e| err("cannot handle the filesystem rights", &e))?
        .handle_access(AccessNet::BindTcp)
        .map_err(|e| err("cannot handle TCP bind", &e))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)
        .map_err(|e| err("cannot scope sockets and signals", &e))?
        .create()
        .map_err(|e| err("cannot create the ruleset", &e))?;

    for (path, access) in rules {
        // a path that is not there (a loader file this system does not have) opens nothing
        let Ok(fd) = PathFd::new(&path) else { continue };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access & handled))
            .map_err(|e| err(&format!("cannot allow {}", path.display()), &e))?;
    }
    for path in &profile.connect_unix {
        // Validate the same descriptor used by the rule in case the path changed since launch.
        let fd = File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| err(&format!("cannot open socket {}", path.display()), &e))?;
        let opened =
            std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(|e| {
                err(
                    &format!("cannot resolve opened socket {}", path.display()),
                    &e,
                )
            })?;
        if opened != *path {
            return Err(format!(
                "landlock: socket path changed while opening {}",
                path.display()
            ));
        }
        let home = profile
            .daemon_home
            .canonicalize()
            .map_err(|e| err("cannot resolve the daemon home", &e))?;
        if opened.starts_with(home) {
            return Err(format!(
                "landlock: {} is inside the daemon home",
                opened.display()
            ));
        }
        let meta = fd
            .metadata()
            .map_err(|e| err(&format!("cannot inspect socket {}", path.display()), &e))?;
        // SAFETY: geteuid reads the process's effective user ID.
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "landlock: {} is not a UNIX socket owned by the launcher user",
                path.display()
            ));
        }
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::ResolveUnix))
            .map_err(|e| err(&format!("cannot allow socket {}", path.display()), &e))?;
    }
    for port in &profile.listen_ports {
        ruleset = ruleset
            .add_rule(NetPort::new(*port, AccessNet::BindTcp))
            .map_err(|e| err(&format!("cannot allow port {port}"), &e))?;
    }
    let status = ruleset
        .restrict_self()
        .map_err(|e| err("cannot restrict the process", &e))?;
    Ok(status.ruleset != RulesetStatus::NotEnforced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, net::UnixListener};

    #[test]
    fn socket_rule_refuses_a_redirected_parent_directory() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let private = temp.path().join("private");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&private).unwrap();
        let requested = original.join("agent.sock");
        let _original_socket = UnixListener::bind(&requested).unwrap();
        let _other_socket = UnixListener::bind(private.join("agent.sock")).unwrap();
        let mut profile = Profile::from_permissions(
            &serde_json::from_str(r#"{"host":{"level":"read","scope":"all"}}"#).unwrap(),
            &oxidepm_sandbox::ProfileInput {
                state_dir: &original,
                daemon_home: &private,
                cwd: None,
                search_path: &[],
            },
        );
        profile.connect_unix = vec![requested.canonicalize().unwrap()];
        std::fs::rename(&original, temp.path().join("moved")).unwrap();
        symlink(&private, &original).unwrap();

        let result = std::thread::spawn(move || apply(&profile, false))
            .join()
            .unwrap();
        assert!(
            result.is_err_and(|err| err.contains("path changed")),
            "{original:?}"
        );
    }
}
