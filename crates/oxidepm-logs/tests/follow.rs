//! Follow and tail behaviour across rotation and truncation (temp dirs only)

use oxidepm_logs::{tail_streams, LogFollower, LogReader, LogStream, LogWriter, RotationConfig};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

async fn next(rx: &mut Receiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for a followed line")
        .expect("follower ended")
}

fn append(path: &Path, text: &str) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

// After rotation, the follower used to stay on the renamed inode
#[tokio::test]
async fn follow_survives_rename_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    append(&path, "one\ntwo\nthree\n");

    let mut rx = LogReader::new(path.clone()).follow().unwrap();
    append(&path, "four\n");
    assert_eq!(next(&mut rx).await, "four");

    fs::rename(&path, dir.path().join("app-out.log.1")).unwrap();
    append(&path, "five\nsix\n");
    assert_eq!(next(&mut rx).await, "five");
    assert_eq!(next(&mut rx).await, "six");
}

// Lines written to the old file just before it was rotated away are not lost
#[tokio::test]
async fn follow_drains_old_file_before_switching() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    append(&path, "one\n");

    let mut f = LogFollower::new(path.clone());
    append(&path, "two\n");
    fs::rename(&path, dir.path().join("app-out.log.1")).unwrap();
    append(&path, "three\n");

    assert_eq!(f.poll().unwrap(), vec!["two", "three"]);
}

async fn follow_through_logwriter_rotation(compress: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    let mut config = RotationConfig::new(100, 3);
    config.compress = compress;
    let mut writer = LogWriter::new(path.clone(), config).unwrap();

    let mut rx = LogReader::new(path.clone()).follow().unwrap();

    // Lines are ~27 bytes each: the 4th write crosses 100 bytes and rotates
    for i in 1..=6 {
        writer.write_line(&format!("msg{}", i)).unwrap();
        let got = next(&mut rx).await;
        assert!(
            got.ends_with(&format!("msg{}", i)),
            "got {:?} for msg{}",
            got,
            i
        );
    }
    writer.wait_for_compression();
    assert!(
        dir.path()
            .join(if compress {
                "app-out.log.1.gz"
            } else {
                "app-out.log.1"
            })
            .exists(),
        "rotation should have happened"
    );
}

// Lines written before and after a rotation within one poll interval are
// all seen, with and without compression (the new file never shares the old
// file's inode, so the switch is always detected)
fn burst_across_rotation(compress: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    let mut config = RotationConfig::new(100, 5);
    config.compress = compress;
    let mut writer = LogWriter::new(path.clone(), config).unwrap();
    let mut follower = LogFollower::new(path.clone());

    // ~45 bytes per line: the 3rd line rotates, lines 4 and 5 go to the new file
    for i in 1..=5 {
        writer
            .write_line(&format!("burst message number {}", i))
            .unwrap();
    }
    let got = follower.poll().unwrap();
    assert_eq!(got.len(), 5, "{:?}", got);
    assert!(got[0].ends_with("number 1") && got[4].ends_with("number 5"));
    writer.wait_for_compression();
    assert!(follower.poll().unwrap().is_empty());
}

#[test]
fn follower_sees_burst_across_rotation() {
    burst_across_rotation(false);
}

#[test]
fn follower_sees_burst_across_compressing_rotation() {
    burst_across_rotation(true);
}

#[tokio::test]
async fn follow_survives_logwriter_rotation() {
    follow_through_logwriter_rotation(false).await;
}

#[tokio::test]
async fn follow_survives_logwriter_rotation_with_compress() {
    follow_through_logwriter_rotation(true).await;
}

// flush truncates the file in place
#[tokio::test]
async fn follow_survives_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    append(
        &path,
        "a long first line of output\nanother long line of output\n",
    );

    let mut rx = LogReader::new(path.clone()).follow().unwrap();
    append(&path, "before\n");
    assert_eq!(next(&mut rx).await, "before");

    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    append(&path, "after\n");
    assert_eq!(next(&mut rx).await, "after");
}

