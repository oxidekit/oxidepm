//! Log writer with rotation support

use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use oxidepm_core::{OutputStats, Result};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdout};
use tracing::{debug, warn};

use crate::lines::LineSplitter;
use crate::rotation::RotationConfig;

type Rotation = Mutex<Option<JoinHandle<()>>>;

// A replacement writer must wait for compression even if the previous one was dropped.
fn rotation_for(path: &Path) -> io::Result<Arc<Rotation>> {
    static ROTATIONS: OnceLock<Mutex<HashMap<PathBuf, Weak<Rotation>>>> = OnceLock::new();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let key =
        parent.canonicalize()?.join(path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "log path has no filename")
        })?);
    let mut rotations = locked(ROTATIONS.get_or_init(Mutex::default));
    rotations.retain(|_, rotation| rotation.strong_count() != 0);
    if let Some(rotation) = rotations.get(&key).and_then(Weak::upgrade) {
        return Ok(rotation);
    }
    let rotation = Arc::new(Mutex::new(None));
    rotations.insert(key, Arc::downgrade(&rotation));
    Ok(rotation)
}

fn wait_for_compression(compressing: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = compressing.take() {
        let _ = handle.join();
    }
}

/// Log writer that handles rotation
pub struct LogWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    config: RotationConfig,
    current_size: u64,
    /// File size at which the next rotation is attempted
    ///
    /// Normally `max_size_bytes`; pushed out after a failed rotation so a
    /// persistent failure is retried once per `max_size_bytes` of output
    /// instead of on every line.
    rotate_at: u64,
    rotation: Arc<Rotation>,
}

impl LogWriter {
    /// Create a new log writer
    pub fn new(path: PathBuf, config: RotationConfig) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;

        let current_size = file.metadata()?.len();
        let writer = BufWriter::new(file);
        let rotate_at = config.max_size_bytes;

