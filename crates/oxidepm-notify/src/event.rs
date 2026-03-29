//! Process event types for notifications

use serde::{Deserialize, Serialize};

/// Notification severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "info" => Ok(Severity::Info),
            "warning" | "warn" => Ok(Severity::Warning),
            "critical" | "crit" => Ok(Severity::Critical),
            _ => Err(format!("Invalid severity: {}", s)),
        }
    }
}

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

    /// Cosmos validator activated (relay-until-synced complete)
    ValidatorActivated {
        name: String,
        id: u32,
        height: u64,
    },

    /// Cosmos node catching up (falling behind)
    CatchingUp {
        name: String,
        id: u32,
        behind_blocks: u64,
    },

    /// Cosmos validator missed blocks (approaching jail threshold)
    MissedBlocks {
        name: String,
        id: u32,
        missed: u64,
        window: u64,
        jail_threshold: u64,
    },

    /// Cosmos validator jailed
    Jailed {
        name: String,
        id: u32,
        height: u64,
    },

    /// Cosmos node stale (no new blocks)
    StaleNode {
        name: String,
        id: u32,
        seconds: u64,
    },

    /// Cosmos upgrade halt detected (intentional stop, not a crash)
    UpgradeHalted {
        name: String,
        id: u32,
        upgrade_name: String,
        halt_height: u64,
    },

    /// Double-sign risk: another instance may be signing with the same validator key
    DoubleSignRisk {
        name: String,
        id: u32,
        chain_height: u64,
        local_height: u64,
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
            ProcessEvent::ValidatorActivated { .. } => "validator_activated",
            ProcessEvent::CatchingUp { .. } => "catching_up",
            ProcessEvent::MissedBlocks { .. } => "missed_blocks",
            ProcessEvent::Jailed { .. } => "jailed",
            ProcessEvent::StaleNode { .. } => "stale_node",
            ProcessEvent::UpgradeHalted { .. } => "upgrade_halted",
            ProcessEvent::DoubleSignRisk { .. } => "double_sign_risk",
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
            // SECURITY: format_message() NEVER includes key paths, IPs, or config details
            ProcessEvent::ValidatorActivated { name, id: _, height } => {
                format!(
                    "\u{2705} Validator activated: `{}` at height {}",
                    name, height
                )
            }
            ProcessEvent::CatchingUp { name, id: _, behind_blocks } => {
                format!(
                    "\u{26A0}\u{FE0F} Node catching up: `{}` — {} blocks behind",
                    name, behind_blocks
                )
            }
            ProcessEvent::MissedBlocks { name, id: _, missed, window, jail_threshold } => {
                format!(
                    "\u{26A0}\u{FE0F} Missed blocks: `{}` — {}/{} (jail at {})",
                    name, missed, window, jail_threshold
                )
            }
            ProcessEvent::Jailed { name, id: _, height } => {
                format!(
                    "\u{1F6A8} JAILED: `{}` at height {}",
                    name, height
                )
            }
            ProcessEvent::StaleNode { name, id: _, seconds } => {
                format!(
                    "\u{1F6A8} Node stale: `{}` — no new blocks for {}s",
                    name, seconds
                )
            }
            ProcessEvent::UpgradeHalted { name, id: _, upgrade_name, halt_height } => {
                format!(
                    "\u{1F6E0}\u{FE0F} Upgrade halt: `{}` halted for upgrade '{}' at height {}. Run: monarch upgrade --version {}",
                    name, upgrade_name, halt_height, upgrade_name
                )
            }
            // SECURITY: format_message() NEVER includes key paths, IPs, or config details
            ProcessEvent::DoubleSignRisk { name, id: _, chain_height, local_height } => {
                format!(
                    "\u{1F6A8}\u{1F6A8} DOUBLE-SIGN RISK: `{}` — chain at height {} but local last signed at {}. \
                     Another instance may be signing with the same validator key! \
                     Investigate IMMEDIATELY to prevent slashing.",
                    name, chain_height, local_height
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
            | ProcessEvent::HealthCheckFailed { name, .. }
            | ProcessEvent::ValidatorActivated { name, .. }
            | ProcessEvent::CatchingUp { name, .. }
            | ProcessEvent::MissedBlocks { name, .. }
            | ProcessEvent::Jailed { name, .. }
            | ProcessEvent::StaleNode { name, .. }
            | ProcessEvent::UpgradeHalted { name, .. }
            | ProcessEvent::DoubleSignRisk { name, .. } => name,
        }
    }

    /// Get the severity level of this event
    pub fn severity(&self) -> Severity {
        match self {
            ProcessEvent::Started { .. } => Severity::Info,
            ProcessEvent::Stopped { .. } => Severity::Info,
            ProcessEvent::Crashed { .. } => Severity::Critical,
            ProcessEvent::Restarted { .. } => Severity::Warning,
            ProcessEvent::MemoryLimit { .. } => Severity::Warning,
            ProcessEvent::HealthCheckFailed { .. } => Severity::Critical,
            ProcessEvent::ValidatorActivated { .. } => Severity::Info,
            ProcessEvent::CatchingUp { .. } => Severity::Warning,
            ProcessEvent::MissedBlocks { .. } => Severity::Warning,
            ProcessEvent::Jailed { .. } => Severity::Critical,
            ProcessEvent::StaleNode { .. } => Severity::Critical,
            ProcessEvent::UpgradeHalted { .. } => Severity::Warning,
            ProcessEvent::DoubleSignRisk { .. } => Severity::Critical,
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
            | ProcessEvent::HealthCheckFailed { id, .. }
            | ProcessEvent::ValidatorActivated { id, .. }
            | ProcessEvent::CatchingUp { id, .. }
            | ProcessEvent::MissedBlocks { id, .. }
            | ProcessEvent::Jailed { id, .. }
            | ProcessEvent::StaleNode { id, .. }
            | ProcessEvent::UpgradeHalted { id, .. }
            | ProcessEvent::DoubleSignRisk { id, .. } => *id,
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
