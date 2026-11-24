//! The self-test: the launcher applies a profile to itself, tries what the profile
//! forbids and reports what happened. "Enforced" is then a measured fact on this machine.

use std::collections::BTreeMap;
use std::path::PathBuf;

use oxidepm_adapter_proto::permissions::Enforce;
use serde::{Deserialize, Serialize};

use crate::caps::SandboxCaps;

/// The names of the attempts, as they appear in a report.
pub mod attempt {
    pub const WRITE_OUTSIDE: &str = "write_outside";
    pub const INET_SOCKET: &str = "inet_socket";
    pub const IO_URING: &str = "io_uring_setup";
    pub const EXEC: &str = "exec";
    pub const DAEMON_SOCKET: &str = "daemon_socket";
    pub const SESSION_BUS: &str = "session_bus";
    pub const ABSTRACT_SOCKET: &str = "abstract_socket";
    pub const PATHNAME_SOCKET: &str = "pathname_socket";
    pub const READ_OUTSIDE: &str = "read_outside";
    pub const READ_DAEMON_HOME: &str = "read_daemon_home";
    /// The attempts that connect to an AF_UNIX service.
    pub const UNIX: [&str; 4] = [DAEMON_SOCKET, SESSION_BUS, ABSTRACT_SOCKET, PATHNAME_SOCKET];
}

/// What one attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// The kernel said no (`EACCES` or `EPERM`).
    Refused { errno: i32 },
    /// It worked.
    Allowed,
    /// There was nothing to try it on (no such path).
    Absent,
    /// Another error.
    Failed { errno: i32 },
}

impl Outcome {
    /// Read the outcome off an I/O result.
    pub fn of<T>(result: std::io::Result<T>) -> Outcome {
        match result {
            Ok(_) => Outcome::Allowed,
            Err(e) => match e.raw_os_error() {
                Some(code) if code == libc::EACCES || code == libc::EPERM => {
                    Outcome::Refused { errno: code }
                }
                Some(code) if code == libc::ENOENT || code == libc::ENOTDIR => Outcome::Absent,
                Some(code) => Outcome::Failed { errno: code },
                None => Outcome::Failed { errno: 0 },
            },
        }
    }

    pub fn is_refused(&self) -> bool {
        matches!(self, Outcome::Refused { .. })
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Outcome::Allowed)
    }
}

/// What the self-test tries, as the host chooses it (every path is made by the host so that the
/// helper creates nothing outside what the profile allows).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SelftestPlan {
    /// A file path in a directory the profile does not allow writing to.
    pub write_outside: Option<PathBuf>,
    /// A readable file that the profile's `fs.read` does not cover.
    pub read_outside: Option<PathBuf>,
    /// A file inside the daemon home.
    pub read_daemon_home: Option<PathBuf>,
    /// The daemon's socket path.
    pub daemon_socket: Option<PathBuf>,
    /// The session bus socket.
    pub session_bus: Option<PathBuf>,
    /// An abstract socket (name without the leading NUL) that the host listens on.
    pub abstract_socket: Option<String>,
    /// A pathname socket in a temporary directory that the host listens on.
    pub pathname_socket: Option<PathBuf>,
    /// A program to try to execute.
    pub exec: Option<PathBuf>,
}

/// The result of one self-test run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SelfTestReport {
    /// Why the profile could not be applied, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_error: Option<String>,
    #[serde(default)]
    pub attempts: BTreeMap<String, Outcome>,
}

impl SelfTestReport {
    pub fn outcome(&self, name: &str) -> Option<Outcome> {
        self.attempts.get(name).copied()
    }

