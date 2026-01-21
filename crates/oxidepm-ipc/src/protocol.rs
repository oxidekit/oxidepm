//! IPC Protocol - Request/Response types

use oxidepm_core::{AppInfo, AppSpec, Selector};
use serde::{Deserialize, Serialize};

/// IPC Request from CLI to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Check if daemon is alive
    Ping,

    /// Start a new process
    Start { spec: AppSpec },

    /// Stop process(es)
    Stop { selector: Selector },

    /// Restart process(es)
    Restart { selector: Selector },

    /// Delete process(es) from registry
    Delete { selector: Selector },

    /// Get status of all processes
    Status,

    /// Get detailed info for a process
    Show { selector: Selector },

    /// Get log lines
    Logs {
        selector: Selector,
        lines: usize,
        follow: bool,
        stdout: bool,
        stderr: bool,
    },

    /// Save current process list
    Save,

    /// Restore saved processes
    Resurrect,

    /// Stop daemon and all processes
    Kill,

    /// Graceful reload (zero-downtime restart)
    Reload { selector: Selector },

    /// Flush/truncate log files for process(es)
    Flush { selector: Selector },

    /// Describe a process (get what command would run)
    Describe { selector: Selector },
}

/// IPC Response from daemon to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Ping response
    Pong,

    /// Success with message
    Ok { message: String },

    /// Error with message
    Error { message: String },

    /// Status response with all app info
    Status { apps: Vec<AppInfo> },

    /// Show response with single app detail
    Show { app: AppInfo },

    /// Log lines response
    LogLines { lines: Vec<String> },

    /// Single log line (for streaming)
    LogLine { line: String },

    /// Start response with app ID
    Started { id: u32, name: String },

    /// Stop response
    Stopped { count: usize },

    /// Restart response
    Restarted { count: usize },

    /// Delete response
    Deleted { count: usize },

    /// Save response
    Saved { count: usize, path: String },

    /// Resurrect response
    Resurrected { count: usize },

    /// Reload response
    Reloaded { count: usize },

    /// Flush response
    Flushed { count: usize },

    /// Describe response with app details
    Described {
        name: String,
        command: String,
        args: Vec<String>,
        cwd: String,
        env: std::collections::HashMap<String, String>,
        mode: String,
    },
}

impl Response {
    pub fn ok<S: Into<String>>(message: S) -> Self {
        Response::Ok {
            message: message.into(),
        }
    }

    pub fn error<S: Into<String>>(message: S) -> Self {
        Response::Error {
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Response::Error { .. })
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Response::Error { message } => Some(message),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppMode;
    use std::path::PathBuf;

    #[test]
    fn test_request_serialize() {
        let req = Request::Start {
            spec: AppSpec::new(
                "test".to_string(),
                AppMode::Node,
                "app.js".to_string(),
                PathBuf::from("/app"),
            ),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("start"));
        assert!(json.contains("test"));
    }

    #[test]
    fn test_response_serialize() {
        let resp = Response::ok("Process started");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("ok"));
        assert!(json.contains("Process started"));
    }

    #[test]
    fn test_selector_in_request() {
        let req = Request::Stop {
            selector: Selector::ByName("myapp".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Stop { selector } => {
                assert_eq!(selector, Selector::ByName("myapp".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_flush_request_serialize() {
        let req = Request::Flush {
            selector: Selector::All,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("flush"));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Flush { selector } => {
                assert_eq!(selector, Selector::All);
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_flushed_response_serialize() {
        let resp = Response::Flushed { count: 3 };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("flushed"));
        assert!(json.contains("3"));
    }

    #[test]
    fn test_describe_request_serialize() {
        let req = Request::Describe {
            selector: Selector::ByName("myapp".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("describe"));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Describe { selector } => {
                assert_eq!(selector, Selector::ByName("myapp".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_described_response_serialize() {
        use std::collections::HashMap;
        let mut env = HashMap::new();
        env.insert("NODE_ENV".to_string(), "production".to_string());

        let resp = Response::Described {
            name: "myapp".to_string(),
            command: "node".to_string(),
            args: vec!["server.js".to_string()],
            cwd: "/app".to_string(),
            env,
            mode: "node".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("described"));
        assert!(json.contains("myapp"));
        assert!(json.contains("node"));
        assert!(json.contains("NODE_ENV"));
    }
}
