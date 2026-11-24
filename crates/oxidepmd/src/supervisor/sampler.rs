//! The metrics sampler: one task for the whole daemon that measures the leader process of every
//! live app and asks for a restart of one over its memory limit.
//!
//! It reads only the processes it tracks, never the whole table (`refresh_all` scanned every
//! process on the host every two seconds), and never on an async worker: the reading runs in
//! `spawn_blocking`. A pid is trusted only while the start time recorded at launch still matches
//! `/proc/<pid>/stat`, so a recycled pid is never measured. The actor clears
//! `Live::proc` right after it reaps the child, and the sampler looks again before it stores a
//! reading, so a dead child is never measured either.
//!
//! The sampler never signals anything. A limit is a `Command::LimitExceeded` post to the app's
//! mailbox (carrying the generation it measured); the actor decides, and ignores a post that
//! names a child it no longer runs.
//!
//! Known limit: only the leader is measured. An app started as `npm start` is measured by the
//! `npm` wrapper, not by the `node` child under it. Summing the descendants is a follow-up.

use super::actor::AbortOnDrop;
use super::handle::{Command, HandleKind, LimitKind, LiveProc, ProcessHandle};
#[cfg(test)]
use super::pids::parse_start_ticks;
pub(super) use super::pids::read_start_ticks as start_ticks;
use super::registry::Registry;
use oxidepm_core::constants::DEFAULT_METRICS_INTERVAL_SECS;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, Once, Weak};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, System};
use tracing::{debug, warn};

/// One measurement of a leader process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub cpu_percent: f32,
    pub mem_bytes: u64,
}

/// Where the numbers come from (tests replace it).
pub trait Probe: Send + 'static {
    /// The start time of `pid` in clock ticks since boot, `None` if there is no such process.
    fn start_ticks(&self, pid: u32) -> Option<u64>;

    /// Measure these processes (all of them validated just before). A process the probe cannot
    /// read is left out. Anything the probe kept for a pid that is not listed is dropped.
    fn measure(&mut self, pids: &[u32]) -> HashMap<u32, Reading>;
}

/// What the whole process tree of an adapter uses. Unlike an app, which is
/// measured by its leader, an adapter is measured over its process group and over everything
/// that descends from its leader, so that a helper it starts counts, in or out of the group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupUsage {
    pub processes: u32,
    pub threads: u64,
    pub rss_bytes: u64,
}

/// The fields of `/proc/<pid>/stat` that the group sum needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatLine {
    pub ppid: i32,
    pub pgrp: i32,
    pub threads: u64,
    pub rss_pages: u64,
}

/// Parse one `/proc/<pid>/stat` line.
pub fn parse_stat(stat: &str) -> Option<StatLine> {
    // the command name (field 2) may hold spaces and parentheses: count from the last ')'
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    // `rest` starts at field 3 (state): ppid is field 4, pgrp field 5, num_threads field 20, rss field 24
    let ppid = fields.nth(1)?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let threads = fields.nth(14)?.parse().ok()?;
    let rss_pages: i64 = fields.nth(3)?.parse().ok()?;
    Some(StatLine {
        ppid,
        pgrp,
        threads,
        rss_pages: rss_pages.max(0) as u64,
    })
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
        .filter_map(|pid| {
            Some((
                pid,
                parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?,
            ))
        })
        .collect();
    sum_group(&table, leader as i32, pgid, page_size)
}

/// The real probe: sysinfo, one `System` per tracked pid. A process's CPU usage is the change
/// since that process was last read, so each one needs a baseline of its own (one shared
/// `System` would divide every reading by the time since the previous pid's).
#[derive(Default)]
pub struct SysProbe {
    systems: HashMap<u32, System>,
}

fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::new().with_cpu().with_memory()
}

impl Probe for SysProbe {
    fn start_ticks(&self, pid: u32) -> Option<u64> {
        start_ticks(pid)
    }

    fn measure(&mut self, pids: &[u32]) -> HashMap<u32, Reading> {
        self.systems.retain(|pid, _| pids.contains(pid));
        let mut readings = HashMap::new();
        for &pid in pids {
            let system = self.systems.entry(pid).or_default();
            let sys_pid = Pid::from(pid as usize);
            if !system.refresh_process_specifics(sys_pid, refresh_kind()) {
                self.systems.remove(&pid);
                continue;
            }
            if let Some(process) = system.process(sys_pid) {
                readings.insert(
                    pid,
                    Reading {
                        cpu_percent: process.cpu_usage(),
                        mem_bytes: process.memory(),
                    },
                );
            }
        }
        readings
    }
}

/// sysinfo keeps a `/proc/<pid>/stat` descriptor open for every process it has read, up to half
/// of RLIMIT_NOFILE. A daemon that ran for hours held thousands. Zero keeps none open.
fn stop_caching_proc_files() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        sysinfo::set_open_files_limit(0);
    });
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
    stop_caching_proc_files();
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

pub struct Sampler<P: Probe> {
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
        let leaders: Vec<(u32, u64)> = tracked
            .iter()
            .map(|(_, p)| (p.pid, p.start_ticks))
            .collect();

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
            if handle.live().proc() != Some(proc) {
                continue;
            }
            handle
                .live()
                .set_metrics(reading.cpu_percent, reading.mem_bytes);
            post_memory_limit(&handle, proc, reading.mem_bytes);
        }
    }
}

/// Validate every leader's start time, measure the ones that match. One entry per leader.
fn sample<P: Probe>(probe: &mut P, leaders: &[(u32, u64)]) -> Vec<Option<Reading>> {
    let valid: Vec<u32> = leaders
        .iter()
        .filter(|(pid, ticks)| {
            let ok = probe.start_ticks(*pid) == Some(*ticks);
            if !ok {
                // a recycled pid, or a child that is already gone
                debug!(
                    "Not measuring pid {}: it is not the process that was launched",
                    pid
                );
            }
            ok
        })
        .map(|(pid, _)| *pid)
        .collect();
    let mut measured = probe.measure(&valid);
    leaders
        .iter()
        .map(|(pid, _)| {
            if valid.contains(pid) {
                measured.remove(pid)
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
        }
    }

    #[test]
    fn a_stat_line_with_spaces_and_parentheses_in_the_name_is_read() {
        let stat = "4242 (my (odd) name) S 100 200 300 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 7 0 12345 1000000 321 18446744073709551615 0 0 0";
        assert_eq!(parse_stat(stat), Some(line(100, 200, 7, 321)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1"), None);
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

        fn measure(&mut self, pids: &[u32]) -> HashMap<u32, Reading> {
            self.measure_calls.fetch_add(1, Ordering::SeqCst);
            self.measured.lock().push(pids.to_vec());
            let readings = self.readings.lock();
            pids.iter()
                .filter_map(|p| readings.get(p).map(|r| (*p, *r)))
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
        assert_eq!(parse_start_ticks(stat), Some(987654));
        assert_eq!(parse_start_ticks("garbage"), None);
        assert_eq!(parse_start_ticks("1 (x) S 1"), None);
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
            fn measure(&mut self, pids: &[u32]) -> HashMap<u32, Reading> {
                if !pids.is_empty() {
                    self.0.live().set_proc(Some(proc(48, 2, 20)));
                }
                pids.iter().map(|p| (*p, reading(80))).collect()
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
