//! Process event types for notifications

use serde::{Deserialize, Serialize};

/// Events that can trigger notifications
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessEvent {
    /// Process started successfully
    Started { name: String, id: u32 },

    /// Process stopped gracefully
    Stopped {
        name: String,
        id: u32,
        exit_code: Option<i32>,
    },

    /// Process crashed unexpectedly
    Crashed { name: String, id: u32, error: String },

    /// Process was restarted (auto or manual)
    Restarted {
        name: String,
        id: u32,
        restart_count: u32,
    },

    /// Process exceeded memory limit
    MemoryLimit {
        name: String,
        id: u32,
        memory_mb: u64,
        limit_mb: u64,
    },

    /// Health check failed
    HealthCheckFailed {
        name: String,
        id: u32,
        endpoint: String,
    },
}

impl ProcessEvent {
    /// Get the event type as a string for filtering
    pub fn event_type(&self) -> &'static str {
        match self {
            ProcessEvent::Started { .. } => "start",
            ProcessEvent::Stopped { .. } => "stop",
            ProcessEvent::Crashed { .. } => "crash",
            ProcessEvent::Restarted { .. } => "restart",
            ProcessEvent::MemoryLimit { .. } => "memory_limit",
            ProcessEvent::HealthCheckFailed { .. } => "health_check",
        }
    }

    /// Format the event as a human-readable message with emoji
    pub fn format_message(&self) -> String {
        match self {
            ProcessEvent::Started { name, id } => {
                format!("\u{1F7E2} Started: `{}` (id: {})", name, id)
            }
            ProcessEvent::Stopped { name, id: _, exit_code } => {
                let code_str = exit_code
                    .map(|c| format!(" - Exit code {}", c))
                    .unwrap_or_default();
                format!("\u{26AA} Stopped: `{}`{}", name, code_str)
            }
            ProcessEvent::Crashed { name, id, error } => {
                format!("\u{1F534} Crashed: `{}` (id: {})\nError: {}", name, id, error)
            }
            ProcessEvent::Restarted {
                name,
                id,
                restart_count,
            } => {
                let ordinal = match restart_count {
                    1 => "1st".to_string(),
                    2 => "2nd".to_string(),
                    3 => "3rd".to_string(),
                    n => format!("{}th", n),
                };
                format!(
                    "\u{1F504} Restarted: `{}` (id: {}, {} restart)",
                    name, id, ordinal
                )
            }
            ProcessEvent::MemoryLimit {
                name,
                id,
                memory_mb,
                limit_mb,
            } => {
                format!(
                    "\u{26A0}\u{FE0F} Memory limit: `{}` (id: {})\nUsing {}MB / {}MB limit",
                    name, id, memory_mb, limit_mb
                )
            }
            ProcessEvent::HealthCheckFailed { name, id, endpoint } => {
                format!(
                    "\u{1F6A8} Health check failed: `{}` (id: {})\nEndpoint: {}",
                    name, id, endpoint
                )
            }
        }
    }

    /// Get the process name from the event
    pub fn name(&self) -> &str {
        match self {
            ProcessEvent::Started { name, .. }
            | ProcessEvent::Stopped { name, .. }
            | ProcessEvent::Crashed { name, .. }
            | ProcessEvent::Restarted { name, .. }
            | ProcessEvent::MemoryLimit { name, .. }
            | ProcessEvent::HealthCheckFailed { name, .. } => name,
        }
    }

    /// Get the process ID from the event
    pub fn id(&self) -> u32 {
        match self {
            ProcessEvent::Started { id, .. }
            | ProcessEvent::Stopped { id, .. }
            | ProcessEvent::Crashed { id, .. }
            | ProcessEvent::Restarted { id, .. }
            | ProcessEvent::MemoryLimit { id, .. }
            | ProcessEvent::HealthCheckFailed { id, .. } => *id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type() {
        assert_eq!(
            ProcessEvent::Started {
                name: "test".to_string(),
                id: 1
            }
            .event_type(),
            "start"
        );
        assert_eq!(
            ProcessEvent::Crashed {
                name: "test".to_string(),
                id: 1,
                error: "err".to_string()
            }
            .event_type(),
            "crash"
        );
    }

    #[test]
    fn test_format_message_started() {
        let event = ProcessEvent::Started {
            name: "api".to_string(),
            id: 1,
        };
        let msg = event.format_message();
        assert!(msg.contains("api"));
        assert!(msg.contains("id: 1"));
    }

    #[test]
    fn test_format_message_crashed() {
        let event = ProcessEvent::Crashed {
            name: "api".to_string(),
            id: 1,
            error: "segfault".to_string(),
        };
        let msg = event.format_message();
        assert!(msg.contains("Crashed"));
        assert!(msg.contains("segfault"));
    }

    #[test]
    fn test_format_message_restarted_ordinals() {
        let event1 = ProcessEvent::Restarted {
            name: "api".to_string(),
            id: 1,
            restart_count: 1,
        };
        assert!(event1.format_message().contains("1st"));

        let event2 = ProcessEvent::Restarted {
            name: "api".to_string(),
            id: 1,
            restart_count: 2,
        };
        assert!(event2.format_message().contains("2nd"));

        let event3 = ProcessEvent::Restarted {
            name: "api".to_string(),
            id: 1,
            restart_count: 3,
        };
        assert!(event3.format_message().contains("3rd"));

        let event4 = ProcessEvent::Restarted {
            name: "api".to_string(),
            id: 1,
            restart_count: 4,
        };
        assert!(event4.format_message().contains("4th"));
    }

    #[test]
    fn test_format_message_memory_limit() {
        let event = ProcessEvent::MemoryLimit {
            name: "api".to_string(),
            id: 1,
            memory_mb: 512,
            limit_mb: 256,
        };
        let msg = event.format_message();
        assert!(msg.contains("512MB"));
        assert!(msg.contains("256MB"));
    }

    #[test]
    fn test_serialization() {
        let event = ProcessEvent::Started {
            name: "api".to_string(),
            id: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"started\""));
    }
}
