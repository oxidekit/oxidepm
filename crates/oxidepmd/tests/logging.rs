//! Characterisation of log capture.

mod common;

use std::time::Duration;

use common::*;
use oxidepm_core::{AppStatus, Selector};
use oxidepmd::handlers::LOG_SCRUB_MIN;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_and_stderr_are_captured_to_files() {
    let h = Harness::new().await;
    let m = h.marker();
    let sh = format!("echo out-{m}; echo err-{m} >&2; exec sleep {m}");
    h.start(h.cmd("log-basic", &sh)).await.expect("start");

    let out = h.wait_log("log-basic", "out", &format!("out-{m}")).await;
    let err = h.wait_log("log-basic", "err", &format!("err-{m}")).await;

    for (log, marker) in [(out, format!("out-{m}")), (err, format!("err-{m}"))] {
        let line = log
            .lines()
            .find(|l| l.contains(&marker))
            .expect("marker line");
        assert!(
            line.starts_with("[20"),
            "line should carry a timestamp: {line:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limited_logs_scrub_multiline_environment_values() {
    let h = Harness::new().await;
    let spec = h.cmd_with(
        "log-labels",
        &format!("printf '%s\\n' \"$LABELS\"; exec sleep {}", h.marker()),
        json!({"env": {"LABELS": "first-label\r\non\nsecond-label"}}),
    );
    h.start(spec).await.expect("start");
    h.wait_log("log-labels", "out", "second-label").await;

    let selector = Selector::parse("log-labels");
    let lines = h
        .sup
        .read_logs(&selector, 3, true, false, Some(LOG_SCRUB_MIN))
        .unwrap();
    assert_eq!(lines.len(), 3);
    for (line, suffix) in lines.iter().zip(["<redacted>", "on", "<redacted>"]) {
        assert!(line.ends_with(&format!("] {suffix}")), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limited_log_tails_scrub_values_across_long_line_splits() {
    let h = Harness::new().await;
    // The reader splits the timestamped line and capture splits the original output.
    for (name, padding, label) in [
        ("log-read-split", 65536 - 26 - 3, "output-label"),
        ("log-write-split", 65536 - 3, "output-label"),
        ("log-unicode-split", 65536 - 4, "outéput-label"),
    ] {
        let spec = h.cmd_with(
            name,
            &format!(
                "printf '%{padding}s%s\\n' '' \"$LABEL\"; exec sleep {}",
                h.marker()
            ),
            json!({"env": {"LABEL": label}}),
        );
        h.start(spec).await.expect("start");
        h.wait_log(name, "out", "put-label").await;

        let selector = Selector::parse(name);
        for wanted in [1, 3] {
            let lines = h
                .sup
                .read_logs(&selector, wanted, true, true, Some(LOG_SCRUB_MIN))
                .unwrap();
            assert!(!lines.is_empty());
            assert!(lines.iter().any(|line| line.contains("<redacted>")));
            assert!(lines
                .iter()
                .all(|line| !line.contains("out") && !line.contains("put-label")));
        }

        let lines = h.sup.logs(&selector, 3, true, false).await.unwrap();
        assert!(lines.iter().any(|line| line.contains("out")));
        assert!(lines.iter().any(|line| line.ends_with("put-label")));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatty_child_does_not_block_on_full_pipe() {
    let h = Harness::new().await;
    let m = h.marker();
    let done = h.file("chatty.done");
    let sh = format!(
        "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 300000; touch '{}'; exec sleep {m}",
        done.display()
    );
    h.start(h.cmd("log-chatty", &sh)).await.expect("start");

    // If nothing drained the pipe the child would block after about 64 KiB and never get here.
    h.wait_file(&done).await;
    assert!(!h.read_log("log-chatty", "out").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_utf8_output_does_not_kill_child() {
    let h = Harness::new().await;
    let m = h.marker();
    // Bug #23 (non-UTF-8 output ended capture and blocked the child) is fixed in the logs crate,
    // so this characterisation runs enabled.
    let sh = format!("printf '\\377\\376\\n'; sleep 1; echo later-{m}; exec sleep {m}");
    h.start(h.cmd("log-binary", &sh)).await.expect("start");

    h.wait_log("log-binary", "out", &format!("later-{m}")).await;

    let state = h.state("log-binary").await;
    assert_eq!(state.status, AppStatus::Running);
    assert!(Harness::pid_alive(state.pid.expect("pid")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_child_output_is_captured_and_not_blocked() {
    let h = Harness::new().await;
    let m = h.marker();
    let launches = h.file("launches");
    let done = h.file("second.done");
    // First run crashes. The second run writes 200 KB (more than a 64 KiB pipe holds), then a
    // marker line, then the done file. A restart that nobody drains blocks inside the 200 KB.
    let sh = format!(
        "echo launch >> '{l}'; \
         if [ \"$(wc -l < '{l}')\" -le 1 ]; then exit 1; fi; \
         yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 200000; \
         echo second-run-{m}; touch '{d}'; exec sleep {m}",
        l = launches.display(),
        d = done.display()
    );
    h.start(h.cmd_with(
        "log-restart",
        &sh,
        json!({ "restart_policy": { "restart_delay_ms": 20, "max_restarts": 3 } }),
    ))
    .await
    .expect("start");

    h.wait_file(&done).await;
    assert_eq!(Harness::line_count(&launches), 2, "exactly one restart");
    let log = h
        .wait_log("log-restart", "out", &format!("second-run-{m}"))
        .await;
    assert!(
        log.len() > 200_000,
        "the restarted run's output is in the log ({} bytes)",
        log.len()
    );
    assert_eq!(h.state("log-restart").await.status, AppStatus::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_fails_cleanly_when_log_dir_is_unwritable() {
    use std::os::unix::fs::PermissionsExt;

    if is_root() {
        skip("root ignores directory permissions");
        return;
    }
    let h = Harness::new().await;
    let m = h.marker();
    let needle = format!("sleep {m}");
    let logs = h.paths.logs_dir();
    std::fs::create_dir_all(&logs).expect("create logs dir");
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500))
        .expect("chmod logs dir");

    let result = h
        .start(h.cmd("log-nowrite", &format!("exec sleep {m}")))
        .await;

    // Restore first so the temp dir can be cleaned up whatever the assertions say.
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700))
        .expect("restore logs dir");
    assert!(
        result.is_err(),
        "start must fail when logs cannot be opened"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        Harness::procs_with_cmdline(&needle).is_empty(),
        "a failed launch must not leave a child behind"
    );
}

/// A flush empties the files through the writers that have them open: what the app writes next
/// starts the file, without padding, and the writers' size count agrees with the files again, so
/// no rotation fires early for bytes that were cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_then_write_leaves_no_nul_padding() {
    let h = Harness::new().await;
    let m = h.marker();
    let (go, a_done, b_done) = (h.file("go"), h.file("a.done"), h.file("b.done"));
    let pad = "x".repeat(60);
    // phase A: 30 lines of about 90 bytes cross the 2 KiB limit once and leave about 640 bytes;
    // phase B: 20 lines, 1800 bytes, are below the limit on their own and above it on top of A's
    let sh = format!(
        "for i in $(seq 1 30); do echo \"A$i {pad}\"; done; touch '{a}'; \
         while [ ! -f '{go}' ]; do sleep 0.05; done; \
         for i in $(seq 1 20); do echo \"B$i {pad}\"; done; touch '{b}'; exec sleep {m}",
        a = a_done.display(),
        go = go.display(),
        b = b_done.display(),
    );
    h.start(h.cmd_with("flush-nul", &sh, json!({ "log_max_size": 2048 })))
        .await
        .expect("start");
    h.wait_file(&a_done).await;
    let out = h.paths.log_path("flush-nul", "out");
    h.wait_log("flush-nul", "out", "A30 ").await;
    let rotated = |n: u32| {
        let mut name = out.as_os_str().to_owned();
        name.push(format!(".{n}"));
        std::path::PathBuf::from(name)
    };
    assert!(rotated(1).exists(), "phase A rotated once");
    assert!(!rotated(2).exists());

    let results = h.flush("flush-nul").await;
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].1, Ok(true)), "{:?}", results[0].1);
    assert_eq!(
        std::fs::metadata(&out).unwrap().len(),
        0,
        "the file was emptied"
    );

    std::fs::write(&go, "").unwrap();
    h.wait_file(&b_done).await;
    let log = h.wait_log("flush-nul", "out", "B20 ").await;

    let raw = std::fs::read(&out).unwrap();
    assert!(!raw.contains(&0u8), "no NUL padding");
    assert!(
        log.lines().next().unwrap().contains("B1 "),
        "the file starts with the first line written after the flush: {:?}",
        log.lines().next()
    );
    assert!(
        !log.contains("A30 "),
        "nothing from before the flush is left"
    );
    assert_eq!(log.lines().count(), 20);
    assert!(
        !rotated(2).exists(),
        "the size count was reset by the flush: phase B alone is below the limit"
    );
}

/// A file that cannot be emptied is that app's failure: it is an error in its result, and the
/// other app's flush still happens and counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_failure_is_reported_per_app() {
    if is_root() {
        return skip("root can write a read-only file");
    }
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new().await;
    let (stopped_m, running_m) = (h.marker(), h.marker());
    h.start(h.cmd(
        "flush-stuck",
        &format!("echo noise; exec sleep {stopped_m}"),
    ))
    .await
    .expect("start");
    h.start(h.cmd("flush-fine", &format!("echo noise; exec sleep {running_m}")))
        .await
        .expect("start");
    h.wait_log("flush-stuck", "out", "noise").await;
    h.wait_log("flush-fine", "out", "noise").await;
    h.stop("flush-stuck").await.expect("stop"); // no live capture: flushed by path

    let stuck = h.paths.log_path("flush-stuck", "out");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o444)).unwrap();

    let results = h.flush("all").await;

    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o644)).unwrap();
    let find = |name: &str| {
        results
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert!(
        find("flush-stuck").1.is_err(),
        "the read-only file is an error: {:?}",
        find("flush-stuck").1
    );
    assert!(matches!(find("flush-fine").1, Ok(true)));
    assert_eq!(
        results
            .iter()
            .filter(|(_, r)| matches!(r, Ok(true)))
            .count(),
        1,
        "only one counts"
    );
    assert!(
        !h.read_log("flush-stuck", "out").is_empty(),
        "the stuck file kept its content"
    );
    assert!(
        h.read_log("flush-fine", "out").is_empty(),
        "the other app's file was emptied"
    );
}