    /// The OS items this machine enforced for this profile, judged by what was refused. With
    /// `read_any` the profile lets the adapter read everything but the daemon home, so the read
    /// rule counts as enforced when the daemon home was refused; otherwise when a file outside
    /// the profile's read paths was.
    pub fn enforced(&self, read_any: bool) -> Vec<Enforce> {
        let refused = |name: &str| self.outcome(name).is_some_and(|o| o.is_refused());
        let mut out = Vec::new();
        if refused(attempt::INET_SOCKET)
            && !self
                .outcome(attempt::IO_URING)
                .is_some_and(|o| o.is_allowed())
        {
            out.push(Enforce::Network);
        }
        if refused(attempt::WRITE_OUTSIDE) {
            out.push(Enforce::FsWrite);
        }
        if refused(if read_any {
            attempt::READ_DAEMON_HOME
        } else {
            attempt::READ_OUTSIDE
        }) {
            out.push(Enforce::FsRead);
        }
        if refused(attempt::EXEC) {
            out.push(Enforce::Spawn);
        }
        out
    }

    /// The AF_UNIX services the sandboxed process could still connect to.
    pub fn unix_reachable(&self) -> Vec<String> {
        attempt::UNIX
            .iter()
            .filter(|n| self.outcome(n).is_some_and(|o| o.is_allowed()))
            .map(|n| n.to_string())
            .collect()
    }
}

/// The items of `required` that `enforced` does not contain.
pub fn missing_enforcement(required: &[Enforce], enforced: &[Enforce]) -> Vec<Enforce> {
    required
        .iter()
        .copied()
        .filter(|r| !enforced.contains(r))
        .collect()
}

/// The name of an item as a manifest spells it.
pub fn enforce_name(item: Enforce) -> &'static str {
    match item {
        Enforce::Network => "network",
        Enforce::FsWrite => "fs_write",
        Enforce::FsRead => "fs_read",
        Enforce::Spawn => "spawn",
    }
}

/// What `enable` stores in the adapter's `enforced_json`: how far the sandbox held on this machine
/// when the user enabled the adapter, and whether they accepted less than the adapter required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcedRecord {
    /// The user passed `--allow-unenforced`.
    #[serde(default)]
    pub allow_unenforced: bool,
    pub enforced: Vec<Enforce>,
    /// AF_UNIX services the adapter could reach in the self-test.
    #[serde(default)]
    pub unix_reachable: Vec<String>,
    pub caps: SandboxCaps,
}

impl EnforcedRecord {
    pub fn from_report(
        report: &SelfTestReport,
        caps: SandboxCaps,
        allow_unenforced: bool,
        read_any: bool,
    ) -> Self {
        Self {
            allow_unenforced,
            enforced: report.enforced(read_any),
            unix_reachable: report.unix_reachable(),
            caps,
        }
    }

    /// The sentence `adapter info` shows when the self-test could reach an AF_UNIX service, which
    /// is the one thing `network = none` and `spawn = none` do not close.
    pub fn unix_note(&self) -> Option<String> {
        (!self.unix_reachable.is_empty()).then(|| {
            format!(
                "AF_UNIX services reachable: {}",
                self.unix_reachable.join(", ")
            )
        })
    }

    /// Read a stored record; anything that does not parse means "nothing was accepted".
    pub fn parse(text: Option<&str>) -> Option<Self> {
        serde_json::from_str(text?).ok()
    }

