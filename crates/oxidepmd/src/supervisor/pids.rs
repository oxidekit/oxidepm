//! Which processes the daemon started, and the sweep of the ones a dead daemon left behind.
//!
//! Children do not survive the daemon. A daemon that is stopped by a signal stops them itself;
//! one that dies by SIGKILL or the OOM killer cannot, so every launch is recorded in
//! `pids.json` and the next daemon kills what is still there before it starts anything. Without
//! it the `resurrect` that follows would start duplicates fighting over the same ports.
//!
//! The in-memory map and the file are updated under one mutex, so concurrent actors cannot lose
//! an update. The file is rewritten as a whole (a few hundred bytes) through a temporary file
//! and a rename; it is not fsynced, because the one thing it must survive is the death of the
//! daemon process, and after a reboot its boot id no longer matches.
//!
//! Linux only (`/proc`); elsewhere the sweep says it is unavailable.

use super::persist;
use nix::sys::signal::Signal;
use oxidepm_runtime::kill_group;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// How long a swept group gets between SIGTERM and SIGKILL.
const SWEEP_GRACE: Duration = Duration::from_secs(5);

/// One live app process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    pub app_id: u32,
    pub name: String,
    pub pid: u32,
    pub pgid: i32,
    /// Start time of the leader (`/proc/<pid>/stat`, field 22): tells the process apart from a
    /// later one that got the same pid.
    pub start_ticks: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct PidsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    boot_id: String,
    #[serde(default)]
    apps: Vec<PidRecord>,
}

/// The kernel's id of this boot; empty where there is none.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The live processes of this daemon, mirrored to `pids.json`.
pub struct PidTable {
    path: PathBuf,
    boot_id: String,
    records: Mutex<BTreeMap<u32, PidRecord>>,
}

/// Removes its app's record when dropped: held by the actor next to its child, so the record
/// goes when the child does, whichever way the actor ends.
pub struct PidGuard {
    table: Arc<PidTable>,
    app_id: u32,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        self.table.remove(self.app_id);
    }
}

impl PidTable {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            boot_id: boot_id(),
            records: Mutex::new(BTreeMap::new()),
        })
    }

    /// Record a process just after it was spawned. Synchronous on purpose: the actor calls it
    /// with no await between the spawn and this, so the only window in which a SIGKILL of the
    /// daemon loses a child is those microseconds (a pid cannot be recorded before the fork).
    pub fn add(self: &Arc<Self>, record: PidRecord) -> PidGuard {
        let app_id = record.app_id;
        let mut records = self.records.lock();
        records.insert(app_id, record);
        self.write(&records);
        PidGuard {
            table: Arc::clone(self),
            app_id,
        }
    }

    fn remove(&self, app_id: u32) {
        let mut records = self.records.lock();
        if records.remove(&app_id).is_some() {
            self.write(&records);
        }
    }

    /// The records now, by app id: the processes that are alive and recorded.
    pub fn snapshot(&self) -> Vec<PidRecord> {
        self.records.lock().values().cloned().collect()
    }

    /// The records, by app id.
    #[cfg(test)]
    pub fn records(&self) -> Vec<PidRecord> {
        self.records.lock().values().cloned().collect()
    }

    /// Write the file from `records`; the caller holds the lock, so no writer overtakes another.
    fn write(&self, records: &BTreeMap<u32, PidRecord>) {
        let file = PidsFile {
            version: 1,
            boot_id: self.boot_id.clone(),
            apps: records.values().cloned().collect(),
        };
        let result = serde_json::to_vec_pretty(&file)
            .map_err(oxidepm_core::Error::from)
            .and_then(|bytes| persist::write_atomic(&self.path, &bytes, false));
        if let Err(e) = result {
            warn!("Could not write {}: {}", self.path.display(), e);
        }
    }
}

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Records read from the file.
    pub recorded: usize,
    /// Process groups that had members and were killed.
    pub killed: Vec<i32>,
    /// Records whose pid now belongs to another process (left alone).
    pub recycled: usize,
}

/// One process, as `/proc/<pid>/stat` describes it.
#[derive(Debug, Clone, Copy)]
struct ProcInfo {
    pid: u32,
    zombie: bool,
    pgrp: i32,
    start_ticks: u64,
}

