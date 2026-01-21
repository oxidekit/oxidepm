//! Log rotation configuration

use oxidepm_core::constants;

/// Log rotation configuration
#[derive(Debug, Clone)]
pub struct RotationConfig {
    /// Maximum log file size in bytes
    pub max_size_bytes: u64,
    /// Maximum number of rotated files to keep
    pub max_files: usize,
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: constants::DEFAULT_LOG_MAX_SIZE,
            max_files: constants::DEFAULT_LOG_MAX_FILES,
        }
    }
}

impl RotationConfig {
    pub fn new(max_size_bytes: u64, max_files: usize) -> Self {
        Self {
            max_size_bytes,
            max_files,
        }
    }
}