    /// `enable`'s check: `Err` names the required items this machine cannot enforce, unless the
    /// user accepted that.
    pub fn check(&self, required: &[Enforce]) -> Result<(), Vec<Enforce>> {
        let missing = missing_enforcement(required, &self.enforced);
        if missing.is_empty() || self.allow_unenforced {
            Ok(())
        } else {
            Err(missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(entries: &[(&str, Outcome)]) -> SelfTestReport {
        SelfTestReport {
            setup_error: None,
            attempts: entries.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    const NO: Outcome = Outcome::Refused { errno: 1 };

    #[test]
    fn outcomes_are_read_off_errno() {
        assert_eq!(
            Outcome::of::<()>(Err(std::io::Error::from_raw_os_error(libc::EACCES))),
            Outcome::Refused {
                errno: libc::EACCES
            }
        );
        assert_eq!(
            Outcome::of::<()>(Err(std::io::Error::from_raw_os_error(libc::EPERM))),
            Outcome::Refused { errno: libc::EPERM }
        );
        assert_eq!(
            Outcome::of::<()>(Err(std::io::Error::from_raw_os_error(libc::ENOENT))),
            Outcome::Absent
        );
        assert_eq!(
            Outcome::of::<()>(Err(std::io::Error::from_raw_os_error(libc::ECONNREFUSED))),
            Outcome::Failed {
                errno: libc::ECONNREFUSED
            }
        );
        assert_eq!(Outcome::of(Ok(1)), Outcome::Allowed);
    }

    #[test]
    fn enforced_items_follow_what_was_refused() {
        let r = report(&[
            (attempt::INET_SOCKET, NO),
            (attempt::IO_URING, NO),
            (attempt::WRITE_OUTSIDE, NO),
            (attempt::READ_OUTSIDE, Outcome::Allowed),
            (attempt::EXEC, NO),
        ]);
        assert_eq!(
            r.enforced(false),
            vec![Enforce::Network, Enforce::FsWrite, Enforce::Spawn]
        );
        // io_uring open means the network rule can be bypassed
        let r = report(&[
            (attempt::INET_SOCKET, NO),
            (attempt::IO_URING, Outcome::Allowed),
        ]);
        assert!(r.enforced(false).is_empty());
        assert!(report(&[]).enforced(false).is_empty());
        // with fs.read = any the daemon home is what shows the read rule
        let any = report(&[
            (attempt::READ_OUTSIDE, Outcome::Allowed),
            (attempt::READ_DAEMON_HOME, NO),
        ]);
        assert_eq!(any.enforced(true), vec![Enforce::FsRead]);
        assert!(any.enforced(false).is_empty());
    }

    #[test]
    fn unix_reachability_lists_the_connections_that_worked() {
        let r = report(&[
            (attempt::PATHNAME_SOCKET, Outcome::Allowed),
            (attempt::SESSION_BUS, NO),
            (attempt::DAEMON_SOCKET, Outcome::Absent),
        ]);
        assert_eq!(r.unix_reachable(), vec!["pathname_socket".to_string()]);
        let note =
            EnforcedRecord::from_report(&r, SandboxCaps::default(), false, false).unix_note();
        assert_eq!(
            note.as_deref(),
            Some("AF_UNIX services reachable: pathname_socket")
        );
        assert_eq!(
            EnforcedRecord::from_report(&report(&[]), SandboxCaps::default(), false, false)
                .unix_note(),
            None
        );
    }

    #[test]
    fn requires_enforcement_names_the_items_that_are_not_enforced() {
        let caps = SandboxCaps::default();
        let weak = EnforcedRecord::from_report(
            &report(&[(attempt::WRITE_OUTSIDE, NO)]),
            caps.clone(),
            false,
            false,
        );
        assert_eq!(weak.check(&[Enforce::FsWrite]), Ok(()));
        assert_eq!(
            weak.check(&[Enforce::Network, Enforce::FsWrite, Enforce::Spawn]),
            Err(vec![Enforce::Network, Enforce::Spawn])
        );
        // the user accepted less
        let accepted = EnforcedRecord::from_report(&report(&[]), caps, true, false);
        assert_eq!(accepted.check(&[Enforce::Network]), Ok(()));
        assert!(accepted.allow_unenforced);
    }

    #[test]
    fn a_record_that_does_not_parse_is_nothing() {
        assert!(EnforcedRecord::parse(None).is_none());
        assert!(EnforcedRecord::parse(Some("{")).is_none());
        let record = EnforcedRecord::from_report(&report(&[]), SandboxCaps::default(), true, false);
        let text = serde_json::to_string(&record).unwrap();
        assert_eq!(EnforcedRecord::parse(Some(&text)), Some(record));
    }
}