#[tokio::test]
async fn follow_waits_for_file_to_appear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late-out.log");

    let mut rx = LogReader::new(path.clone()).follow().unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    append(&path, "hello\n");
    assert_eq!(next(&mut rx).await, "hello");
}

#[test]
fn follower_only_returns_complete_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    append(&path, "old\n");

    let mut f = LogFollower::new(path.clone());
    append(&path, "par");
    assert!(f.poll().unwrap().is_empty());
    append(&path, "tial\nnext\n");
    assert_eq!(f.poll().unwrap(), vec!["partial", "next"]);
}

#[test]
fn tail_handles_non_utf8_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    fs::write(&path, b"a\n\xff\xfe\nb\n").unwrap();

    let lines = LogReader::new(path).tail(10).unwrap();
    assert_eq!(lines, vec!["a", "\u{fffd}\u{fffd}", "b"]);
}

// grep must apply before the --lines cut
#[test]
fn tail_matching_filters_before_cutting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    let mut content = String::new();
    for i in 0..50 {
        content.push_str(&format!("noise {}\n", i));
        if i % 10 == 0 {
            content.push_str(&format!("ERROR {}\n", i));
        }
    }
    fs::write(&path, content).unwrap();

    let reader = LogReader::new(path);
    // The last 3 lines hold no match, but 3 matches exist earlier in the file
    assert!(reader.tail(3).unwrap().iter().all(|l| !l.contains("ERROR")));
    let hits = reader.tail_matching(3, |l| l.contains("ERROR")).unwrap();
    assert_eq!(hits, vec!["ERROR 20", "ERROR 30", "ERROR 40"]);
}

// both streams, merged by timestamp, grep before the cut
#[test]
fn tail_streams_merges_by_timestamp_and_filters_first() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("app-out.log");
    let err = dir.path().join("app-err.log");
    fs::write(
        &out,
        "[2026-01-01 10:00:01] out-1\n[2026-01-01 10:00:03] out-3\n[2026-01-01 10:00:05] out-5\n",
    )
    .unwrap();
    fs::write(
        &err,
        "[2026-01-01 10:00:02] err-2\n[2026-01-01 10:00:04] err-4 boom\n",
    )
    .unwrap();
    let streams = [(LogStream::Out, out), (LogStream::Err, err)];

    let all = tail_streams(&streams, 4, None).unwrap();
    let text: Vec<&str> = all.iter().map(|l| &l.line[22..]).collect();
    assert_eq!(text, vec!["err-2", "out-3", "err-4 boom", "out-5"]);
    assert_eq!(all[0].stream, LogStream::Err);
    assert_eq!(all[1].stream, LogStream::Out);

    let boom = |l: &str| l.contains("boom");
    let hits = tail_streams(&streams, 1, Some(&boom)).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].stream, LogStream::Err);
}

// history used to vanish at rotation (tail read only the current file)
#[test]
fn tail_walks_rotated_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    let mut config = RotationConfig::new(100, 10);
    config.compress = true;
    let mut writer = LogWriter::new(path.clone(), config).unwrap();
    for i in 1..=10 {
        writer
            .write_line(&format!("history message {:02}", i))
            .unwrap();
        // let each compression finish so .1 is a .gz like in a long-lived daemon
        writer.wait_for_compression();
    }
    // Leave a plain file in the middle of the chain too
    fs::write(
        dir.path().join("app-out.log.9"),
        "[2020-01-01 00:00:00.000] ancient\n",
    )
    .unwrap();

    let reader = LogReader::new(path);
    let current = reader.size().unwrap();
    assert!(current < 100 + 50);

    let all = reader.tail(1000).unwrap();
    let bodies: Vec<&str> = all
        .iter()
        .map(|l| l.get(26..).unwrap_or(l.as_str()))
        .collect();
    assert_eq!(
        bodies.first(),
        Some(&"ancient"),
        "oldest history first: {:?}",
        bodies
    );
    assert_eq!(bodies.last(), Some(&"history message 10"));
    for i in 1..=10 {
        let want = format!("history message {:02}", i);
        assert!(
            bodies.contains(&want.as_str()),
            "missing {} in {:?}",
            want,
            bodies
        );
    }

    // The cut keeps the newest lines, crossing file boundaries
    let last4 = reader.tail(4).unwrap();
    assert_eq!(last4.len(), 4);
    assert!(last4[3].ends_with("history message 10"));
    assert!(last4[0].ends_with("history message 07"));

    // grep before the cut also reaches into history
    let hits = reader
        .tail_matching(2, |l| l.contains("message 02") || l.contains("message 03"))
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits[0].ends_with("message 02"));
}