        Ok(Self {
            rotation: rotation_for(&path)?,
            path,
            writer,
            config,
            current_size,
            rotate_at,
        })
    }

    /// Write a line to the log and flush it
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        self.append_line(line)?;
        self.flush()
    }

    /// Write a line to the log without flushing
    ///
    /// Used by capture, which flushes once per batch of lines instead of
    /// once per line. Call `flush` to make the data visible to readers.
    pub fn append_line(&mut self, line: &str) -> Result<()> {
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let formatted = format!("[{}] {}\n", timestamp, line);
        let bytes = formatted.as_bytes();

        self.writer.write_all(bytes)?;
        self.current_size += bytes.len() as u64;

        self.maybe_rotate();
        Ok(())
    }

    /// Flush buffered data to the log file
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    /// Rotate when the file is large enough
    ///
    /// The data was already written, so a failed rotation is logged and
    /// retried only after another `max_size_bytes` of output; it never fails
    /// the write and never repeats on every line.
    fn maybe_rotate(&mut self) {
        if self.current_size < self.rotate_at {
            return;
        }
        match self.rotate() {
            Ok(()) => self.rotate_at = self.config.max_size_bytes,
            Err(e) => {
                warn!("Log rotation failed for {}: {}", self.path.display(), e);
                self.rotate_at = self
                    .current_size
                    .saturating_add(self.config.max_size_bytes.max(1));
            }
        }
    }

    /// Wait until the gzip of the last rotated file has finished
    ///
    /// Rotation compresses in the background; this is for callers (and
    /// tests) that need the final `.gz` on disk.
    pub fn wait_for_compression(&mut self) {
        wait_for_compression(&mut locked(&self.rotation));
    }

    #[cfg(test)]
    fn has_pending_compression(&self) -> bool {
        locked(&self.rotation).is_some()
    }

    /// Rotate the log file
    ///
    /// Both the plain (`.N`) and compressed (`.N.gz`) families are shifted,
    /// so toggling `compress` never orphans history. The current file is
    /// renamed to `.1` (never removed) before the new file is created, and
    /// compression happens afterwards in a background thread via a temp file,
    /// so neither the executor nor a reader ever sees a half-written `.gz`.
    fn rotate(&mut self) -> Result<()> {
        self.rotate_with(|path| OpenOptions::new().create(true).append(true).open(path))
    }

    fn rotate_with(&mut self, reopen: impl FnOnce(&Path) -> io::Result<File>) -> Result<()> {
        debug!("Rotating log file: {}", self.path.display());

        self.writer.flush()?;
        let rotation = Arc::clone(&self.rotation);
        let mut compressing = locked(&rotation);
        // The previous compression must be done before `.1` is shifted.
        wait_for_compression(&mut compressing);

        let max_files = self.config.max_files;

        // Shift oldest first: .4 -> .5, .3 -> .4, ... and drop the oldest.
        // A failure stops here and leaves the current file untouched; the
        // set then only has a gap, never a lost or overwritten file.
        for i in (1..max_files).rev() {
            for ext in ROTATED_EXTS {
                let old_path = rotated_path(&self.path, i, ext);
                if !old_path.exists() {
                    continue;
                }
                if i + 1 >= max_files {
                    fs::remove_file(&old_path)?;
                } else {
                    fs::rename(&old_path, rotated_path(&self.path, i + 1, ext))?;
                }
            }
        }

        // Clear slot 1 even when the retention setting now keeps only the live file.
        for ext in ROTATED_EXTS {
            let first = rotated_path(&self.path, 1, ext);
            if first.exists() {
                fs::remove_file(&first)?;
            }
        }

        let first = rotated_path(&self.path, 1, "");
        let renamed = self.path.exists();
        if renamed {
            fs::rename(&self.path, &first)?;
        }

        // Create the new file while the old one still exists, so the two
        // never share an inode (followers rely on the inode changing)
        let file = match reopen(&self.path) {
            Ok(file) => file,
            Err(e) => {
                if renamed {
                    if let Err(restore) = fs::rename(&first, &self.path) {
                        warn!(
                            "Could not restore {} after rotation failed: {}",
                            self.path.display(),
                            restore
                        );
                    }
                }
                return Err(e.into());
            }
        };
        self.writer = BufWriter::new(file);
        self.current_size = 0;

        if renamed {
            if max_files <= 1 {
                fs::remove_file(first)?;
            } else if self.config.compress {
                *compressing = self.compress_rotated();
            }
        }

        Ok(())
    }

    /// Gzip `.1` into `.1.gz` on a background thread
    fn compress_rotated(&self) -> Option<JoinHandle<()>> {
        let src = rotated_path(&self.path, 1, "");
        let dst = rotated_path(&self.path, 1, ".gz");
        let rotation = Arc::clone(&self.rotation);
        let spawned = std::thread::Builder::new()
            .name("oxidepm-log-gzip".into())
            .spawn(move || {
                let _rotation = rotation;
                if let Err(e) = compress_file(&src, &dst) {
                    // The plain .1 stays in place and is shifted like any other
                    warn!("Failed to compress {}: {}", src.display(), e);
                }
            });
        match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                warn!("Failed to start log compression: {}", e);
                None
            }
        }
    }

    /// Empty the log file through this writer's own handle, and start counting from zero.
    ///
    /// Buffered data goes out first (it belongs before the truncation), then the file is cut.
    /// The file is opened for appending, so what is written next lands at the new end: no
    /// padding, and `current_size` agrees with the file again, so rotation is not triggered
    /// early by bytes that are no longer there.
    pub fn truncate(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().set_len(0)?;
        self.current_size = 0;
        self.rotate_at = self.config.max_size_bytes;
        Ok(())
    }

    /// Get the log file path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get current file size
    pub fn current_size(&self) -> u64 {
        self.current_size
    }
}

/// Extensions of the two families of rotated files
const ROTATED_EXTS: [&str; 2] = ["", ".gz"];

/// Get the path for a rotated log file
pub(crate) fn rotated_path(base: &Path, index: usize, suffix: &str) -> PathBuf {
    let name = base.file_name().unwrap().to_string_lossy();
    base.with_file_name(format!("{}.{}{}", name, index, suffix))
}

