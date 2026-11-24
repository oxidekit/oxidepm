//! Capture of child output into log files (temp dirs only, never ~/.oxidepm)

use oxidepm_logs::{LogCapture, LogWriter, RotationConfig};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

fn capture_into(dir: &Path) -> (LogCapture, PathBuf, PathBuf) {
    let out = dir.join("app-out.log");
    let err = dir.join("app-err.log");
    let capture = LogCapture {
        stdout_writer: LogWriter::new(out.clone(), RotationConfig::default()).unwrap(),
        stderr_writer: LogWriter::new(err.clone(), RotationConfig::default()).unwrap(),
    };
    (capture, out, err)
}

/// Run a shell script with captured pipes and wait for both capture tasks
async fn run(script: &str, capture: LogCapture) -> std::process::ExitStatus {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (h_out, h_err) = capture.spawn_capture(child.stdout.take(), child.stderr.take());
    let status = child.wait().await.unwrap();
    if let Some(h) = h_out {
        h.await.unwrap();
    }
    if let Some(h) = h_err {
        h.await.unwrap();
    }
    status
}

/// Log lines with the "[YYYY-MM-DD HH:MM:SS.mmm] " prefix removed
fn bodies(path: &Path) -> Vec<String> {
    let content = std::fs::read(path).unwrap();
    String::from_utf8_lossy(&content)
        .lines()
        .map(|l| l.split_once("] ").map_or("", |x| x.1).to_string())
        .collect()
}

// one non-UTF-8 byte used to end capture, the child then died of SIGPIPE
#[tokio::test]
async fn non_utf8_output_does_not_stop_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (capture, out, _err) = capture_into(dir.path());

    let status = run(r#"printf 'a\n\377\nb\n'; sleep 0.2; echo after"#, capture).await;

    assert_eq!(
        status.code(),
        Some(0),
        "child must not be killed by SIGPIPE"
    );
    assert_eq!(bodies(&out), vec!["a", "\u{fffd}", "b", "after"]);
}

#[tokio::test]
async fn non_utf8_on_stderr_does_not_stop_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (capture, _out, err) = capture_into(dir.path());

    let status = run(
        r#"printf 'a\n\377\nb\n' >&2; sleep 0.2; echo after >&2"#,
        capture,
    )
    .await;

    assert_eq!(status.code(), Some(0));
    assert_eq!(bodies(&err), vec!["a", "\u{fffd}", "b", "after"]);
}

// line length was unbounded
#[tokio::test]
async fn long_lines_are_split_without_losing_data() {
    let dir = tempfile::tempdir().unwrap();
    let (capture, out, _err) = capture_into(dir.path());

    // 200000 bytes with no newline, then a short line
    let status = run(
        "head -c 200000 /dev/zero | tr '\\000' x; echo; echo tail",
        capture,
    )
    .await;

    assert_eq!(status.code(), Some(0));
    let lines = bodies(&out);
    assert_eq!(lines.last().map(String::as_str), Some("tail"));
    let long: Vec<&String> = lines.iter().filter(|l| l.starts_with('x')).collect();
    assert!(
        long.len() >= 4,
        "200000 bytes need at least 4 pieces of 64 KiB"
    );
    assert!(long.iter().all(|l| l.len() <= 64 * 1024));
    assert_eq!(long.iter().map(|l| l.len()).sum::<usize>(), 200_000);
}

// A failing log file must never stop the pipe from being drained
#[cfg(target_os = "linux")]
#[tokio::test]
async fn write_errors_keep_draining_the_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let (mut capture, _out, _err) = capture_into(dir.path());
    // /dev/full fails every write with ENOSPC
    capture.stdout_writer =
        LogWriter::new(PathBuf::from("/dev/full"), RotationConfig::default()).unwrap();

    // ~1 MiB of output, far more than a pipe buffer holds
    let status = run(
        "i=0; while [ $i -lt 20000 ]; do echo 'a line of output that is fairly long ....................'; i=$((i+1)); done; sleep 0.1; echo done",
        capture,
    )
    .await;

    assert_eq!(status.code(), Some(0));
}

/// The owner of a capture can empty the files through the writers: what the child writes
/// afterwards starts the file, nothing is padded, and the size count agrees with the file (so
/// no rotation happens for bytes that are gone).
#[tokio::test]
async fn control_truncate_empties_the_files_behind_a_running_capture() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("app-out.log");
    let err = dir.path().join("app-err.log");
    let config = RotationConfig {
        max_size_bytes: 1500,
        ..RotationConfig::default()
    };
    let capture = LogCapture::with_paths(out.clone(), err.clone(), config).unwrap();

    // 20 lines of about 60 bytes: 1200 bytes, then it waits for `go`
    let go = dir.path().join("go");
    let script = format!(
        "for i in $(seq 1 20); do echo \"before-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; done; \
         echo err-before >&2; while [ ! -f '{}' ]; do sleep 0.05; done; \
         for i in $(seq 1 12); do echo \"after-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; done; echo err-after >&2",
        go.display()
    );
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let tasks = capture.spawn_controlled_capture(child.stdout.take(), child.stderr.take());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while bodies(&out).len() < 20 || bodies(&err).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first lines never arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    tasks.control.truncate().expect("truncate");
    assert_eq!(std::fs::metadata(&out).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&err).unwrap().len(), 0);

    std::fs::write(&go, "").unwrap();
    child.wait().await.unwrap();
    tasks.stdout.unwrap().await.unwrap();
    tasks.stderr.unwrap().await.unwrap();

    let raw = std::fs::read(&out).unwrap();
    assert!(!raw.contains(&0u8), "no NUL padding");
    let lines = bodies(&out);
    assert_eq!(
        lines.len(),
        12,
        "only what came after the truncate: {lines:?}"
    );
    assert!(lines[0].starts_with("after-1-"), "{lines:?}");
    assert_eq!(bodies(&err), vec!["err-after"]);
    // 1200 bytes before + 12 * 80 after would pass the 1500 limit only if the count was stale
    assert!(
        !dir.path().join("app-out.log.1").exists(),
        "the size count was reset: no rotation for bytes that were cut"
    );
}