fn parse_stat(pid: u32, stat: &str) -> Option<ProcInfo> {
    // the command name (field 2) may hold spaces and parentheses: count from the last ')'
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?; // field 3
    let pgrp = fields.nth(1)?.parse().ok()?; // field 5
    let start_ticks = fields.nth(16)?.parse().ok()?; // field 22
    Some(ProcInfo {
        pid,
        zombie: state == "Z" || state == "X",
        pgrp,
        start_ticks,
    })
}

fn read_proc(pid: u32) -> Option<ProcInfo> {
    parse_stat(
        pid,
        &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?,
    )
}

/// Every process there is. A process that vanishes while the directory is read is skipped.
fn all_procs() -> Vec<ProcInfo> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter_map(read_proc)
        .collect()
}

/// The live members of group `pgid` that started no earlier than its recorded leader.
fn group_members(pgid: i32, leader_start: u64) -> Vec<ProcInfo> {
    all_procs()
        .into_iter()
        .filter(|p| !p.zombie && p.pgrp == pgid && p.start_ticks >= leader_start)
        .collect()
}

/// Kill what a daemon that died without stopping its apps left running, then empty the file.
///
/// For each record of this boot the sweep matches by process group, not by the leader alone: a
/// wrapper that died just before the daemon leaves members behind. A record whose leader pid is
/// alive with another start time belongs to a recycled pid (the kernel does not reuse a pid
/// while it names a group with members, so nothing of the old group is left) and is skipped.
/// Matching groups get SIGTERM, up to five seconds, then SIGKILL; each step is logged.
///
/// Blocking: the daemon runs it on a blocking thread before it starts anything.
pub fn sweep(path: &Path) -> SweepReport {
    let mut report = SweepReport::default();
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
        Err(e) => {
            warn!("Orphan sweep: cannot read {}: {}", path.display(), e);
            return report;
        }
    };
    let file: PidsFile = match serde_json::from_str(&content) {
        Ok(file) => file,
        Err(e) => {
            warn!(
                "Orphan sweep: {} is not usable ({}); ignoring it",
                path.display(),
                e
            );
            clear(path);
            return report;
        }
    };
    report.recorded = file.apps.len();

    if !cfg!(target_os = "linux") || !Path::new("/proc/self/stat").exists() {
        warn!(
            "Orphan sweep is unavailable on this platform; {} apps may have been left running",
            file.apps.len()
        );
        clear(path);
        return report;
    }
    let current = boot_id();
    if file.boot_id != current {
        if !file.apps.is_empty() {
            debug!(
                "Orphan sweep: {} is from another boot, nothing can be left",
                path.display()
            );
        }
        clear(path);
        return report;
    }

    let mut groups: Vec<(PidRecord, Vec<ProcInfo>)> = Vec::new();
    for record in file.apps {
        if let Some(leader) = read_proc(record.pid) {
            if !leader.zombie && leader.start_ticks != record.start_ticks {
                info!(
                    "Orphan sweep: pid {} ({}) now belongs to another process; leaving it alone",
                    record.pid, record.name
                );
                report.recycled += 1;
                continue;
            }
        }
        let members = group_members(record.pgid, record.start_ticks);
        if members.is_empty() {
            debug!(
                "Orphan sweep: nothing left of {} (group {})",
                record.name, record.pgid
            );
            continue;
        }
        groups.push((record, members));
    }

    for (record, members) in &groups {
        warn!(
            "Orphan sweep: {} (app {}) left process(es) {:?} in group {} by a daemon that died; sending SIGTERM",
            record.name,
            record.app_id,
            members.iter().map(|m| m.pid).collect::<Vec<_>>(),
            record.pgid
        );
        let _ = kill_group(record.pgid, Signal::SIGTERM);
    }
    let deadline = Instant::now() + SWEEP_GRACE;
    while Instant::now() < deadline
        && groups
            .iter()
            .any(|(record, _)| !group_members(record.pgid, record.start_ticks).is_empty())
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    for (record, _) in &groups {
        if !group_members(record.pgid, record.start_ticks).is_empty() {
            warn!(
                "Orphan sweep: group {} of {} ignored SIGTERM; sending SIGKILL",
                record.pgid, record.name
            );
            let _ = kill_group(record.pgid, Signal::SIGKILL);
        }
        report.killed.push(record.pgid);
    }
    // give the SIGKILLed processes a moment to go before anything is started
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && groups
            .iter()
            .any(|(record, _)| !group_members(record.pgid, record.start_ticks).is_empty())
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    clear(path);
    report
}

