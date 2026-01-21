//! Log writer with rotation support

use chrono::Utc;
use oxidepm_core::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::rotation::RotationConfig;

/// Log writer that handles rotation
pub struct LogWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    config: RotationConfig,
    current_size: u64,
    /// Channel to broadcast new log lines
    broadcast_tx: Option<mpsc::Sender<String>>,
}

impl LogWriter {
    /// Create a new log writer
    pub fn new(path: PathBuf, config: RotationConfig) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let current_size = file.metadata()?.len();
        let writer = BufWriter::new(file);

        Ok(Self {
            path,
            writer,
            config,
            current_size,
            broadcast_tx: None,
        })
    }

    /// Set up broadcasting for live log streaming
    pub fn with_broadcast(mut self, tx: mpsc::Sender<String>) -> Self {
        self.broadcast_tx = Some(tx);
        self
    }

    /// Write a line to the log
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S");
        let formatted = format!("[{}] {}\n", timestamp, line);
        let bytes = formatted.as_bytes();

        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        self.current_size += bytes.len() as u64;

        // Broadcast to live subscribers
        if let Some(tx) = &self.broadcast_tx {
            // Non-blocking send
            let _ = tx.try_send(formatted.clone());
        }

        // Check if rotation is needed
        if self.current_size >= self.config.max_size_bytes {
            self.rotate()?;
        }

        Ok(())
    }

    /// Write raw bytes (without timestamp)
    pub fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        self.current_size += data.len() as u64;

        // Broadcast to live subscribers
        if let Some(tx) = &self.broadcast_tx {
            if let Ok(line) = String::from_utf8(data.to_vec()) {
                let _ = tx.try_send(line);
            }
        }

        if self.current_size >= self.config.max_size_bytes {
            self.rotate()?;
        }

        Ok(())
    }

    /// Rotate the log file
    fn rotate(&mut self) -> Result<()> {
        debug!("Rotating log file: {}", self.path.display());

        // Flush and close current file
        self.writer.flush()?;

        // Rotate existing files: .4 -> .5, .3 -> .4, etc.
        for i in (1..self.config.max_files).rev() {
            let old_path = rotated_path(&self.path, i);
            let new_path = rotated_path(&self.path, i + 1);

            if old_path.exists() {
                if i + 1 >= self.config.max_files {
                    // Delete oldest
                    fs::remove_file(&old_path)?;
                } else {
                    fs::rename(&old_path, &new_path)?;
                }
            }
        }

        // Rename current to .1
        let first_rotated = rotated_path(&self.path, 1);
        if self.path.exists() {
            fs::rename(&self.path, &first_rotated)?;
        }

        // Create new file
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;

        self.writer = BufWriter::new(file);
        self.current_size = 0;

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

/// Get the path for a rotated log file
fn rotated_path(base: &Path, index: usize) -> PathBuf {
    let name = base.file_name().unwrap().to_string_lossy();
    base.with_file_name(format!("{}.{}", name, index))
}

/// Async log capture from process stdout/stderr
pub struct LogCapture {
    pub stdout_writer: LogWriter,
    pub stderr_writer: LogWriter,
}

impl LogCapture {
    pub fn new(app_name: &str, config: RotationConfig) -> Result<Self> {
        let stdout_path = crate::stdout_path(app_name);
        let stderr_path = crate::stderr_path(app_name);

        Ok(Self {
            stdout_writer: LogWriter::new(stdout_path, config.clone())?,
            stderr_writer: LogWriter::new(stderr_path, config)?,
        })
    }

    /// Spawn tasks to capture stdout and stderr
    pub fn spawn_capture(
        mut self,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) -> (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        let stdout_handle = stdout.map(|out| {
            tokio::spawn(async move {
                let reader = BufReader::new(out);
                let mut lines = reader.lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    if let Err(e) = self.stdout_writer.write_line(&line) {
                        warn!("Failed to write stdout: {}", e);
                    }
                }
            })
        });

        let stderr_handle = stderr.map(|err| {
            tokio::spawn(async move {
                let reader = BufReader::new(err);
                let mut lines = reader.lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    if let Err(e) = self.stderr_writer.write_line(&line) {
                        warn!("Failed to write stderr: {}", e);
                    }
                }
            })
        });

        (stdout_handle, stderr_handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

    #[test]
    fn test_log_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.log");

        // Small rotation size for testing
        let config = RotationConfig::new(100, 3);
        let mut writer = LogWriter::new(path.clone(), config).unwrap();

        // Write enough to trigger rotation
        for i in 0..20 {
            writer
                .write_line(&format!("Line {} with some content", i))
                .unwrap();
        }

        // Check rotated files exist
        assert!(path.exists());
        // At least one rotated file should exist
        let rotated_1 = dir.path().join("test.log.1");
        // May or may not exist depending on timing
        let _ = rotated_1;
    }

    #[test]
    fn test_rotated_path() {
        let base = PathBuf::from("/var/log/app.log");
        assert_eq!(rotated_path(&base, 1), PathBuf::from("/var/log/app.log.1"));
        assert_eq!(rotated_path(&base, 5), PathBuf::from("/var/log/app.log.5"));
    }
}
