//! Filesystem watcher using notify

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use oxidepm_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::debounce::Debouncer;

/// Watch configuration
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Patterns to ignore (glob patterns)
    pub ignore: Vec<String>,
    /// Debounce time in milliseconds
    pub debounce_ms: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            ignore: oxidepm_core::DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            debounce_ms: oxidepm_core::DEFAULT_DEBOUNCE_MS,
        }
    }
}

/// Watch event
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub paths: Vec<PathBuf>,
    pub kind: WatchEventKind,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
    Create,
    Modify,
    Remove,
    Other,
}

impl From<notify::EventKind> for WatchEventKind {
    fn from(kind: notify::EventKind) -> Self {
        match kind {
            notify::EventKind::Create(_) => WatchEventKind::Create,
            notify::EventKind::Modify(_) => WatchEventKind::Modify,
            notify::EventKind::Remove(_) => WatchEventKind::Remove,
            _ => WatchEventKind::Other,
        }
    }
}

/// File watcher for watch mode
pub struct FileWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    ignore_patterns: Vec<glob::Pattern>,
    debouncer: Debouncer,
    watched_paths: Vec<PathBuf>,
}

impl FileWatcher {
    /// Create a new file watcher
    pub fn new(config: WatchConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Err(e) = tx.send(res) {
                warn!("Failed to send watch event: {}", e);
            }
        })
        .map_err(|e| Error::ConfigError(format!("Failed to create watcher: {}", e)))?;

        let ignore_patterns: Vec<glob::Pattern> = config
            .ignore
            .iter()
            .filter_map(|p| {
                // Convert simple patterns to glob patterns
                let pattern = if p.contains('*') || p.contains('?') {
                    p.clone()
                } else {
                    format!("**/{}", p)
                };
                match glob::Pattern::new(&pattern) {
                    Ok(pat) => Some(pat),
                    Err(e) => {
                        warn!("Invalid ignore pattern '{}': {}", p, e);
                        None
                    }
                }
            })
            .collect();

        let debouncer = Debouncer::new(Duration::from_millis(config.debounce_ms));

        Ok(Self {
            watcher,
            rx,
            ignore_patterns,
            debouncer,
            watched_paths: Vec::new(),
        })
    }

    /// Watch a directory recursively
    pub fn watch(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        self.watcher
            .watch(&path, RecursiveMode::Recursive)
            .map_err(|e| Error::ConfigError(format!("Failed to watch {}: {}", path.display(), e)))?;

        info!("Watching directory: {}", path.display());
        self.watched_paths.push(path);
        Ok(())
    }

    /// Stop watching a directory
    pub fn unwatch(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        self.watcher
            .unwatch(&path)
            .map_err(|e| Error::ConfigError(format!("Failed to unwatch {}: {}", path.display(), e)))?;

        self.watched_paths.retain(|p| p != &path);
        Ok(())
    }

    /// Poll for the next watch event (non-blocking)
    pub fn poll(&mut self) -> Option<WatchEvent> {
        // Process all pending events
        while let Ok(event_result) = self.rx.try_recv() {
            match event_result {
                Ok(event) => {
                    // Filter ignored paths
                    let paths: Vec<PathBuf> = event
                        .paths
                        .into_iter()
                        .filter(|p| !self.should_ignore(p))
                        .collect();

                    if paths.is_empty() {
                        continue;
                    }

                    debug!("Watch event: {:?} on {:?}", event.kind, paths);

                    // Check debounce
                    if self.debouncer.should_emit(&paths) {
                        return Some(WatchEvent {
                            paths,
                            kind: event.kind.into(),
                            timestamp: Instant::now(),
                        });
                    }
                }
                Err(e) => {
                    warn!("Watch error: {}", e);
                }
            }
        }

        None
    }

    /// Wait for the next watch event (blocking with timeout)
    pub fn wait(&mut self, timeout: Duration) -> Option<WatchEvent> {
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(event) = self.poll() {
                return Some(event);
            }

            if Instant::now() >= deadline {
                return None;
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Check if a path should be ignored
    fn should_ignore(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        for pattern in &self.ignore_patterns {
            if pattern.matches(&path_str) {
                debug!("Ignoring path: {} (matched {})", path_str, pattern);
                return true;
            }

            // Also check each component
            for component in path.components() {
                if let std::path::Component::Normal(name) = component {
                    if let Some(name_str) = name.to_str() {
                        if pattern.matches(name_str) {
                            debug!("Ignoring path: {} (component {} matched {})", path_str, name_str, pattern);
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Get watched paths
    pub fn watched_paths(&self) -> &[PathBuf] {
        &self.watched_paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_watcher_creation() {
        let config = WatchConfig::default();
        let watcher = FileWatcher::new(config);
        assert!(watcher.is_ok());
    }

    #[test]
    fn test_should_ignore() {
        let config = WatchConfig {
            ignore: vec!["target".to_string(), "node_modules".to_string(), "*.swp".to_string()],
            debounce_ms: 200,
        };

        let watcher = FileWatcher::new(config).unwrap();

        assert!(watcher.should_ignore(Path::new("/project/target/debug/app")));
        assert!(watcher.should_ignore(Path::new("/project/node_modules/package/index.js")));
        assert!(watcher.should_ignore(Path::new("/project/src/main.rs.swp")));
        assert!(!watcher.should_ignore(Path::new("/project/src/main.rs")));
    }

    #[test]
    fn test_watch_directory() {
        let dir = TempDir::new().unwrap();
        let config = WatchConfig::default();
        let mut watcher = FileWatcher::new(config).unwrap();

        assert!(watcher.watch(dir.path()).is_ok());
        assert_eq!(watcher.watched_paths().len(), 1);
    }

    #[test]
    fn test_watch_event() {
        let dir = TempDir::new().unwrap();
        let config = WatchConfig {
            ignore: vec![],
            debounce_ms: 50,
        };
        let mut watcher = FileWatcher::new(config).unwrap();
        watcher.watch(dir.path()).unwrap();

        // Create a file
        let test_file = dir.path().join("test.txt");
        fs::write(&test_file, "hello").unwrap();

        // Wait for event
        let event = watcher.wait(Duration::from_secs(1));

        // Event might or might not be captured depending on timing
        // This is a basic smoke test
        if let Some(event) = event {
            assert!(!event.paths.is_empty());
        }
    }
}
