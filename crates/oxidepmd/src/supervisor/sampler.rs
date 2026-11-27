//! The metrics sampler measures every live app in one pass over /proc on a blocking worker.
//!
//! It publishes tree totals alongside the leader metrics. The sampler never signals anything.
//! A leader over its memory limit causes a mailbox post tagged with the measured generation.

use super::actor::AbortOnDrop;
use super::handle::{Command, HandleKind, LimitKind, LiveProc, ProcessHandle};
#[cfg(test)]
use super::pids::parse_start_ticks;
pub(super) use super::pids::read_start_ticks as start_ticks;
use super::pids::stat_fields;
use super::registry::Registry;
use super::stats::Reading;
use nix::unistd::{sysconf, SysconfVar};
use oxidepm_core::constants::DEFAULT_METRICS_INTERVAL_SECS;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const MAX_PROC_ENTRIES: usize = 32_768;
const MAX_TREE_PROCESSES: usize = 4_096;
const MAX_TREE_DEPTH: usize = 64;

/// Where the numbers come from (tests replace it).
pub(super) trait Probe: Send + 'static {
    /// The start time of `pid` in clock ticks since boot, `None` if there is no such process.
    fn start_ticks(&self, pid: u32) -> Option<u64>;

    /// Measure these apps (the leader pids were validated just before). Forget old baselines.
    fn measure(&mut self, leaders: &[LiveProc]) -> HashMap<u32, Reading>;
}

/// What the whole process tree of an adapter uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupUsage {
    pub processes: u32,
    pub threads: u64,
    pub rss_bytes: u64,
}

/// The fields of `/proc/<pid>/stat` used for sampling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatLine {
    pub ppid: i32,
    pub pgrp: i32,
    pub threads: u64,
    pub rss_pages: u64,
    session: i32,
    start_ticks: u64,
    cpu_ticks: u64,
    zombie: bool,
}

/// Parse one `/proc/<pid>/stat` line.
fn parse_stat(stat: &[u8]) -> Option<StatLine> {
    let mut fields = stat_fields(stat)?;
    let zombie = matches!(fields.next()?, "Z" | "X"); // field 3
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    let user_ticks: u64 = fields.nth(7)?.parse().ok()?; // field 14
    let system_ticks = fields.next()?.parse().ok()?;
    let threads = fields.nth(4)?.parse().ok()?; // field 20
    let start_ticks = fields.nth(1)?.parse().ok()?; // field 22
    let rss_pages: i64 = fields.nth(1)?.parse().ok()?; // field 24
    Some(StatLine {
        ppid,
        pgrp,
        threads,
        rss_pages: rss_pages.max(0) as u64,
        session,
        start_ticks,
        cpu_ticks: user_ticks.saturating_add(system_ticks),
        zombie,
    })
}

fn read_stat(pid: u32) -> Option<StatLine> {
    parse_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)
}

