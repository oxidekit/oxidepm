//! Log reader for tail and follow operations

use oxidepm_core::Result;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use crate::lines::LineSplitter;
use crate::writer::rotated_path;

/// How often followers check the file for new data
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FOLLOW_BATCH_BYTES: usize = 256 * 1024;

/// Highest rotation index looked at when walking history
const MAX_HISTORY_FILES: usize = 100;

/// Which output stream a log line came from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Out,
    Err,
}

impl LogStream {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogStream::Out => "out",
            LogStream::Err => "err",
        }
    }
}

/// A log line tagged with its stream
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: LogStream,
    pub line: String,
}

/// Log reader for tailing and following logs
pub struct LogReader {
    path: PathBuf,
}

impl LogReader {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the last N lines from the log file
    pub fn tail(&self, n: usize) -> Result<Vec<String>> {
        self.tail_matching(n, |_| true)
    }

    /// Read the last N lines for which `keep` returns true
    ///
    /// The filter runs before the cut, so `n` matching lines are returned
    /// even when they are far from the end of the file. Invalid UTF-8 is
    /// decoded lossily instead of failing the read. When the current file
    /// holds fewer than `n` lines, the rotated history (`.1`, `.2`, ... and
    /// their `.gz` forms) is read newest first until `n` lines are found.
    pub fn tail_matching(&self, n: usize, keep: impl Fn(&str) -> bool) -> Result<Vec<String>> {
        self.tail_matching_bounded(n, usize::MAX, keep)
    }

    /// Read the newest lines that fit, charging one byte per line for its newline.
    pub fn tail_bounded(&self, n: usize, max_bytes: usize) -> Result<Vec<String>> {
        self.tail_matching_bounded(n, max_bytes, |_| true)
    }

