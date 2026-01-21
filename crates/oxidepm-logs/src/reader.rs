//! Log reader for tail and follow operations

use oxidepm_core::{Error, Result};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::debug;

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
        if !self.path.exists() {
            return Ok(vec![]);
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);

        // Read all lines and keep last N
        let mut lines: VecDeque<String> = VecDeque::with_capacity(n + 1);

        for line_result in reader.lines() {
            let line = line_result?;
            lines.push_back(line);
            if lines.len() > n {
                lines.pop_front();
            }
        }

        Ok(lines.into_iter().collect())
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
    /// Returns a receiver that yields new lines as they're written
    pub fn follow(&self) -> Result<mpsc::Receiver<String>> {
        let path = self.path.clone();
        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            if let Err(e) = follow_file(&path, tx).await {
                debug!("Follow ended: {}", e);
            }
        });

        Ok(rx)
    }

    /// Check if the log file exists
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Get the file path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get file size
    pub fn size(&self) -> Result<u64> {
        if !self.path.exists() {
            return Ok(0);
        }
        Ok(std::fs::metadata(&self.path)?.len())
    }
}

/// Follow a file for new content
async fn follow_file(path: &Path, tx: mpsc::Sender<String>) -> Result<()> {
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc as std_mpsc;

    let mut file = File::open(path)?;
    let mut position = file.seek(SeekFrom::End(0))?;

    // Set up file watcher
    let (watch_tx, watch_rx) = std_mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = watch_tx.send(res);
        },
        notify::Config::default(),
    )
    .map_err(|e| Error::ConfigError(format!("Failed to create watcher: {}", e)))?;

    watcher
        .watch(path, RecursiveMode::NonRecursive)
        .map_err(|e| Error::ConfigError(format!("Failed to watch file: {}", e)))?;

    loop {
        // Wait for file change or timeout
        match watch_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Ok(_event)) => {
                // File changed, read new content
            }
            Ok(Err(e)) => {
                debug!("Watch error: {}", e);
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Check if channel is still open
                if tx.is_closed() {
                    break;
                }
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        // Read new content
        file.seek(SeekFrom::Start(position))?;
        let reader = BufReader::new(&file);

        for line_result in reader.lines() {
            let line = line_result?;
            position += line.len() as u64 + 1; // +1 for newline

            if tx.send(line).await.is_err() {
                return Ok(()); // Channel closed
            }
        }

        // Update position
        position = file.seek(SeekFrom::End(0))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

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
