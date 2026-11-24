//! The metrics sampler, seen from outside the daemon process.

mod common;

use std::time::Duration;

use common::daemon::DaemonProc;
use oxidepm_core::{AppSpec, Runtime};
use serde_json::json;
use tempfile::TempDir;

/// The descriptors of `pid`: how many there are and how many point into `/proc`.
fn fd_counts(pid: u32) -> (usize, usize) {
    let mut total = 0;
    let mut proc_fds = 0;
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fd dir")
        .flatten()
    {
        total += 1;
        if let Ok(target) = std::fs::read_link(entry.path()) {
            if target.starts_with("/proc") {
                proc_fds += 1;
            }
        }
    }
    (total, proc_fds)
}

/// The sampler reads only the pids of the apps it tracks and keeps no `/proc/<pid>/stat` open
/// (sysinfo caches them, up to half of RLIMIT_NOFILE, when it scans every process): after a
/// few ticks the daemon holds a handful of descriptors into `/proc` at most, and the total does
/// not grow from one tick to the next.
#[test]
fn sampler_does_not_hold_proc_fds() {
    let tmp = TempDir::new().expect("temp dir");
    let home = tmp.path().join("home");
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    // a tick every 100 ms: tick 3 is at 0.3 s, tick 10 at 1 s
    let daemon = DaemonProc::spawn_with_env(&home, &[("OXIDEPM_SAMPLE_INTERVAL_MS", "100")]);

    let mut spec = AppSpec::new(
        "fd-app".to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        workdir,
    );
    spec.args = vec!["301.7".to_string()];
    let started = daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");

    std::thread::sleep(Duration::from_millis(500));
    let (total_early, proc_early) = fd_counts(daemon.pid());
    std::thread::sleep(Duration::from_millis(1500));
    let (total_late, proc_late) = fd_counts(daemon.pid());
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert!(
        proc_early <= 4,
        "{proc_early} descriptors into /proc after tick 3"
    );
    assert!(
        proc_late <= 4,
        "{proc_late} descriptors into /proc after tick 10 and more"
    );
    assert!(
        total_late <= total_early,
        "the descriptor count grew from {total_early} to {total_late}"
    );
}
