//! Attribute main-socket connections to running adapters.
//!
//! A buggy or hostile adapter that may spawn programs can open the main socket itself, or run the
//! CLI. Such a peer is in the adapter's process tree, so the daemon follows the `PPid` links of
//! the connecting process (at most [`MAX_HOPS`]) and compares each process and its start time
//! with the recorded adapter processes. A match makes the connection that adapter, under its
//! grant. The peer is looked at once, when the connection is accepted, and not for each request:
//! a process that connects, writes and exits cannot slip past the check after its `/proc` entry
//! is gone. A peer that cannot be looked at while an adapter is alive is not a person: it is
//! `ai:unresolved`, with `via: unattributed` in the audit log.
//!
//! The walk is pure ([`walk`] takes the table of adapter processes and a reader of `/proc`), so it
//! is tested without a process. The caller does it only while at least one adapter process is
//! recorded, and on a blocking thread.
//!
//! Honest limit: an adapter that double-forks and `setsid`s a helper, or starts the CLI
//! with its own `HOME`, is not found by the ancestry. The protection against that is the
//! environment scrub, the sandbox and the consent screen, not this walk.

use std::sync::atomic::{AtomicU64, Ordering};

/// The most parent links followed.
pub const MAX_HOPS: usize = 64;

/// One adapter process, as `adapter-pids.json` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterProc {
    pub name: String,
    pub pid: u32,
    /// Start time in clock ticks (`/proc/<pid>/stat` field 22): tells the process from a later
    /// one that got the same pid.
    pub start_ticks: u64,
}

/// What the walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Walk {
    /// The peer is this adapter or one of its descendants.
    Adapter(String),
    /// The chain ended at the top without meeting an adapter: not an adapter's.
    Outside,
    /// A process on the way could not be read, or the chain was longer than [`MAX_HOPS`].
    Unresolved,
}

/// `(parent pid, start ticks)` of a process, if it can be read.
pub type ReadProc<'a> = &'a dyn Fn(u32) -> Option<(u32, u64)>;

/// Follow the parents of `pid` through `read` and say whether the chain meets one of `adapters`.
/// A process that has an adapter's pid but another start time is a later process that got the
/// number, and is passed over.
pub fn walk(pid: u32, adapters: &[AdapterProc], read: ReadProc<'_>) -> Walk {
    let mut current = pid;
    for _ in 0..=MAX_HOPS {
        let Some((parent, ticks)) = read(current) else {
            return Walk::Unresolved;
        };
        if let Some(found) = adapters
            .iter()
            .find(|a| a.pid == current && a.start_ticks == ticks)
        {
            return Walk::Adapter(found.name.clone());
        }
        if parent <= 1 {
            return Walk::Outside;
        }
        current = parent;
    }
    Walk::Unresolved
}

pub(crate) fn stat_fields(stat: &[u8]) -> Option<std::str::SplitWhitespace<'_>> {
    // The command name (field 2) may hold parentheses, spaces and non-UTF-8 bytes.
    // Only decode the fields after its final ')'.
    let rest = &stat[stat.iter().rposition(|&byte| byte == b')')? + 1..];
    Some(std::str::from_utf8(rest).ok()?.split_whitespace())
}

/// `(ppid, start ticks)` from the bytes of `/proc/<pid>/stat`.
pub fn parse_stat(stat: &[u8]) -> Option<(u32, u64)> {
    let mut fields = stat_fields(stat)?;
    let _state = fields.next()?; // field 3
    let ppid = fields.next()?.parse().ok()?; // field 4
    let start_ticks = fields.nth(17)?.parse().ok()?; // field 22
    Some((ppid, start_ticks))
}

/// Reads `/proc`, counting the reads (a daemon with no adapter must make none).
#[derive(Default)]
pub struct ProcReader {
    reads: AtomicU64,
}

impl ProcReader {
    /// How many `/proc/<pid>/stat` files were opened so far.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// The parent and start time of `pid`, or `None` when it is gone or unreadable.
    pub fn read(&self, pid: u32) -> Option<(u32, u64)> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        parse_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)
    }
}

