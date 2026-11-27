//! The metrics sampler, seen from outside the daemon process.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::daemon::DaemonProc;
use oxidepm_core::{AppSpec, Runtime};
use serde_json::json;
use tempfile::TempDir;

/// The descriptors of `pid` that point into `/proc`.
fn proc_fds(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fd dir")
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target.starts_with("/proc"))
        .collect()
}

/// Poll until `pid` holds no `/proc` descriptors or `timeout` elapses.
fn wait_for_no_proc_fds(pid: u32, timeout: Duration) -> Vec<PathBuf> {
    let deadline = Instant::now() + timeout;
    let mut open = proc_fds(pid);
    while !open.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        open = proc_fds(pid);
    }
    open
}

/// The sampler reads only the pids of the apps it tracks and keeps no `/proc` descriptors open
/// after a sampling pass completes.
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
    let proc_early = wait_for_no_proc_fds(daemon.pid(), Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(1500));
    let proc_late = wait_for_no_proc_fds(daemon.pid(), Duration::from_secs(1));
    let _ = daemon.rpc(json!({ "type": "kill" }));

    assert!(
        proc_early.is_empty(),
        "descriptors into /proc stayed open after early ticks: {proc_early:?}"
    );
    assert!(
        proc_late.is_empty(),
        "descriptors into /proc stayed open after later ticks: {proc_late:?}"
    );
}
