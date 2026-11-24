//! The sandbox profile: what the launcher applies to one adapter process.
//!
//! The daemon builds a [`Profile`] from the permissions the user granted, serialises it as JSON
//! and hands it to the launcher as one argument. The profile holds paths and numbers only; the
//! launcher decides how the kernel can express them.

use std::path::{Path, PathBuf};

use oxidepm_adapter_proto::permissions::{Enforce, NetworkMode, SpawnMode};
use oxidepm_adapter_proto::Permissions;
use serde::{Deserialize, Serialize};

/// Open files an adapter may hold.
pub const NOFILE: u64 = 256;
/// The largest file an adapter may write.
pub const FSIZE_BYTES: u64 = 64 * 1024 * 1024;
/// The niceness every adapter gets.
pub const NICE: i32 = 10;
/// The OOM score adjustment every adapter gets: the kernel picks an adapter before an app.
pub const OOM_SCORE_ADJ: i32 = 1000;
/// Headroom for writable virtual reservations such as V8's code range in subprocesses.
const UNRESTRICTED_SPAWN_DATA_HEADROOM_MB: u64 = 512;
/// Directories every dynamically linked program reads while it starts.
pub const LOADER_READ: &[&str] = &[
    "/lib",
    "/lib32",
    "/lib64",
    "/usr",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/dev/null",
    "/dev/urandom",
];

/// Everything the launcher needs to know about one adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// The adapter tree's resident memory budget, in MiB. Zero leaves memory unlimited.
    pub memory_mb: u32,
    pub nofile: u64,
    pub fsize_bytes: u64,
    pub nice: i32,
    pub oom_score_adj: i32,
    pub network: NetworkMode,
    /// Ports the adapter may bind (TCP), when the kernel can say so.
    pub listen_ports: Vec<u16>,
    /// `fs.read = any`: everything but the daemon home, other processes' `/proc` entries and
    /// the sockets of the daemon.
    pub read_any: bool,
    /// Paths (files or directories) that may be read, with the loader paths added by the launcher.
    pub read: Vec<PathBuf>,
    /// Paths that may be written (and read).
    pub write: Vec<PathBuf>,
    /// Pathname sockets the adapter may connect to by virtue of a socket grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect_unix: Vec<PathBuf>,
    pub spawn: SpawnMode,
    /// For `spawn.mode = named`: the absolute paths of the programs that may be executed.
    pub programs: Vec<PathBuf>,
    /// The adapter's own state directory.
    pub state_dir: PathBuf,
    /// The daemon's home: out of reach of an adapter whose `fs.read` is `any`.
    pub daemon_home: PathBuf,
    /// Items the OS must enforce (`requires_enforcement`); the launcher refuses to start the
    /// adapter if it cannot, unless it was told `--allow-unenforced`.
    pub require: Vec<Enforce>,
}

/// What the daemon knows when it builds a profile.
#[derive(Debug, Clone)]
pub struct ProfileInput<'a> {
    pub state_dir: &'a Path,
    pub daemon_home: &'a Path,
    /// The working directory of a verb invocation, for `$CWD`.
    pub cwd: Option<&'a Path>,
    /// The directories searched for the names of `spawn.mode = named`.
    pub search_path: &'a [PathBuf],
}

fn expand(entry: &str, input: &ProfileInput<'_>) -> Option<PathBuf> {
    let path = match entry {
        "$STATE" => input.state_dir.to_path_buf(),
        "$CWD" => input.cwd?.to_path_buf(),
        other => PathBuf::from(other),
    };
    // a relative path means nothing to a process whose directory the launcher does not know
    path.is_absolute().then_some(path)
}

/// The port of a `host:port` listen entry.
fn port_of(entry: &str) -> Option<u16> {
    entry
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .filter(|p| *p != 0)
}

/// The absolute path of a program name found in `dirs`.
pub fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    dirs.iter().map(|d| d.join(name)).find_map(|candidate| {
        let real = std::fs::canonicalize(&candidate).ok()?;
        let meta = std::fs::metadata(&real).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(real)
    })
}