/// The file after a sweep: no records.
fn clear(path: &Path) {
    let empty = PidsFile {
        version: 1,
        boot_id: boot_id(),
        apps: Vec::new(),
    };
    let result = serde_json::to_vec_pretty(&empty)
        .map_err(oxidepm_core::Error::from)
        .and_then(|bytes| persist::write_atomic(path, &bytes, false));
    if let Err(e) = result {
        warn!("Could not empty {}: {}", path.display(), e);
    }
}

/// Start time of process `pid` in clock ticks since boot, as the sweep matches it.
pub fn start_ticks(pid: u32) -> u64 {
    read_start_ticks(pid).unwrap_or(0)
}

pub(super) fn read_start_ticks(pid: u32) -> Option<u64> {
    read_proc(pid).map(|p| p.start_ticks)
}

#[cfg(test)]
pub(super) fn parse_start_ticks(stat: &str) -> Option<u64> {
    parse_stat(0, stat).map(|p| p.start_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_survives_a_command_name_with_spaces_and_parentheses() {
        let stat = "1234 (my (odd) app) S 1 777 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 10000 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        let info = parse_stat(1234, stat).unwrap();
        assert_eq!(
            (info.pgrp, info.start_ticks, info.zombie),
            (777, 987654, false)
        );
        assert!(parse_stat(1, "garbage").is_none());
        assert!(parse_stat(1, "1 (x) S 1").is_none());
        let zombie = stat.replace(") S ", ") Z ");
        assert!(parse_stat(1234, &zombie).unwrap().zombie);
    }

    /// 32 threads add and remove records at once; the file and the map agree, nothing is lost.
    #[test]
    fn pids_file_survives_concurrent_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pids.json");
        let table = PidTable::new(path.clone());
        let threads: Vec<_> = (0..32u32)
            .map(|i| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    let guard = table.add(PidRecord {
                        app_id: i + 1,
                        name: format!("app-{i}"),
                        pid: 100_000 + i,
                        pgid: 100_000 + i as i32,
                        start_ticks: 5,
                    });
                    // the odd ones go again, the even ones stay
                    if i % 2 == 1 {
                        drop(guard);
                        None
                    } else {
                        Some(guard)
                    }
                })
            })
            .collect();
        let kept: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        let in_map: Vec<u32> = table.records().iter().map(|r| r.app_id).collect();
        let expected: Vec<u32> = (0..32u32).filter(|i| i % 2 == 0).map(|i| i + 1).collect();
        assert_eq!(in_map, expected, "no lost entry in the map");
        let file: PidsFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let on_disk: Vec<u32> = file.apps.iter().map(|r| r.app_id).collect();
        assert_eq!(on_disk, expected, "the file says the same");
        assert_eq!(file.boot_id, boot_id());

        drop(kept);
        let file: PidsFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(file.apps.is_empty(), "a guard takes its record with it");
        assert!(table.records().is_empty());
    }

    #[test]
    fn the_file_is_private_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pids.json");
        let table = PidTable::new(path.clone());
        let _guard = table.add(PidRecord {
            app_id: 1,
            name: "a".into(),
            pid: 1,
            pgid: 1,
            start_ticks: 1,
        });
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["pids.json"], "{names:?}");
    }

    #[test]
    fn a_missing_or_corrupt_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pids.json");
        assert_eq!(sweep(&path), SweepReport::default());
        assert!(!path.exists(), "nothing to empty");
        std::fs::write(&path, "{ nope").unwrap();
        assert_eq!(sweep(&path).recorded, 0);
        let after: PidsFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(after.apps.is_empty());
    }
}
