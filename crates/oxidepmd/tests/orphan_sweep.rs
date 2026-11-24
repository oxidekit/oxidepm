//! The orphan sweep against real processes: what a SIGKILLed daemon leaves behind
//! is found through `pids.json` and killed, and nothing else is touched.
//!
//! The sweep is driven directly (`sweep_orphans` on a private `pids.json`); the whole path with a
//! real daemon is `orphans_are_swept_after_sigkilled_daemon` in `daemon_proc.rs`.

mod common;

use common::Harness;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use oxidepmd::supervisor::sweep_orphans;
use serde_json::json;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A marker no other process on the machine has in its command line.
fn marker(tag: u32) -> String {
    format!("311.{:05}{:02}", std::process::id() % 100_000, tag)
}

/// `sh -c <script>` as the leader of its own group, like an app's process.
fn spawn_group(script: &str) -> Child {
    Command::new("sh")
        .args(["-c", script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sh")
}

/// Kills what is left with the marker, whatever the test did.
struct Leaks(String);

impl Drop for Leaks {
    fn drop(&mut self) {
        for pid in Harness::procs_with_cmdline(&self.0) {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot id")
        .trim()
        .to_string()
}

/// Field 22 of `/proc/<pid>/stat`.
fn start_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_whitespace().nth(19).unwrap().parse().unwrap()
}

fn write_records(path: &Path, boot: &str, records: serde_json::Value) {
    let file = json!({ "version": 1, "boot_id": boot, "apps": records });
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

fn record_of(child: &Child, name: &str, start_ticks: u64) -> serde_json::Value {
    json!({
        "app_id": 1, "name": name, "pid": child.id(), "pgid": child.id(), "start_ticks": start_ticks
    })
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_alive(pid: u32) -> bool {
    Harness::pid_alive(pid)
}

fn records_in(path: &Path) -> usize {
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    value["apps"].as_array().unwrap().len()
}

/// A record that names a live process with another start time is a recycled pid: the sweep
/// must not touch it.
#[test]
fn recycled_pid_is_not_killed_by_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pids.json");
    let m = marker(1);
    let _leaks = Leaks(format!("sleep {m}"));
    let mut bystander = spawn_group(&format!("exec sleep {m}"));
    wait_for("the bystander's exec", || {
        !Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    });
    // the app that was recorded started long before this process
    let record = record_of(&bystander, "old-app", start_ticks(bystander.id()) + 1000);
    write_records(&path, &boot_id(), json!([record]));

    let report = sweep_orphans(&path);
    assert_eq!(report.recycled, 1, "{report:?}");
    assert!(report.killed.is_empty(), "{report:?}");
    assert!(is_alive(bystander.id()), "the bystander lives on");
    assert_eq!(records_in(&path), 0, "the file is emptied");

    bystander.kill().unwrap();
    bystander.wait().unwrap();
}

/// The same record with the right start time is the app: the sweep kills its group.
#[test]
fn sweep_kills_a_group_whose_leader_is_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pids.json");
    let m = marker(2);
    let _leaks = Leaks(format!("sleep {m}"));
    let mut orphan = spawn_group(&format!("exec sleep {m}"));
    wait_for("the orphan's exec", || {
        !Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    });
    write_records(
        &path,
        &boot_id(),
        json!([record_of(&orphan, "orphan", start_ticks(orphan.id()))]),
    );

    let report = sweep_orphans(&path);
    assert_eq!(report.killed, vec![orphan.id() as i32], "{report:?}");
    orphan.wait().unwrap(); // reaped: it died by the sweep's signal
    assert!(Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty());
    assert_eq!(records_in(&path), 0);
}

/// The leader is gone (a wrapper that died just before the daemon) but a member of its group
/// is not: the sweep matches the group, not the leader.
#[test]
fn sweep_kills_group_members_when_leader_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pids.json");
    let m = marker(3);
    let needle = format!("sleep {m}");
    let _leaks = Leaks(needle.clone());
    // the member is a background sleep; the leader waits for it. The script's own command line
    // holds the marker too, so count the member as the one that is not the leader.
    let mut leader = spawn_group(&format!("sleep {m} & wait"));
    wait_for("the member", || {
        Harness::procs_with_cmdline(&needle).len() == 2
    });
    write_records(
        &path,
        &boot_id(),
        json!([record_of(&leader, "wrapper", start_ticks(leader.id()))]),
    );
    // the leader dies alone; its member stays in the group
    kill(Pid::from_raw(leader.id() as i32), Signal::SIGKILL).unwrap();
    leader.wait().unwrap();
    assert_eq!(
        Harness::procs_with_cmdline(&needle).len(),
        1,
        "the member is still there"
    );

    let report = sweep_orphans(&path);
    assert_eq!(report.killed, vec![leader.id() as i32], "{report:?}");
    wait_for("the member to die", || {
        Harness::procs_with_cmdline(&needle).is_empty()
    });
}

/// Records from another boot name processes that cannot exist any more.
#[test]
fn records_from_another_boot_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pids.json");
    let m = marker(4);
    let _leaks = Leaks(format!("sleep {m}"));
    let mut bystander = spawn_group(&format!("exec sleep {m}"));
    wait_for("the bystander's exec", || {
        !Harness::procs_with_cmdline(&format!("sleep {m}")).is_empty()
    });
    let record = record_of(&bystander, "old", start_ticks(bystander.id()));
    write_records(&path, "some-other-boot", json!([record]));

    let report = sweep_orphans(&path);
    assert_eq!(report.recorded, 1);
    assert!(
        report.killed.is_empty() && report.recycled == 0,
        "{report:?}"
    );
    assert!(is_alive(bystander.id()));
    assert_eq!(records_in(&path), 0);
    bystander.kill().unwrap();
    bystander.wait().unwrap();
}