    fn tail_matching_bounded(
        &self,
        n: usize,
        mut remaining: usize,
        keep: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>> {
        if n == 0 || remaining == 0 {
            return Ok(vec![]);
        }

        let mut chunks = Vec::new();
        let mut have = 0;
        let mut limited = false;
        match File::open(&self.path) {
            Ok(file) => {
                let (lines, used, full) = tail_of(file, n, remaining, &keep)?;
                have += lines.len();
                remaining -= used;
                limited = full;
                chunks.push(lines);
            }
            // Between the rename and the re-create of a rotation.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        for index in 1..=MAX_HISTORY_FILES {
            if have >= n || remaining == 0 || limited {
                break;
            }
            let Some((lines, used, full)) = self.history_tail(index, n - have, remaining, &keep)
            else {
                continue;
            };
            have += lines.len();
            remaining -= used;
            limited = full;
            chunks.push(lines);
        }

        Ok(chunks.into_iter().rev().flatten().collect())
    }

    /// Last `n` matching lines of rotated file `index`, if it exists and is
    /// readable (history is best-effort: a corrupt `.gz` is skipped)
    ///
    /// A plain file wins over a `.gz` of the same index: both exist only
    /// while a rotation is still compressing, and then hold the same data.
    fn history_tail(
        &self,
        index: usize,
        n: usize,
        max_bytes: usize,
        keep: &dyn Fn(&str) -> bool,
    ) -> Option<(Vec<String>, usize, bool)> {
        let plain = rotated_path(&self.path, index, "");
        if let Ok(file) = File::open(&plain) {
            return tail_of(file, n, max_bytes, keep).ok();
        }
        let gz = rotated_path(&self.path, index, ".gz");
        let file = File::open(&gz).ok()?;
        match tail_of(flate2::read::GzDecoder::new(file), n, max_bytes, keep) {
            Ok(lines) => Some(lines),
            Err(e) => {
                debug!("Skipping unreadable {}: {}", gz.display(), e);
                None
            }
        }
    }

    /// Read the last N lines efficiently (seeking from end)
    pub fn tail_efficient(&self, n: usize) -> Result<Vec<String>> {
        if !self.path.exists() {
            return Ok(vec![]);
        }

        let mut file = File::open(&self.path)?;
        let file_size = file.metadata()?.len();

        if file_size == 0 {
            return Ok(vec![]);
        }

        // Start reading from end, chunk by chunk
        let chunk_size = 8192u64;
        let mut lines = Vec::new();
        let mut position = file_size;
        let mut partial_line = String::new();

        while position > 0 && lines.len() < n {
            let read_size = std::cmp::min(chunk_size, position);
            position -= read_size;

            file.seek(SeekFrom::Start(position))?;

            let mut buffer = vec![0u8; read_size as usize];
            file.read_exact(&mut buffer)?;

            // Convert to string and split by newlines
            let chunk = String::from_utf8_lossy(&buffer);
            let chunk_with_partial = format!("{}{}", chunk, partial_line);

            let mut chunk_lines: Vec<&str> = chunk_with_partial.lines().collect();

            // The first element might be partial (continuing from previous chunk)
            if position > 0 && !chunk_lines.is_empty() {
                partial_line = chunk_lines.remove(0).to_string();
            } else {
                partial_line.clear();
            }

            // Add lines in reverse (we're reading backwards)
            for line in chunk_lines.into_iter().rev() {
                if lines.len() >= n {
                    break;
                }
                lines.push(line.to_string());
            }
        }

        // Add any remaining partial line
        if !partial_line.is_empty() && lines.len() < n {
            lines.push(partial_line);
        }

        // Reverse to get correct order
        lines.reverse();

        Ok(lines)
    }

    /// Follow the log file (like tail -f)
    /// Returns a receiver that yields new lines as they're written.
    /// Survives rotation (rename) and truncation by reopening the file.
    pub fn follow(&self) -> Result<mpsc::Receiver<String>> {
        let follower = LogFollower::new(self.path.clone());
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(follow_loop(follower, tx));

        Ok(rx)
    }

    /// Get file size
    pub fn size(&self) -> Result<u64> {
        if !self.path.exists() {
            return Ok(0);
        }
        Ok(std::fs::metadata(&self.path)?.len())
    }
}

/// Last `n` lines of `source` for which `keep` returns true
fn tail_of(
    mut source: impl Read,
    n: usize,
    max_bytes: usize,
    keep: &dyn Fn(&str) -> bool,
) -> std::io::Result<(Vec<String>, usize, bool)> {
    let mut lines: VecDeque<String> = VecDeque::with_capacity(n.min(1024) + 1);
    let mut used = 0;
    let mut limited = false;
    let mut emit = |line: String| {
        if keep(&line) {
            used += line.len() + 1;
            lines.push_back(line);
            if lines.len() > n {
                used -= lines.pop_front().unwrap().len() + 1;
            }
            while used > max_bytes {
                used -= lines.pop_front().unwrap().len() + 1;
                limited = true;
            }
        }
    };
    let mut splitter = LineSplitter::new();
    let mut chunk = [0u8; 8192];
    loop {
        match source.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => splitter.push(&chunk[..n], &mut emit),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    splitter.finish(&mut |mut line| {
        if line.ends_with('\r') {
            line.pop();
        }
        emit(line);
    });
    Ok((lines.into_iter().collect(), used, limited))
}

/// Merge the last `n` lines of several streams, ordered by timestamp
///
/// The filter is applied before the cut. Lines are ordered by the
/// `[YYYY-MM-DD HH:MM:SS.mmm]` prefix written by `LogWriter` (millisecond
/// resolution; lines of older files have whole seconds and sort before
/// lines of the same second that have milliseconds). A line without a
/// prefix (for example the rest of a split long line) takes the key of the
/// line before it in its own stream, so a stream is never reordered. Equal
/// keys keep the order of `streams`.
pub fn tail_streams(
    streams: &[(LogStream, PathBuf)],
    n: usize,
    keep: Option<&dyn Fn(&str) -> bool>,
) -> Result<Vec<LogLine>> {
    let mut all: Vec<(String, LogLine)> = Vec::new();
    for (stream, path) in streams {
        let reader = LogReader::new(path.clone());
        let lines = match keep {
            Some(f) => reader.tail_matching(n, f)?,
            None => reader.tail(n)?,
        };
        let mut key = String::new();
        for line in lines {
            if let Some(k) = timestamp_key(&line) {
                key = k.to_string();
            }
            all.push((
                key.clone(),
                LogLine {
                    stream: *stream,
                    line,
                },
            ));
        }
    }

    if streams.len() > 1 {
        // Stable sort: ties keep stream order and in-stream order
        all.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut all: Vec<LogLine> = all.into_iter().map(|(_, l)| l).collect();
    if all.len() > n {
        all.drain(..all.len() - n);
    }
    Ok(all)
}

/// The timestamp inside the `[...]` prefix of a log line, if it has one
fn timestamp_key(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let end = bytes.iter().take(40).position(|&b| b == b']')?;
    let inner = line.get(1..end)?;
    let b = inner.as_bytes();
    let shaped = b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':';
    shaped.then_some(inner)
}

#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn file_id(_meta: &fs::Metadata) -> u64 {
    0
}

/// Polling tail of one log file that survives rotation and truncation
///
/// A replaced file (different inode, e.g. renamed by rotation) is drained to
/// the end and then reopened from the start; a file that shrank (flush) is
/// re-read from the start. If the file does not exist yet, reading starts at
/// its beginning once it appears.
pub struct LogFollower {
    path: PathBuf,
    file: Option<File>,
    id: u64,
    pos: u64,
    splitter: LineSplitter,
}

impl LogFollower {
    /// Start following at the current end of the file
    pub fn new(path: PathBuf) -> Self {
        let mut follower = Self {
            path,
            file: None,
            id: 0,
            pos: 0,
            splitter: LineSplitter::new(),
        };
        if let Ok(file) = File::open(&follower.path) {
            if let Ok(meta) = file.metadata() {
                follower.id = file_id(&meta);
                follower.pos = meta.len();
                follower.file = Some(file);
            }
        }
        follower
    }

    /// Return the next batch of complete lines written since the last call.
    pub fn poll(&mut self) -> Result<Vec<String>> {
        self.poll_batch().map(|(lines, _)| lines)
    }

    fn poll_batch(&mut self) -> Result<(Vec<String>, bool)> {
        let mut out = Vec::new();
        let mut remaining = FOLLOW_BATCH_BYTES;

        match fs::metadata(&self.path) {
            Ok(meta) => {
                let id = file_id(&meta);
                if self.file.is_none() {
                    self.open_from_start()?;
                } else if id != self.id {
                    // Rotated: read what is left in the old file first
                    self.drain(&mut out, &mut remaining)?;
                    if remaining == 0 {
                        return Ok((out, true));
                    }
                    self.splitter.finish(&mut |l| out.push(l));
                    self.open_from_start()?;
                } else if meta.len() < self.pos {
                    // Truncated in place
                    self.pos = 0;
                    self.splitter.clear();
                }
            }
            // Between rename and re-create: keep draining the old file
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        self.drain(&mut out, &mut remaining)?;
        Ok((out, remaining == 0))
    }

    fn open_from_start(&mut self) -> Result<()> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.file = None;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        self.id = file_id(&file.metadata()?);
        self.pos = 0;
        self.file = Some(file);
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<String>, remaining: &mut usize) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(self.pos))?;
        let mut chunk = vec![0u8; 64 * 1024];
        while *remaining > 0 {
            let limit = chunk.len().min(*remaining);
            let n = file.read(&mut chunk[..limit])?;
            if n == 0 {
                break;
            }
            *remaining -= n;
            self.pos += n as u64;
            self.splitter.push(&chunk[..n], &mut |l| out.push(l));
        }
        Ok(())
    }
}

