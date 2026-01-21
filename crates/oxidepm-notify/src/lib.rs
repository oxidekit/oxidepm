//! OxidePM Notification System
//!
//! Provides notification capabilities for process events via various channels:
//! - Telegram
//! - (Future: Slack, Discord, Webhooks, etc.)

pub mod config;
mod error;
mod event;
#[cfg(test)]
pub mod mock;
mod telegram;

pub use config::{notify_config_path, NotifyConfig, TelegramConfig};
pub use error::{NotifyError, Result};
pub use event::ProcessEvent;
pub use telegram::TelegramNotifier;

use async_trait::async_trait;

/// Trait for notification backends
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Send a plain text message
    async fn send(&self, message: &str) -> Result<()>;

    /// Send a formatted process event notification
    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()>;

    /// Check if the notifier is configured and ready
    fn is_configured(&self) -> bool;
}

/// Manager for all notification channels
pub struct NotificationManager {
    telegram: Option<TelegramNotifier>,
    config: NotifyConfig,
}

impl NotificationManager {
    /// Create a new notification manager from config
    pub fn new(config: NotifyConfig) -> Self {
        let telegram = config
            .telegram
            .as_ref()
            .map(|tc| TelegramNotifier::new(tc.bot_token.clone(), tc.chat_id.clone()));

        Self { telegram, config }
    }

    /// Create a notification manager by loading config from default path
    pub fn from_config_file() -> Result<Self> {
        let config = NotifyConfig::load()?;
        Ok(Self::new(config))
    }

    /// Send a process event to all configured channels
    pub async fn notify(&self, event: &ProcessEvent) -> Result<()> {
        // Check if this event type should be notified
        if !self.should_notify(event) {
            return Ok(());
        }

        // Send to Telegram if configured
        if let Some(ref telegram) = self.telegram {
            telegram.send_process_event(event).await?;
        }

        Ok(())
    }

    /// Send a plain message to all configured channels
    pub async fn send_message(&self, message: &str) -> Result<()> {
        if let Some(ref telegram) = self.telegram {
            telegram.send(message).await?;
        }
        Ok(())
    }

    /// Check if any notification channel is configured
    pub fn is_configured(&self) -> bool {
        self.telegram
            .as_ref()
            .map(|t| t.is_configured())
            .unwrap_or(false)
    }

    /// Check if this event type should trigger a notification
    fn should_notify(&self, event: &ProcessEvent) -> bool {
        if self.config.events.is_empty() {
            // If no events specified, notify all
            return true;
        }

        let event_type = event.event_type();
        self.config.events.iter().any(|e| e == event_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_manager_not_configured() {
        let config = NotifyConfig::default();
        let manager = NotificationManager::new(config);
        assert!(!manager.is_configured());
    }

    #[test]
    fn test_should_notify_all_events() {
        let config = NotifyConfig {
            telegram: Some(TelegramConfig {
                bot_token: "test".to_string(),
                chat_id: "123".to_string(),
            }),
            events: vec![],
        };
        let manager = NotificationManager::new(config);

        let event = ProcessEvent::Started {
            name: "test".to_string(),
            id: 1,
        };
        assert!(manager.should_notify(&event));
    }

    #[test]
    fn test_should_notify_filtered_events() {
        let config = NotifyConfig {
            telegram: Some(TelegramConfig {
                bot_token: "test".to_string(),
                chat_id: "123".to_string(),
            }),
            events: vec!["crash".to_string(), "memory_limit".to_string()],
        };
        let manager = NotificationManager::new(config);

        // Should notify crash
        let crash_event = ProcessEvent::Crashed {
            name: "test".to_string(),
            id: 1,
            error: "segfault".to_string(),
        };
        assert!(manager.should_notify(&crash_event));

        // Should not notify start
        let start_event = ProcessEvent::Started {
            name: "test".to_string(),
            id: 1,
        };
        assert!(!manager.should_notify(&start_event));
    }
}
