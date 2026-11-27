#![forbid(unsafe_code)]

//! OxidePM Core - Shared types, configuration, and error handling

pub mod actor;
pub mod config;
pub mod constants;
pub mod cron;
pub mod daemon_config;
pub mod envfile;
pub mod error;
pub mod error_code;
pub mod fsutil;
pub mod graph;
pub mod paths;
pub mod redact;
pub mod release_bundle;
pub mod restart;
pub mod runtime_hints;
pub mod types;
pub mod validate;

pub use config::*;
pub use constants::*;
pub use daemon_config::DaemonConfig;
pub use error::{Error, Result};
pub use error_code::ErrorCode;
pub use paths::Paths;
pub use redact::{redact_spec, scrub_message, scrub_text};
pub use types::*;