/// Drive a follower and forward its lines until the receiver is dropped
async fn follow_loop(follower: LogFollower, tx: mpsc::Sender<String>) {
    let mut follower = Some(follower);
    while !tx.is_closed() {
        let mut f = follower.take().expect("follower present");
        let polled = tokio::task::spawn_blocking(move || {
            let res = f.poll_batch();
            (f, res)
        })
        .await;
        let (f, res) = match polled {
            Ok(v) => v,
            Err(e) => {
                debug!("Follow ended: {}", e);
                return;
            }
        };
        follower = Some(f);

        match res {
            Ok((lines, has_more)) => {
                for line in lines {
                    if tx.send(line).await.is_err() {
                        return;
                    }
                }
                if has_more {
                    continue;
                }
            }
            Err(e) => debug!("Follow poll failed: {}", e),
        }
        tokio::time::sleep(FOLLOW_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn tail_splits_oversized_lines() {
        let limit = crate::lines::MAX_LINE_BYTES;
        let input = format!("{}rest\n", "x".repeat(limit));
        let (result, _, _) = tail_of(input.as_bytes(), 2, usize::MAX, &|_| true).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), limit);
        assert_eq!(result[1], "rest");
    }

    #[test]
    fn follower_drains_a_rotated_file_over_multiple_polls() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.log");
        File::create(&path).unwrap();
        let mut follower = LogFollower::new(path.clone());
        let data = "old\n".repeat(128 * 1024);
        fs::write(&path, &data).unwrap();
        fs::rename(&path, dir.path().join("out.log.1")).unwrap();
        fs::write(&path, "new\n").unwrap();

        let first = follower.poll().unwrap();
        assert!(!first.is_empty());
        assert!(first.len() < 128 * 1024);
        let mut all = first;
        while all.last().is_some_and(|line| line == "old") {
            let next = follower.poll().unwrap();
            assert!(
                !next.is_empty(),
                "the rest of the rotated log should still be readable"
            );
            all.extend(next);
        }
        assert_eq!(all.len(), 128 * 1024 + 1);
        assert_eq!(all.last().unwrap(), "new");
    }

    #[test]
    fn test_tail_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.log");
        File::create(&path).unwrap();

        let reader = LogReader::new(path);
        let lines = reader.tail(10).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn test_tail_nonexistent_file() {
        let reader = LogReader::new(PathBuf::from("/nonexistent/file.log"));
        let lines = reader.tail(10).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn test_tail_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        {
            let mut file = File::create(&path).unwrap();
            for i in 1..=20 {
                writeln!(file, "Line {}", i).unwrap();
            }
        }

        let reader = LogReader::new(path);
        let lines = reader.tail(5).unwrap();

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "Line 16");
        assert_eq!(lines[4], "Line 20");
    }

    #[test]
    fn test_tail_efficient() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        {
            let mut file = File::create(&path).unwrap();
            for i in 1..=100 {
                writeln!(file, "Line {} with some longer content here", i).unwrap();
            }
        }

        let reader = LogReader::new(path);
        let lines = reader.tail_efficient(10).unwrap();

        assert_eq!(lines.len(), 10);
        assert!(lines[0].contains("91"));
        assert!(lines[9].contains("100"));
    }

    #[test]
    fn test_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"Hello, world!\n").unwrap();
        }

        let reader = LogReader::new(path);
        assert_eq!(reader.size().unwrap(), 14);
    }
}