#[test]
fn tail_reads_history_while_current_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    fs::write(dir.path().join("app-out.log.1"), "a\nb\n").unwrap();
    assert_eq!(LogReader::new(path).tail(5).unwrap(), vec!["a", "b"]);
}

#[test]
fn tail_skips_a_corrupt_gz_in_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-out.log");
    fs::write(&path, "now\n").unwrap();
    fs::write(dir.path().join("app-out.log.1.gz"), b"not gzip at all").unwrap();
    fs::write(dir.path().join("app-out.log.2"), "older\n").unwrap();
    assert_eq!(LogReader::new(path).tail(5).unwrap(), vec!["older", "now"]);
}

// Merge order: millisecond keys interleave streams within one second
#[test]
fn tail_streams_orders_by_milliseconds() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("app-out.log");
    let err = dir.path().join("app-err.log");
    fs::write(
        &out,
        "[2026-01-01 10:00:01.100] out-a\n[2026-01-01 10:00:01.500] out-b\n",
    )
    .unwrap();
    fs::write(
        &err,
        "[2026-01-01 10:00:01.300] err-a\n[2026-01-01 10:00:01.700] err-b\n",
    )
    .unwrap();
    let streams = [(LogStream::Out, out), (LogStream::Err, err)];
    let all = tail_streams(&streams, 10, None).unwrap();
    let order: Vec<&str> = all.iter().map(|l| &l.line[26..]).collect();
    assert_eq!(order, vec!["out-a", "err-a", "out-b", "err-b"]);
}

// A line without a prefix stays behind the line it continues
#[test]
fn tail_streams_keeps_unprefixed_lines_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("app-out.log");
    let err = dir.path().join("app-err.log");
    fs::write(
        &out,
        "[2026-01-01 10:00:01.000] first\ncontinuation\n[2026-01-01 10:00:09.000] last\n",
    )
    .unwrap();
    fs::write(&err, "[2026-01-01 10:00:05.000] middle\n").unwrap();
    let streams = [(LogStream::Out, out), (LogStream::Err, err)];
    let all = tail_streams(&streams, 10, None).unwrap();
    let lines: Vec<&str> = all.iter().map(|l| l.line.as_str()).collect();
    assert!(lines[0].ends_with("first"));
    assert_eq!(lines[1], "continuation");
    assert!(lines[2].ends_with("middle"));
    assert!(lines[3].ends_with("last"));
}

// Rotation never frees the old file's inode before the new file exists, so
// the follower cannot mistake a rotation for a truncation (inode reuse)
#[cfg(unix)]
#[test]
fn rotation_gives_the_new_file_a_new_inode() {
    use std::os::unix::fs::MetadataExt;
    for compress in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-out.log");
        let mut config = RotationConfig::new(100, 3);
        config.compress = compress;
        let mut writer = LogWriter::new(path.clone(), config).unwrap();
        let before = fs::metadata(&path).unwrap().ino();
        for i in 0..3 {
            writer
                .write_line(&format!("inode check line {}", i))
                .unwrap();
        }
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            before,
            "compress={}",
            compress
        );
        writer.wait_for_compression();
    }
}