/// Compress `src` into `dst` (gzip) and remove `src`
///
/// Writes to `<dst>.tmp` and renames it into place, so `dst` is either
/// absent or complete. On failure the temp file is removed and `src` kept.
fn compress_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut tmp_name = dst.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let result = (|| {
        let mut input = File::open(src)?;
        let mut encoder = GzEncoder::new(File::create(&tmp)?, Compression::default());
        io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.sync_all()?;
        fs::rename(&tmp, dst)?;
        fs::remove_file(src)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        debug!("Compressed {} -> {}", src.display(), dst.display());
    }
    result
}

/// Consecutive read errors retried quickly before backing off to a slow retry
const FAST_READ_RETRIES: u32 = 100;

/// Delay before retrying a failed read
fn read_error_backoff(consecutive_errors: u32) -> Duration {
    if consecutive_errors < FAST_READ_RETRIES {
        Duration::from_millis(10)
    } else {
        Duration::from_secs(1)
    }
}

/// Writes captured lines to a log file; failures are warned about once per
/// streak and never stop the capture
struct CaptureSink<'a> {
    writer: &'a mut LogWriter,
    label: &'a str,
    /// Lives across batches: the streak does not end with the lock being released
    failed: &'a mut bool,
    batch_failed: bool,
}

impl CaptureSink<'_> {
    fn failure(&mut self, e: impl std::fmt::Display) {
        if !*self.failed {
            warn!("Failed to write {}: {}", self.label, e);
        }
        *self.failed = true;
        self.batch_failed = true;
    }

    fn line(&mut self, line: &str) {
        if let Err(e) = self.writer.append_line(line) {
            self.failure(e);
        }
    }

    /// A batch without write or flush failures ends a failure streak.
    fn flush(&mut self) {
        match self.writer.flush() {
            Ok(()) if !self.batch_failed => *self.failed = false,
            Ok(()) => {}
            Err(e) => self.failure(e),
        }
    }
}

/// The lock on a writer (a panic elsewhere must not stop the log from being written).
fn locked<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

struct OutputActivity {
    started: Instant,
    last: Mutex<Option<(DateTime<Utc>, Instant)>>,
}

fn capture_activity() -> &'static Mutex<HashMap<PathBuf, Weak<OutputActivity>>> {
    static CAPTURES: OnceLock<Mutex<HashMap<PathBuf, Weak<OutputActivity>>>> = OnceLock::new();
    CAPTURES.get_or_init(Mutex::default)
}

impl OutputActivity {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last: Mutex::new(None),
        }
    }

    fn record(&self) {
        let mut last = locked(&self.last);
        *last = Some((Utc::now(), Instant::now()));
    }

    fn stats(&self) -> OutputStats {
        let (last_output_at, since) = match *locked(&self.last) {
            Some((utc, instant)) => (Some(utc), instant),
            None => (None, self.started),
        };
        OutputStats {
            last_output_at,
            seconds_since_output: Some(since.elapsed().as_secs()),
        }
    }
}

async fn write_batch(
    writer: &Arc<Mutex<LogWriter>>,
    label: &'static str,
    lines: Vec<String>,
    mut failed: bool,
) -> bool {
    let writer = Arc::clone(writer);
    match tokio::task::spawn_blocking(move || {
        let mut guard = locked(&writer);
        let mut sink = CaptureSink {
            writer: &mut guard,
            label,
            failed: &mut failed,
            batch_failed: false,
        };
        for line in lines {
            sink.line(&line);
        }
        sink.flush();
        failed
    })
    .await
    {
        Ok(failed) => failed,
        Err(e) => {
            warn!("Log writer task for {} failed: {}", label, e);
            true
        }
    }
}

/// Drain a child pipe into a log writer
///
/// Never stops on bad data: bytes are split on newline, long lines are cut
/// at 64 KiB, invalid UTF-8 is decoded lossily, and a failing log file never
/// stops the pipe from being drained (a full pipe would block the child, a
/// closed one would kill it). Read errors are retried until EOF with a
/// growing delay; the pipe is never dropped early.
async fn capture_stream<R: AsyncRead + Unpin>(
    reader: R,
    writer: &Arc<Mutex<LogWriter>>,
    label: &'static str,
    activity: &OutputActivity,
) {
    capture_stream_with(reader, writer, label, activity, read_error_backoff).await;
}