impl Profile {
    /// The profile of an adapter that was granted `permissions`.
    pub fn from_permissions(permissions: &Permissions, input: &ProfileInput<'_>) -> Profile {
        let read_any = permissions.fs.read.iter().any(|p| p == "any");
        let mut read: Vec<PathBuf> = permissions
            .fs
            .read
            .iter()
            .filter(|p| *p != "any")
            .filter_map(|p| expand(p, input))
            .collect();
        let mut write: Vec<PathBuf> = permissions
            .fs
            .write
            .iter()
            .filter_map(|p| expand(p, input))
            .collect();
        read.sort();
        read.dedup();
        write.sort();
        write.dedup();
        let mut programs: Vec<PathBuf> = match permissions.spawn.mode {
            SpawnMode::Named => permissions
                .spawn
                .names
                .iter()
                .filter_map(|n| find_program(n, input.search_path))
                .collect(),
            _ => Vec::new(),
        };
        programs.sort();
        programs.dedup();
        let mut listen_ports: Vec<u16> = permissions
            .network
            .listen
            .iter()
            .filter_map(|l| port_of(l))
            .collect();
        listen_ports.sort_unstable();
        listen_ports.dedup();
        Profile {
            memory_mb: permissions.memory_mb,
            nofile: NOFILE,
            fsize_bytes: FSIZE_BYTES,
            nice: NICE,
            oom_score_adj: OOM_SCORE_ADJ,
            network: permissions.network.mode,
            listen_ports,
            read_any,
            read,
            write,
            connect_unix: Vec::new(),
            spawn: permissions.spawn.mode,
            programs,
            state_dir: input.state_dir.to_path_buf(),
            daemon_home: input.daemon_home.to_path_buf(),
            require: permissions.requires_enforcement.clone(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(text: &str) -> Result<Profile, String> {
        serde_json::from_str(text).map_err(|e| format!("the profile is not valid: {e}"))
    }

    /// Per-process `RLIMIT_DATA` in bytes, `None` when the profile sets none.
    /// The daemon still enforces `memory_mb` on the whole tree's resident memory.
    pub fn data_limit_bytes(&self) -> Option<u64> {
        let headroom = if self.spawn == SpawnMode::Any {
            UNRESTRICTED_SPAWN_DATA_HEADROOM_MB
        } else {
            0
        };
        (self.memory_mb > 0).then(|| (u64::from(self.memory_mb) + headroom) * 1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::permissions::{
        FsPerm, HostLevel, HostPerm, HostScope, NetworkPerm, SpawnPerm,
    };

    fn perms() -> Permissions {
        Permissions {
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
            memory_mb: 128,
            requires_enforcement: vec![],
        }
    }

    fn input<'a>(state: &'a Path, home: &'a Path, search: &'a [PathBuf]) -> ProfileInput<'a> {
        ProfileInput {
            state_dir: state,
            daemon_home: home,
            cwd: None,
            search_path: search,
        }
    }

    #[test]
    fn state_and_cwd_are_expanded_and_relative_paths_dropped() {
        let mut p = perms();
        p.fs = FsPerm {
            read: vec![
                "$STATE".into(),
                "relative/dir".into(),
                "/srv/data".into(),
                "$CWD".into(),
            ],
            write: vec!["$STATE".into(), "$CWD".into()],
        };
        let (state, home) = (PathBuf::from("/h/adapters/state/x"), PathBuf::from("/h"));
        let cwd = PathBuf::from("/work/app");
        let mut i = input(&state, &home, &[]);
        let without = Profile::from_permissions(&p, &i);
        assert_eq!(
            without.read,
            vec![
                PathBuf::from("/h/adapters/state/x"),
                PathBuf::from("/srv/data")
            ]
        );
        assert_eq!(without.write, vec![PathBuf::from("/h/adapters/state/x")]);
        i.cwd = Some(&cwd);
        let with = Profile::from_permissions(&p, &i);
        assert!(with.write.contains(&cwd) && with.read.contains(&cwd));
        assert!(!with.read_any);
    }

    #[test]
    fn any_is_a_flag_not_a_path() {
        let mut p = perms();
        p.fs.read = vec!["any".into(), "/srv".into()];
        let (state, home) = (PathBuf::from("/h/s"), PathBuf::from("/h"));
        let profile = Profile::from_permissions(&p, &input(&state, &home, &[]));
        assert!(profile.read_any);
        assert_eq!(profile.read, vec![PathBuf::from("/srv")]);
    }

    #[test]
    fn named_programs_resolve_to_absolute_files_and_missing_ones_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("mytool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut p = perms();
        p.spawn = SpawnPerm {
            mode: SpawnMode::Named,
            names: vec!["mytool".into(), "not-there".into()],
        };
        let (state, home) = (PathBuf::from("/h/s"), PathBuf::from("/h"));
        let search = vec![dir.path().to_path_buf()];
        let profile = Profile::from_permissions(&p, &input(&state, &home, &search));
        assert_eq!(
            profile.programs,
            vec![std::fs::canonicalize(&tool).unwrap()]
        );
    }

    #[test]
    fn listen_ports_come_from_host_port_entries() {
        let mut p = perms();
        p.network = NetworkPerm {
            mode: NetworkMode::Outbound,
            hosts: vec![],
            listen: vec![
                "127.0.0.1:9615".into(),
                "bad".into(),
                "[::1]:80".into(),
                "x:0".into(),
            ],
        };
        let (state, home) = (PathBuf::from("/h/s"), PathBuf::from("/h"));
        let profile = Profile::from_permissions(&p, &input(&state, &home, &[]));
        assert_eq!(profile.listen_ports, vec![80, 9615]);
    }

    #[test]
    fn the_profile_survives_json_and_carries_the_fixed_limits() {
        let mut p = perms();
        p.requires_enforcement = vec![Enforce::Network];
        let (state, home) = (PathBuf::from("/h/s"), PathBuf::from("/h"));
        let profile = Profile::from_permissions(&p, &input(&state, &home, &[]));
        assert_eq!(Profile::from_json(&profile.to_json()).unwrap(), profile);
        assert_eq!(
            (
                profile.nofile,
                profile.fsize_bytes,
                profile.nice,
                profile.oom_score_adj
            ),
            (256, 64 << 20, 10, 1000)
        );
        assert_eq!(profile.data_limit_bytes(), Some(128 << 20));
        assert!(Profile::from_json("{").is_err());
    }
}