fn read_resident_pages(pid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The usage of the group `pgid` led by `leader`, from a table of every process: the members of
/// the group plus every descendant of the leader.
pub fn sum_group(table: &[(i32, StatLine)], leader: i32, pgid: i32, page_size: u64) -> GroupUsage {
    let mut members: Vec<i32> = table
        .iter()
        .filter(|(pid, line)| line.pgrp == pgid || *pid == leader)
        .map(|(pid, _)| *pid)
        .collect();
    // descendants by parent pid, which reaches helpers that left the group with `setsid`
    let mut frontier = vec![leader];
    while let Some(parent) = frontier.pop() {
        for (pid, line) in table {
            if line.ppid == parent && !members.contains(pid) {
                members.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    let mut usage = GroupUsage::default();
    for (pid, line) in table {
        if members.contains(pid) {
            usage.processes += 1;
            usage.threads += line.threads;
            usage.rss_bytes += line.rss_pages * page_size;
        }
    }
    usage
}

/// Measure an adapter's tree now. Blocking: reads `/proc`. A process that exits while the table
/// is read is skipped.
pub fn group_usage(leader: u32, pgid: i32) -> GroupUsage {
    let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .map(|v| v as u64)
        .unwrap_or(4096);
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return GroupUsage::default();
    };
    let table: Vec<(i32, StatLine)> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
        .filter_map(|pid| Some((pid, read_stat(pid as u32)?)))
        .collect();
    sum_group(&table, leader as i32, pgid, page_size)
}

/// A cgroup v2 path as seen from the daemon's cgroup namespace.
fn cgroup_path(pid: u32) -> Option<PathBuf> {
    let entry = std::fs::read(format!("/proc/{pid}/cgroup")).ok()?;
    let path = entry
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"0::"))?;
    Some(PathBuf::from(OsStr::from_bytes(path)))
}

/// Look up members only when an app occupies a subtree separate from the daemon and other apps.
fn cgroup_members(leaders: &[LiveProc], table: &HashMap<u32, StatLine>) -> HashMap<u32, Vec<u32>> {
    let Some(daemon) = cgroup_path(std::process::id()) else {
        return HashMap::new();
    };
    let paths: Vec<_> = leaders
        .iter()
        .filter_map(|leader| {
            let path = cgroup_path(leader.pid)?;
            (start_ticks(leader.pid) == Some(leader.start_ticks)).then_some((leader.pid, path))
        })
        .collect();
    let scopes: HashMap<&Path, u32> = paths
        .iter()
        .filter(|(pid, path)| {
            path.is_absolute()
                && !daemon.starts_with(path)
                && !paths
                    .iter()
                    .any(|(other_pid, other_path)| pid != other_pid && other_path.starts_with(path))
        })
        .map(|(pid, path)| (path.as_path(), *pid))
        .collect();
    let mut members = HashMap::<u32, Vec<u32>>::new();
    if !scopes.is_empty() {
        for (&pid, stat) in table {
            let Some(leader) = cgroup_path(pid)
                .and_then(|path| path.ancestors().find_map(|path| scopes.get(path)).copied())
            else {
                continue;
            };
            if start_ticks(pid) == Some(stat.start_ticks) {
                members.entry(leader).or_default().push(pid);
            }
        }
    }
    members
}

fn tree_members(
    leader: LiveProc,
    table: &HashMap<u32, StatLine>,
    children: &HashMap<u32, Vec<u32>>,
    cgroup: &[u32],
) -> Vec<u32> {
    let lead = table[&leader.pid];
    // Apps normally inherit the daemon's session. Only an app-owned session identifies members.
    let owns_session = lead.session == leader.pid as i32;
    let peers = table.iter().filter_map(|(&pid, stat)| {
        (stat.pgrp == leader.pgid || (owns_session && stat.session == lead.session)).then_some(pid)
    });
    let mut seen = HashSet::from([leader.pid]);
    let mut queue = vec![(leader.pid, 0)];
    let mut add = |queue: &mut Vec<_>, pid, depth, oldest| {
        if table[&pid].start_ticks >= oldest && seen.insert(pid) {
            queue.push((pid, depth));
        }
    };
    for pid in peers.chain(cgroup.iter().copied()) {
        if queue.len() == MAX_TREE_PROCESSES {
            break;
        }
        add(&mut queue, pid, 0, lead.start_ticks);
    }
    let mut cursor = 0;
    while cursor < queue.len() && queue.len() < MAX_TREE_PROCESSES {
        let (parent, depth) = queue[cursor];
        cursor += 1;
        if depth == MAX_TREE_DEPTH {
            continue;
        }
        for &pid in children.get(&parent).into_iter().flatten() {
            if queue.len() == MAX_TREE_PROCESSES {
                break;
            }
            // An older child cannot have been forked by this incarnation of the parent.
            add(&mut queue, pid, depth + 1, table[&parent].start_ticks);
        }
    }
    queue.into_iter().map(|(pid, _)| pid).collect()
}

pub(super) struct SysProbe {
    previous: HashMap<u32, StatLine>,
    sampled_at: Option<Instant>,
    page_size: u64,
    ticks_per_second: f32,
}

impl Default for SysProbe {
    fn default() -> Self {
        Self {
            previous: HashMap::new(),
            sampled_at: None,
            page_size: sysconf(SysconfVar::PAGE_SIZE)
                .ok()
                .flatten()
                .unwrap_or(4096) as u64,
            ticks_per_second: sysconf(SysconfVar::CLK_TCK).ok().flatten().unwrap_or(100) as f32,
        }
    }
}

impl Probe for SysProbe {
    fn start_ticks(&self, pid: u32) -> Option<u64> {
        start_ticks(pid)
    }

    fn measure(&mut self, leaders: &[LiveProc]) -> HashMap<u32, Reading> {
        let previous = std::mem::take(&mut self.previous);
        let now = Instant::now();
        let elapsed = self
            .sampled_at
            .replace(now)
            .map(|time| (now - time).as_secs_f32());
        if leaders.is_empty() {
            return HashMap::new();
        }
        let mut table: HashMap<u32, StatLine> = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .take(MAX_PROC_ENTRIES)
            .filter_map(|pid| Some((pid, read_stat(pid)?)))
            .collect();
        let leaders: Vec<_> = leaders
            .iter()
            .copied()
            .filter(|leader| {
                // Also covers leaders outside the bounded census and ones that exited during it.
                table.remove(&leader.pid);
                let Some(stat) = read_stat(leader.pid) else {
                    return false;
                };
                table.insert(leader.pid, stat);
                stat.start_ticks == leader.start_ticks && !stat.zombie
            })
            .collect();
        table.retain(|_, stat| !stat.zombie);
        let mut children = HashMap::<u32, Vec<u32>>::new();
        for (&pid, stat) in &table {
            children.entry(stat.ppid as u32).or_default().push(pid);
        }
        let cgroups = cgroup_members(&leaders, &table);
        let mut readings = HashMap::new();
        for leader in leaders {
            // Keep the same resident-memory source for the leader's existing limit.
            let leader_rss =
                read_resident_pages(leader.pid).unwrap_or(table[&leader.pid].rss_pages);
            if start_ticks(leader.pid) != Some(leader.start_ticks) {
                continue;
            }
            let cgroup = cgroups.get(&leader.pid).map_or(&[][..], Vec::as_slice);
            let mut reading = Reading {
                mem_bytes: leader_rss.saturating_mul(self.page_size),
                ..Reading::default()
            };
            for pid in tree_members(leader, &table, &children, cgroup) {
                let stat = table[&pid];
                let cpu = previous
                    .get(&pid)
                    .filter(|old| old.start_ticks == stat.start_ticks)
                    .zip(elapsed.filter(|&seconds| seconds > 0.0))
                    .map_or(0.0, |(old, seconds)| {
                        100.0 * stat.cpu_ticks.saturating_sub(old.cpu_ticks) as f32
                            / (seconds * self.ticks_per_second)
                    });
                let rss = stat.rss_pages.saturating_mul(self.page_size);
                self.previous.insert(pid, stat);
                if pid == leader.pid {
                    reading.cpu_percent = cpu;
                }
                reading.tree.process_count += 1;
                reading.tree.cpu_percent += cpu;
                reading.tree.memory_bytes = reading.tree.memory_bytes.saturating_add(rss);
            }
            readings.insert(leader.pid, reading);
        }
        readings
    }
}

/// How often the sampler runs: two seconds, or `OXIDEPM_SAMPLE_INTERVAL_MS` (a test knob,
/// 20 ms to 60 s).
pub fn sample_interval() -> Duration {
    let default = Duration::from_secs(DEFAULT_METRICS_INTERVAL_SECS);
    match std::env::var("OXIDEPM_SAMPLE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(ms) if (20..=60_000).contains(&ms) => Duration::from_millis(ms),
        _ => default,
    }
}

/// The task: sample every interval until the registry is gone.
pub fn spawn(registry: Weak<Registry>) -> AbortOnDrop {
    let sampler = Sampler::new(SysProbe::default());
    AbortOnDrop::new(tokio::spawn(async move {
        let mut interval = tokio::time::interval(sample_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // the first tick is immediate; nothing has started yet
        loop {
            interval.tick().await;
            let Some(registry) = registry.upgrade() else {
                return;
            };
            let handles = registry.list();
            drop(registry);
            sampler.tick(&handles).await;
        }
    }))
}

pub(super) struct Sampler<P: Probe> {
    probe: Arc<Mutex<P>>,
}

impl<P: Probe> Sampler<P> {
    pub fn new(probe: P) -> Self {
        Self {
            probe: Arc::new(Mutex::new(probe)),
        }
    }

    /// Measure the apps among `handles` that have a live child, store the numbers in their
    /// `Live`, and post the memory limit of the ones over it.
    pub async fn tick(&self, handles: &[ProcessHandle]) {
        let tracked: Vec<(ProcessHandle, LiveProc)> = handles
            .iter()
            .filter(|h| h.kind == HandleKind::Process)
            .filter_map(|h| h.live().proc().map(|proc| (h.clone(), proc)))
            .collect();
        let leaders: Vec<LiveProc> = tracked.iter().map(|(_, proc)| *proc).collect();

        let probe = Arc::clone(&self.probe);
        let readings = if leaders.is_empty() {
            // nothing to read: the probe only forgets what it kept
            probe.lock().measure(&[]);
            return;
        } else {
            // reading /proc is blocking I/O: not on an async worker
            match tokio::task::spawn_blocking(move || sample(&mut *probe.lock(), &leaders)).await {
                Ok(readings) => readings,
                Err(e) => {
                    warn!("The metrics sampler failed: {}", e);
                    return;
                }
            }
        };

        for ((handle, proc), reading) in tracked.into_iter().zip(readings) {
            let Some(reading) = reading else { continue };
            // the child may have been reaped or replaced while the reading was taken
            if !handle.live().set_metrics(proc, reading) {
                continue;
            }
            post_memory_limit(&handle, proc, reading.mem_bytes);
        }
    }
}

/// Validate every leader's start time, measure the ones that match. One entry per leader.
fn sample<P: Probe>(probe: &mut P, leaders: &[LiveProc]) -> Vec<Option<Reading>> {
    let valid: Vec<LiveProc> = leaders
        .iter()
        .copied()
        .filter(|leader| {
            let ok = probe.start_ticks(leader.pid) == Some(leader.start_ticks);
            if !ok {
                // a recycled pid, or a child that is already gone
                debug!(
                    "Not measuring pid {}: it is not the process that was launched",
                    leader.pid
                );
            }
            ok
        })
        .collect();
    let mut measured = probe.measure(&valid);
    leaders
        .iter()
        .map(|leader| {
            if valid.contains(leader) {
                measured.remove(&leader.pid)
            } else {
                None
            }
        })
        .collect()
}

/// Post `LimitExceeded` for the child `proc` if it is over its limit and no post for it is
/// pending (the flag is cleared again by the next launch, or here if the mailbox was full).
fn post_memory_limit(handle: &ProcessHandle, proc: LiveProc, mem_bytes: u64) {
    let Some(limit_mb) = handle.snapshot().spec.max_memory_mb else {
        return;
    };
    let observed_mb = mem_bytes / (1024 * 1024);
    if observed_mb <= limit_mb || !handle.live().begin_limit_restart() {
        return;
    }
    let posted = handle.try_post(Command::LimitExceeded {
        gen: proc.gen,
        kind: LimitKind::Memory {
            observed_mb,
            limit_mb,
        },
    });
    if !posted {
        // full or gone: try again at the next tick
        handle.live().clear_limit_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::handle::{HandleParts, Snapshot, MAILBOX};

    fn line(ppid: i32, pgrp: i32, threads: u64, rss_pages: u64) -> StatLine {
        StatLine {
            ppid,
            pgrp,
            threads,
            rss_pages,
            ..StatLine::default()
        }
    }

    #[test]
    fn a_stat_line_with_spaces_and_parentheses_in_the_name_is_read() {
        let stat = "4242 (my (odd) name) S 100 200 300 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 7 0 12345 1000000 321 18446744073709551615 0 0 0";
        assert_eq!(
            parse_stat(stat.as_bytes()),
            Some(StatLine {
                session: 300,
                start_ticks: 12345,
                cpu_ticks: 11,
                ..line(100, 200, 7, 321)
            })
        );
        assert_eq!(parse_stat(b"garbage"), None);
        assert_eq!(parse_stat(b"1 (x) S 1"), None);
    }

    #[test]
    fn the_group_sum_counts_members_and_descendants_that_left_the_group_but_nothing_else() {
        let table = vec![
            (100, line(1, 100, 2, 10)),   // the leader
            (101, line(100, 100, 1, 20)), // in the group
            (102, line(100, 102, 4, 30)), // a child that called setsid
            (103, line(102, 102, 1, 40)), // its own child
            (104, line(103, 104, 1, 50)), // orphaned helper with a different parent chain: counted through 103
            (200, line(1, 200, 9, 999)),  // unrelated
            (201, line(100, 100, 1, 5)),  // in the group
            (300, line(1, 100, 1, 7)),    // reparented to init but still in the group
        ];
        let usage = sum_group(&table, 100, 100, 4096);
        assert_eq!(
            usage,
            GroupUsage {
                processes: 7,
                threads: 2 + 1 + 4 + 1 + 1 + 1 + 1,
                rss_bytes: (10 + 20 + 30 + 40 + 50 + 5 + 7) * 4096
            }
        );
        // a pgid with nobody in it and a leader that is gone
        assert_eq!(sum_group(&table, 999, 999, 4096), GroupUsage::default());
    }

    #[test]
    fn the_real_group_sum_sees_this_process() {
        let me = std::process::id();
        let pgid = nix::unistd::getpgrp().as_raw();
        let usage = group_usage(me, pgid);
        assert!(
            usage.processes >= 1 && usage.threads >= 1 && usage.rss_bytes > 0,
            "{usage:?}"
        );
    }

    #[test]
    fn tree_combines_membership_sources_and_ignores_an_older_child() {
        let table: HashMap<_, _> = [
            (100, 1, 100, 100, 10),   // leader
            (101, 1, 100, 100, 11),   // reparented group member
            (102, 101, 102, 102, 12), // detached child of that group member
            (103, 1, 103, 100, 12),   // reparented member of the leader's session
            (104, 1, 104, 104, 12),   // cgroup member
            (105, 102, 105, 105, 11), // older than its reported parent
            (106, 1, 106, 106, 12),   // unrelated
        ]
        .into_iter()
        .map(|(pid, ppid, pgrp, session, start_ticks)| {
            (
                pid,
                StatLine {
                    session,
                    start_ticks,
                    ..line(ppid, pgrp, 1, 1)
                },
            )
        })
        .collect();
        let children = HashMap::from([
            (1, vec![100, 101, 103, 104, 106]),
            (101, vec![102]),
            (102, vec![105]),
        ]);
        let mut members = tree_members(proc(100, 1, 10), &table, &children, &[104, 101]);
        members.sort_unstable();
        assert_eq!(members, [100, 101, 102, 103, 104]);
    }

    #[test]
    fn tree_walk_caps_depth_and_process_count() {
        let count = MAX_TREE_PROCESSES as u32 + 1;
        let mut table: HashMap<_, _> = (1..=count)
            .map(|pid| (pid, line(pid as i32 - 1, pid as i32, 1, 1)))
            .collect();
        let children = (1..count).map(|pid| (pid, vec![pid + 1])).collect();
        assert_eq!(
            tree_members(proc(1, 1, 0), &table, &children, &[]).len(),
            MAX_TREE_DEPTH + 1
        );
        for stat in table.values_mut() {
            stat.pgrp = 1;
        }
        assert_eq!(
            tree_members(proc(1, 1, 0), &table, &children, &[]).len(),
            MAX_TREE_PROCESSES
        );
    }

    use oxidepm_core::{AppSpec, Desired, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the probe says about each pid, and how often it was asked to measure.
    #[derive(Clone, Default)]
    struct Fake {
        ticks: Arc<Mutex<HashMap<u32, u64>>>,
        readings: Arc<Mutex<HashMap<u32, Reading>>>,
        measured: Arc<Mutex<Vec<Vec<u32>>>>,
        measure_calls: Arc<AtomicUsize>,
    }

    impl Probe for Fake {
        fn start_ticks(&self, pid: u32) -> Option<u64> {
            self.ticks.lock().get(&pid).copied()
        }

        fn measure(&mut self, leaders: &[LiveProc]) -> HashMap<u32, Reading> {
            self.measure_calls.fetch_add(1, Ordering::SeqCst);
            self.measured
                .lock()
                .push(leaders.iter().map(|p| p.pid).collect());
            let readings = self.readings.lock();
            leaders
                .iter()
                .filter_map(|p| readings.get(&p.pid).map(|r| (p.pid, *r)))
                .collect()
        }
    }

    fn handle(max_memory_mb: Option<u64>) -> (ProcessHandle, HandleParts) {
        let mut spec = AppSpec::new(
            "sampled".to_string(),
            Runtime::CMD,
            "true".to_string(),
            "/".into(),
        );
        spec.id = 1;
        spec.max_memory_mb = max_memory_mb;
        ProcessHandle::channel(
            1,
            HandleKind::Process,
            Snapshot::new(Arc::new(spec), Desired::Running, None),
        )
    }

    fn proc(pid: u32, gen: u64, start_ticks: u64) -> LiveProc {
        LiveProc {
            pid,
            pgid: pid as i32,
            gen,
            start_ticks,
        }
    }

    fn reading(mb: u64) -> Reading {
        Reading {
            cpu_percent: 1.5,
            mem_bytes: mb * 1024 * 1024,
            ..Reading::default()
        }
    }

    fn drain(parts: &mut HandleParts) -> Vec<Command> {
        let mut out = Vec::new();
        while let Ok(cmd) = parts.rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    #[test]
    fn start_ticks_is_field_22_even_when_the_name_has_spaces_and_parentheses() {
        let stat = "123 (my (odd) name) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 100 18446744073709551615";
        assert_eq!(parse_start_ticks(stat.as_bytes()), Some(987654));
        let mut non_utf8 = stat.as_bytes().to_vec();
        non_utf8[5] = 0xff;
        assert_eq!(parse_start_ticks(&non_utf8), Some(987654));
        assert_eq!(parse_start_ticks(b"garbage"), None);
        assert_eq!(parse_start_ticks(b"1 (x) S 1"), None);
        // and it works on this very process
        assert!(start_ticks(std::process::id()).is_some());
    }

    #[tokio::test]
    async fn sampler_skips_stale_start_ticks() {
        let (h, mut parts) = handle(Some(20));
        // launched as (pid 40, start 1000); the pid now belongs to a process that started at 2000
        h.live().set_proc(Some(proc(40, 1, 1000)));
        let fake = Fake::default();
        fake.ticks.lock().insert(40, 2000);
        fake.readings.lock().insert(40, reading(500));
        let sampler = Sampler::new(fake.clone());

        sampler.tick(std::slice::from_ref(&h)).await;

        assert_eq!(
            h.live().mem_bytes(),
            0,
            "a stranger's numbers are not stored"
        );
        assert!(
            drain(&mut parts).is_empty(),
            "and no limit is posted for it"
        );
        assert_eq!(
            *fake.measured.lock(),
            vec![Vec::<u32>::new()],
            "the pid was not even read"
        );

        // the same pid with the matching start time is measured
        fake.ticks.lock().insert(40, 1000);
        sampler.tick(std::slice::from_ref(&h)).await;
        assert_eq!(h.live().mem_bytes(), 500 * 1024 * 1024);
        assert_eq!(drain(&mut parts).len(), 1);
    }

    #[tokio::test]
    async fn sampler_posts_limit_once_until_relaunch() {
        let (h, mut parts) = handle(Some(20));
        h.live().set_proc(Some(proc(41, 1, 10)));
        let fake = Fake::default();
        fake.ticks.lock().insert(41, 10);
        fake.readings.lock().insert(41, reading(80));
        let sampler = Sampler::new(fake.clone());

        sampler.tick(std::slice::from_ref(&h)).await;
        sampler.tick(std::slice::from_ref(&h)).await;
        sampler.tick(std::slice::from_ref(&h)).await;

        let posted = drain(&mut parts);
        assert_eq!(
            posted.len(),
            1,
            "one post however many ticks see the child over the limit"
        );
        match &posted[0] {
            Command::LimitExceeded { gen, kind } => {
                assert_eq!(*gen, 1);
                assert_eq!(
                    *kind,
                    LimitKind::Memory {
                        observed_mb: 80,
                        limit_mb: 20
                    }
                );
            }
            _ => panic!("expected LimitExceeded"),
        }

        // a relaunch (the actor does exactly this) re-arms it for the new child
        h.live().set_proc(Some(proc(42, 2, 11)));
        h.live().clear_limit_pending();
        fake.ticks.lock().insert(42, 11);
        fake.readings.lock().insert(42, reading(90));
        sampler.tick(std::slice::from_ref(&h)).await;
        let posted = drain(&mut parts);
        assert_eq!(posted.len(), 1);
        assert!(matches!(posted[0], Command::LimitExceeded { gen: 2, .. }));
    }

    #[tokio::test]
    async fn sampler_posts_nothing_under_the_limit_or_without_one() {
        let (under, mut under_parts) = handle(Some(100));
        under.live().set_proc(Some(proc(43, 1, 10)));
        let (unlimited, mut unlimited_parts) = handle(None);
        unlimited.live().set_proc(Some(proc(44, 1, 10)));
        let fake = Fake::default();
        for pid in [43, 44] {
            fake.ticks.lock().insert(pid, 10);
            fake.readings.lock().insert(pid, reading(80));
        }
        let sampler = Sampler::new(fake);

        sampler.tick(&[under.clone(), unlimited.clone()]).await;

        assert!(drain(&mut under_parts).is_empty());
        assert!(drain(&mut unlimited_parts).is_empty());
        assert_eq!(
            under.live().mem_bytes(),
            80 * 1024 * 1024,
            "the numbers are stored anyway"
        );
        assert_eq!(unlimited.live().mem_bytes(), 80 * 1024 * 1024);
    }

    #[tokio::test]
    async fn sampler_retries_at_the_next_tick_when_the_mailbox_is_full() {
        let (h, mut parts) = handle(Some(20));
        h.live().set_proc(Some(proc(45, 1, 10)));
        for _ in 0..MAILBOX {
            assert!(h.try_post(Command::HealthOk { gen: 99 }));
        }
        let fake = Fake::default();
        fake.ticks.lock().insert(45, 10);
        fake.readings.lock().insert(45, reading(80));
        let sampler = Sampler::new(fake);

        sampler.tick(std::slice::from_ref(&h)).await;
        assert_eq!(
            drain(&mut parts).len(),
            MAILBOX,
            "nothing was added to the full mailbox"
        );

        sampler.tick(std::slice::from_ref(&h)).await;
        let posted = drain(&mut parts);
        assert_eq!(
            posted.len(),
            1,
            "the flag was cleared, so the next tick posted"
        );
        assert!(matches!(posted[0], Command::LimitExceeded { .. }));
    }

    #[tokio::test]
    async fn sampler_clears_proc_after_reap_so_dead_pid_is_never_measured() {
        let (h, mut parts) = handle(Some(20));
        let fake = Fake::default();
        fake.ticks.lock().insert(46, 10);
        fake.readings.lock().insert(46, reading(80));
        let sampler = Sampler::new(fake.clone());

        // the child was reaped: the actor cleared `proc` before anything else
        h.live().set_proc(Some(proc(46, 1, 10)));
        h.live().set_proc(None);
        sampler.tick(std::slice::from_ref(&h)).await;

        assert_eq!(
            fake.measure_calls.load(Ordering::SeqCst),
            1,
            "only the empty clean-up call"
        );
        assert_eq!(
            *fake.measured.lock(),
            vec![Vec::<u32>::new()],
            "the dead pid was never read"
        );
        assert_eq!(h.live().mem_bytes(), 0);
        assert!(drain(&mut parts).is_empty());
    }

    #[tokio::test]
    async fn sampler_drops_a_reading_taken_for_a_child_that_was_replaced_meanwhile() {
        // the reading comes back after `proc` changed: it belongs to the old child
        struct Replacing(Arc<ProcessHandle>);
        impl Probe for Replacing {
            fn start_ticks(&self, _pid: u32) -> Option<u64> {
                Some(10)
            }
            fn measure(&mut self, leaders: &[LiveProc]) -> HashMap<u32, Reading> {
                if !leaders.is_empty() {
                    self.0.live().set_proc(Some(proc(48, 2, 20)));
                }
                leaders.iter().map(|p| (p.pid, reading(80))).collect()
            }
        }
        let (h, mut parts) = handle(Some(20));
        h.live().set_proc(Some(proc(47, 1, 10)));
        let sampler = Sampler::new(Replacing(Arc::new(h.clone())));

        sampler.tick(std::slice::from_ref(&h)).await;

        assert_eq!(h.live().mem_bytes(), 0);
        assert!(drain(&mut parts).is_empty());
    }
}