/// The writer is locked for one batch of lines at a time and never across an await, so
/// [`LogControl::truncate`] can get in between two reads.
async fn capture_stream_with<R: AsyncRead + Unpin>(
    mut reader: R,
    writer: &Arc<Mutex<LogWriter>>,
    label: &'static str,
    activity: &OutputActivity,
    backoff: impl Fn(u32) -> Duration,
) {
    let mut chunk = vec![0u8; 8192];
    let mut splitter = LineSplitter::new();
    let mut failed = false;

    let mut read_errors = 0u32;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                activity.record();
                read_errors = 0;
                let mut lines = Vec::new();
                splitter.push(&chunk[..n], &mut |line| lines.push(line));
                if !lines.is_empty() {
                    failed = write_batch(writer, label, lines, failed).await;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                read_errors = read_errors.saturating_add(1);
                if read_errors == 1 {
                    warn!("Failed to read {}: {}", label, e);
                }
                tokio::time::sleep(backoff(read_errors)).await;
            }
        }
    }
    let mut lines = Vec::new();
    splitter.finish(&mut |line| lines.push(line));
    if !lines.is_empty() {
        write_batch(writer, label, lines, failed).await;
    }
}

/// Async log capture from process stdout/stderr
pub struct LogCapture {
    pub stdout_writer: LogWriter,
    pub stderr_writer: LogWriter,
}

impl LogCapture {
    /// Capture into explicit files instead of the default per-app locations.
    pub fn with_paths(stdout: PathBuf, stderr: PathBuf, config: RotationConfig) -> Result<Self> {
        Ok(Self {
            stdout_writer: LogWriter::new(stdout, config.clone())?,
            stderr_writer: LogWriter::new(stderr, config)?,
        })
    }

    /// Activity from both pipes of the current capture opened at this stdout log path.
    pub fn output_stats(stdout: &Path) -> OutputStats {
        let activity = locked(capture_activity())
            .get(stdout)
            .and_then(Weak::upgrade);
        activity.map_or_else(OutputStats::default, |activity| activity.stats())
    }

    /// Spawn tasks to capture stdout and stderr
    pub fn spawn_capture(
        self,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) -> (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        let tasks = self.spawn_controlled_capture(stdout, stderr);
        (tasks.stdout, tasks.stderr)
    }

    /// [`spawn_capture`](Self::spawn_capture), also returning a [`LogControl`] for the same
    /// writers: the owner of the capture can empty the files through them.
    pub fn spawn_controlled_capture(
        self,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) -> CaptureTasks {
        let activity = Arc::new(OutputActivity::new());
        {
            let mut captures = locked(capture_activity());
            captures.retain(|_, activity| activity.strong_count() != 0);
            // Each launch owns a new clock. An older reader can only update its own capture.
            captures.insert(
                self.stdout_writer.path().to_path_buf(),
                Arc::downgrade(&activity),
            );
        }
        let out_writer = Arc::new(Mutex::new(self.stdout_writer));
        let err_writer = Arc::new(Mutex::new(self.stderr_writer));
        let control = LogControl {
            stdout: Arc::clone(&out_writer),
            stderr: Arc::clone(&err_writer),
            _activity: Arc::clone(&activity),
        };

        let stdout_handle = stdout.map(|out| {
            let activity = Arc::clone(&activity);
            tokio::spawn(async move {
                capture_stream(out, &out_writer, "stdout", &activity).await;
            })
        });
        let stderr_handle = stderr.map(|err| {
            tokio::spawn(async move {
                capture_stream(err, &err_writer, "stderr", &activity).await;
            })
        });

        CaptureTasks {
            stdout: stdout_handle,
            stderr: stderr_handle,
            control,
        }
    }
}

/// The tasks of a capture and the handle to its writers.
pub struct CaptureTasks {
    pub stdout: Option<tokio::task::JoinHandle<()>>,
    pub stderr: Option<tokio::task::JoinHandle<()>>,
    pub control: LogControl,
}