#[cfg(test)]
pub(crate) fn with_non_utf8_child<T>(inspect: impl FnOnce(u32) -> T) -> T {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let command = dir.path().join(OsStr::from_bytes(b"my ) \xff cat"));
    symlink("/bin/cat", &command).unwrap();
    let mut child = Command::new(command).stdin(Stdio::piped()).spawn().unwrap();
    let stat_path = format!("/proc/{}/stat", child.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stat = std::fs::read(&stat_path);
    while stat
        .as_ref()
        .is_ok_and(|bytes| std::str::from_utf8(bytes).is_ok())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
        stat = std::fs::read(&stat_path);
    }
    let result = inspect(child.id());
    assert!(child.wait().unwrap().success());
    assert!(std::str::from_utf8(&stat.unwrap()).is_err());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn table(entries: &[(u32, u32, u64)]) -> HashMap<u32, (u32, u64)> {
        entries
            .iter()
            .map(|(pid, ppid, ticks)| (*pid, (*ppid, *ticks)))
            .collect()
    }

    fn adapter(name: &str, pid: u32, ticks: u64) -> AdapterProc {
        AdapterProc {
            name: name.to_string(),
            pid,
            start_ticks: ticks,
        }
    }

    #[test]
    fn the_adapter_itself_and_its_descendants_are_found() {
        let procs = table(&[(10, 1, 100), (11, 10, 110), (12, 11, 120)]);
        let read = |pid: u32| procs.get(&pid).copied();
        let adapters = [adapter("ui", 10, 100)];
        for pid in [10, 11, 12] {
            assert_eq!(
                walk(pid, &adapters, &read),
                Walk::Adapter("ui".to_string()),
                "pid {pid}"
            );
        }
    }

    #[test]
    fn a_process_outside_every_tree_is_outside() {
        let procs = table(&[(10, 1, 100), (20, 1, 200), (21, 20, 210)]);
        let read = |pid: u32| procs.get(&pid).copied();
        assert_eq!(walk(21, &[adapter("ui", 10, 100)], &read), Walk::Outside);
    }

    #[test]
    fn a_recycled_pid_is_not_the_adapter() {
        // pid 10 now belongs to another process (another start time)
        let procs = table(&[(10, 1, 999), (11, 10, 1000)]);
        let read = |pid: u32| procs.get(&pid).copied();
        assert_eq!(walk(11, &[adapter("ui", 10, 100)], &read), Walk::Outside);
    }

    #[test]
    fn a_peer_that_cannot_be_read_is_unresolved() {
        let read = |_pid: u32| None;
        assert_eq!(walk(5, &[adapter("ui", 10, 100)], &read), Walk::Unresolved);
        // or a link of the chain that has gone
        let procs = table(&[(11, 10, 110)]);
        let read = |pid: u32| procs.get(&pid).copied();
        assert_eq!(walk(11, &[adapter("ui", 10, 100)], &read), Walk::Unresolved);
    }

    #[test]
    fn the_walk_follows_at_most_64_parents() {
        // a chain of 200 processes, the adapter at the top
        let mut entries = vec![(1000u32, 1u32, 1u64)];
        for i in 1..200u32 {
            entries.push((1000 + i, 999 + i, 1 + u64::from(i)));
        }
        let procs = table(&entries);
        let reads = std::cell::Cell::new(0usize);
        let read = |pid: u32| {
            reads.set(reads.get() + 1);
            procs.get(&pid).copied()
        };
        let adapters = [adapter("ui", 1000, 1)];
        // 64 hops away is still found; further is not followed, and is stricter than a person
        assert_eq!(
            walk(1000 + 64, &adapters, &read),
            Walk::Adapter("ui".to_string())
        );
        reads.set(0);
        assert_eq!(walk(1000 + 199, &adapters, &read), Walk::Unresolved);
        assert_eq!(reads.get(), MAX_HOPS + 1, "the walk stops at the limit");
    }

    #[test]
    fn stat_parsing_survives_a_command_name_with_spaces_and_parentheses() {
        let stat = b"1234 (my (odd) app) S 77 777 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 10000 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(parse_stat(stat), Some((77, 987654)));
        assert_eq!(parse_stat(b"garbage"), None);
        assert_eq!(parse_stat(b"1 (x) S 1"), None);
    }

    #[test]
    fn this_process_can_be_read() {
        let reader = ProcReader::default();
        let me = std::process::id();
        let (ppid, ticks) = reader.read(me).expect("our own stat");
        assert!(ppid > 0 && ticks > 0);
        assert_eq!(reader.reads(), 1);
    }

    #[test]
    fn reads_a_process_with_a_non_utf8_name() {
        let reader = ProcReader::default();
        let (parent, ticks) = with_non_utf8_child(|pid| reader.read(pid)).unwrap();
        assert_eq!(parent, std::process::id());
        assert!(ticks > 0);
        assert_eq!(reader.reads(), 1);
    }
}
