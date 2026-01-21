//! Debounce logic for watch events

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Debouncer for file system events
pub struct Debouncer {
    /// Minimum time between events for the same path
    threshold: Duration,
    /// Last event time per path
    last_events: HashMap<PathBuf, Instant>,
}

impl Debouncer {
    /// Create a new debouncer
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            last_events: HashMap::new(),
        }
    }

    /// Check if an event for these paths should be emitted
    pub fn should_emit(&mut self, paths: &[PathBuf]) -> bool {
        let now = Instant::now();
        let mut should_emit = false;

        for path in paths {
            if let Some(last) = self.last_events.get(path) {
                if now.duration_since(*last) >= self.threshold {
                    self.last_events.insert(path.clone(), now);
                    should_emit = true;
                }
            } else {
                self.last_events.insert(path.clone(), now);
                should_emit = true;
            }
        }

        // Clean up old entries (older than 10x threshold)
        let cleanup_threshold = self.threshold * 10;
        self.last_events.retain(|_, v| now.duration_since(*v) < cleanup_threshold);

        should_emit
    }

    /// Reset the debouncer
    pub fn reset(&mut self) {
        self.last_events.clear();
    }

    /// Get the threshold duration
    pub fn threshold(&self) -> Duration {
        self.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debounce_first_event() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let paths = vec![PathBuf::from("/test/file.txt")];

        assert!(debouncer.should_emit(&paths));
    }

    #[test]
    fn test_debounce_rapid_events() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let paths = vec![PathBuf::from("/test/file.txt")];

        // First event should emit
        assert!(debouncer.should_emit(&paths));

        // Immediate second event should not emit
        assert!(!debouncer.should_emit(&paths));
    }

    #[test]
    fn test_debounce_after_threshold() {
        let mut debouncer = Debouncer::new(Duration::from_millis(10));
        let paths = vec![PathBuf::from("/test/file.txt")];

        // First event
        assert!(debouncer.should_emit(&paths));

        // Wait longer than threshold
        std::thread::sleep(Duration::from_millis(20));

        // Should emit again
        assert!(debouncer.should_emit(&paths));
    }

    #[test]
    fn test_debounce_different_paths() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));

        let paths1 = vec![PathBuf::from("/test/file1.txt")];
        let paths2 = vec![PathBuf::from("/test/file2.txt")];

        // Both should emit
        assert!(debouncer.should_emit(&paths1));
        assert!(debouncer.should_emit(&paths2));
    }

    #[test]
    fn test_debounce_reset() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let paths = vec![PathBuf::from("/test/file.txt")];

        debouncer.should_emit(&paths);
        debouncer.reset();

        // After reset, should emit again
        assert!(debouncer.should_emit(&paths));
    }
}