/// A handle to the writers of a running capture. Cloning it shares the writers.
#[derive(Clone)]
pub struct LogControl {
    stdout: Arc<Mutex<LogWriter>>,
    stderr: Arc<Mutex<LogWriter>>,
    // A running app can close both pipes and still have output history.
    _activity: Arc<OutputActivity>,
}

impl LogControl {
    /// Empty both log files through the writers' own handles (a truncate, not a rotate).
    ///
    /// Both files are tried; the error names every file that could not be emptied. May wait a
    /// moment for a rotation that is compressing, so call it off the async workers.
    pub fn truncate(&self) -> Result<()> {
        let mut failures = Vec::new();
        for writer in [&self.stdout, &self.stderr] {
            let mut writer = locked(writer);
            if let Err(e) = writer.truncate() {
                failures.push(format!("{}: {}", writer.path().display(), e));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(oxidepm_core::Error::IoError(io::Error::other(
                failures.join("; "),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_log_capture_with_paths_uses_the_given_files() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("a").join("o.log");
        let err = dir.path().join("a").join("e.log");

        let mut capture =
            LogCapture::with_paths(out.clone(), err.clone(), RotationConfig::default()).unwrap();
        capture.stdout_writer.write_line("to-out").unwrap();
        capture.stderr_writer.write_line("to-err").unwrap();

        assert!(fs::read_to_string(&out).unwrap().contains("to-out"));
        assert!(fs::read_to_string(&err).unwrap().contains("to-err"));
    }

    #[test]
    fn test_log_writer_creation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        let writer = LogWriter::new(path.clone(), RotationConfig::default());
        assert!(writer.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn test_log_writer_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        let mut writer = LogWriter::new(path.clone(), RotationConfig::default()).unwrap();
        writer.write_line("Hello, world!").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Hello, world!"));
        assert!(content.contains("[20")); // Timestamp starts with year
    }

    fn gunzip(path: &Path) -> String {
        use std::io::Read;
        let mut out = String::new();
        flate2::read::GzDecoder::new(File::open(path).unwrap())
            .read_to_string(&mut out)
            .unwrap();
        out
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn test_log_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        // Each line is ~50 bytes: every 2nd write crosses 100 bytes
        let config = RotationConfig::new(100, 3);
        let mut writer = LogWriter::new(path.clone(), config).unwrap();
        for i in 0..20 {
            writer
                .write_line(&format!("Line {} with some content", i))
                .unwrap();
        }

        // max_files = 3: current file plus .1 and .2, nothing else
        assert_eq!(
            names(dir.path()),
            vec!["test.log", "test.log.1", "test.log.2"]
        );
        let newest = fs::read_to_string(dir.path().join("test.log.1")).unwrap();
        assert!(newest.contains("Line 18") || newest.contains("Line 17"));
    }

    #[test]
    fn test_flush_is_deferred_for_append_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut writer = LogWriter::new(path.clone(), RotationConfig::default()).unwrap();

        writer.append_line("buffered").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        writer.flush().unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("buffered"));
    }

    // rotation compresses off-thread via a temp file
    #[test]
    fn test_rotation_compresses_atomically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut config = RotationConfig::new(100, 4);
        config.compress = true;
        let mut writer = LogWriter::new(path.clone(), config).unwrap();

        // ~44 bytes per line: the 3rd write crosses 100 bytes
        for i in 0..3 {
            writer
                .write_line(&format!("compressed line {}", i))
                .unwrap();
        }
        writer.wait_for_compression();

        assert_eq!(names(dir.path()), vec!["test.log", "test.log.1.gz"]);
        let text = gunzip(&dir.path().join("test.log.1.gz"));
        assert!(text.contains("compressed line 0"));
        assert!(text.contains("compressed line 2"));
        // No half-written temp file is left behind
        assert!(!names(dir.path()).iter().any(|n| n.ends_with(".tmp")));
    }

    // toggling compress used to orphan the other family
    #[test]
    fn writers_to_the_same_log_share_pending_compression() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut config = RotationConfig::new(100, 3);
        config.compress = true;
        let mut first = LogWriter::new(path.clone(), config.clone()).unwrap();
        let mut next = LogWriter::new(path.clone(), config).unwrap();
        first.write_line(&"x".repeat(120)).unwrap();
        next.wait_for_compression();
        assert!(!first.has_pending_compression());
        assert!(gunzip(&rotated_path(&path, 1, ".gz")).contains(&"x".repeat(120)));
    }

    #[test]
    fn test_toggling_compress_keeps_history_consistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        let run = |compress: bool, tag: &str| {
            let mut config = RotationConfig::new(100, 5);
            config.compress = compress;
            let mut w = LogWriter::new(path.clone(), config).unwrap();
            for i in 0..3 {
                w.write_line(&format!("{} {} padding", tag, i)).unwrap();
            }
            w.wait_for_compression();
        };

        run(false, "plain");
        run(true, "gz");
        run(false, "plain2");

        // Every rotation produced a file and none was lost or overwritten:
        // .1 (newest, plain), .2.gz, .3 (oldest, plain) - one slot each
        let found = names(dir.path());
        assert_eq!(
            found,
            vec!["test.log", "test.log.1", "test.log.2.gz", "test.log.3"],
            "{:?}",
            found
        );
        assert!(fs::read_to_string(dir.path().join("test.log.1"))
            .unwrap()
            .contains("plain2"));
        assert!(gunzip(&dir.path().join("test.log.2.gz")).contains("gz 0"));
        assert!(fs::read_to_string(dir.path().join("test.log.3"))
            .unwrap()
            .contains("plain 0"));
    }

    // a failing rotation must not repeat on every line (each retry
    // used to delete one more file of history)
    #[test]
    fn test_failed_rotation_backs_off() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let blocker = dir.path().join("test.log.2");

        // max_files = 3: rotation first removes .2, which is a non-empty
        // directory here, so it fails before anything else is touched
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("keep"), "x").unwrap();
        fs::write(dir.path().join("test.log.1"), "history\n").unwrap();

        let mut writer = LogWriter::new(path.clone(), RotationConfig::new(100, 3)).unwrap();
        for i in 0..3 {
            writer.write_line(&format!("fill the file {}", i)).unwrap();
        }
        assert!(
            writer.current_size() >= 100,
            "failed rotation keeps the file"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("test.log.1")).unwrap(),
            "history\n",
            "history is untouched after a failed rotation"
        );

        // Unblock: the very next lines must not rotate (backoff), only after
        // another max_size_bytes of output
        fs::remove_dir_all(&blocker).unwrap();
        let before = writer.current_size();
        writer.write_line("one more").unwrap();
        assert!(
            writer.current_size() > before,
            "no rotation right after a failure"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("test.log.1")).unwrap(),
            "history\n"
        );

        for i in 0..4 {
            writer.write_line(&format!("fill again {}", i)).unwrap();
        }
        assert!(
            writer.current_size() < before,
            "rotation retried after backoff"
        );
        assert!(fs::read_to_string(dir.path().join("test.log.2"))
            .unwrap()
            .contains("history"));
    }

    #[test]
    fn failed_reopen_restores_the_live_log() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut writer = LogWriter::new(path.clone(), RotationConfig::default()).unwrap();
        writer.write_line("before rotation").unwrap();
        assert!(writer
            .rotate_with(|_| Err(io::Error::other("reopen failed")))
            .is_err());
        writer.write_line("after rotation").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("before rotation"));
        assert!(text.contains("after rotation"));
    }

    #[test]
    fn one_file_retention_keeps_only_the_live_log() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let mut config = RotationConfig::new(100, 1);
        config.compress = true;
        let mut writer = LogWriter::new(path.clone(), config).unwrap();
        writer.write_line(&"x".repeat(120)).unwrap();
        writer.wait_for_compression();

        assert_eq!(names(dir.path()), ["test.log"]);
        assert_eq!(writer.current_size(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_successful_flush_does_not_hide_an_unbuffered_write_failure() {
        let mut writer =
            LogWriter::new(PathBuf::from("/dev/full"), RotationConfig::default()).unwrap();
        let mut failed = false;
        let mut sink = CaptureSink {
            writer: &mut writer,
            label: "stdout",
            failed: &mut failed,
            batch_failed: false,
        };
        sink.line(&"x".repeat(16 * 1024));
        assert!(*sink.failed);
        sink.flush();
        assert!(*sink.failed);
    }

    #[test]
    fn test_truncate_empties_the_file_and_restarts_the_size_count() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let config = RotationConfig {
            max_size_bytes: 400,
            ..RotationConfig::default()
        };
        let mut writer = LogWriter::new(path.clone(), config).unwrap();
        for i in 0..4 {
            writer
                .write_line(&format!("before {} {}", i, "x".repeat(40)))
                .unwrap();
        }
        assert!(writer.current_size() > 200);

        writer.truncate().unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(writer.current_size(), 0);
        // buffered, unflushed data does not survive the cut either
        writer.append_line("kept?").unwrap();
        writer.truncate().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);

        // what follows starts the file, unpadded, and does not rotate before `max_size_bytes`
        for i in 0..3 {
            writer
                .write_line(&format!("after {} {}", i, "y".repeat(40)))
                .unwrap();
        }
        let raw = fs::read(&path).unwrap();
        assert!(!raw.contains(&0u8), "no NUL padding");
        let text = String::from_utf8(raw).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().next().unwrap().contains("after 0"));
        assert!(!dir.path().join("test.log.1").exists());
    }

    /// Reader that fails `errors` times, then yields one line and EOF
    struct FlakyReader {
        errors: u32,
        sent: bool,
    }

    impl AsyncRead for FlakyReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.errors > 0 {
                self.errors -= 1;
                return std::task::Poll::Ready(Err(io::Error::other("boom")));
            }
            if !self.sent {
                self.sent = true;
                buf.put_slice(b"still here\n");
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    // after the fast-retry cap the pipe used to be dropped
    #[tokio::test]
    async fn test_capture_survives_persistent_read_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let writer = Arc::new(Mutex::new(
            LogWriter::new(path.clone(), RotationConfig::default()).unwrap(),
        ));

        let errors = FAST_READ_RETRIES * 2 + 5;
        capture_stream_with(
            FlakyReader {
                errors,
                sent: false,
            },
            &writer,
            "stdout",
            &OutputActivity::new(),
            |_| Duration::ZERO,
        )
        .await;

        assert!(fs::read_to_string(&path).unwrap().contains("still here"));
    }

    #[tokio::test]
    async fn waiting_for_a_log_writer_keeps_the_async_worker_available() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");
        let writer = Arc::new(Mutex::new(
            LogWriter::new(path, RotationConfig::default()).unwrap(),
        ));
        let locked_writer = Arc::clone(&writer);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = locked(&locked_writer);
            ready_tx.send(()).unwrap();
            progress_rx.recv_timeout(Duration::from_secs(2)).is_ok()
        });
        ready_rx.recv().unwrap();

        let activity = OutputActivity::new();
        tokio::join!(
            biased;
            capture_stream(&b"line\n"[..], &writer, "stdout", &activity),
            async { let _ = progress_tx.send(()); },
        );
        assert!(worker.join().unwrap());
    }

    #[test]
    fn test_read_error_backoff_grows_but_never_gives_up() {
        assert_eq!(read_error_backoff(1), Duration::from_millis(10));
        assert_eq!(
            read_error_backoff(FAST_READ_RETRIES),
            Duration::from_secs(1)
        );
        assert_eq!(read_error_backoff(u32::MAX), Duration::from_secs(1));
    }

    #[test]
    fn test_rotated_path() {
        let base = PathBuf::from("/var/log/app.log");
        assert_eq!(
            rotated_path(&base, 1, ""),
            PathBuf::from("/var/log/app.log.1")
        );
        assert_eq!(
            rotated_path(&base, 5, ""),
            PathBuf::from("/var/log/app.log.5")
        );
        assert_eq!(
            rotated_path(&base, 1, ".gz"),
            PathBuf::from("/var/log/app.log.1.gz")
        );
    }
}
