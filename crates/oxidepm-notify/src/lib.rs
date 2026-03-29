//! OxidePM Notification System
//!
//! Provides notification capabilities for process events via various channels:
//! - Telegram
//! - Discord (webhook)
//! - Slack (incoming webhook)
//! - Generic HTTP webhook

pub mod config;
mod error;
mod event;
#[cfg(test)]
pub mod mock;
mod telegram;
mod webhook;

pub use config::{notify_config_path, DiscordConfig, NotifyConfig, SlackConfig, TelegramConfig, WebhookConfig};
pub use error::{NotifyError, Result};
pub use event::{ProcessEvent, Severity};
pub use telegram::TelegramNotifier;
pub use webhook::{DiscordNotifier, SlackNotifier, WebhookNotifier};

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

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

/// Rate limiter to prevent notification spam during crash loops
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(300); // 5 minutes
const RATE_LIMIT_MAX: usize = 5; // Max notifications per event type per window

/// Manager for all notification channels
pub struct NotificationManager {
    telegram: Option<TelegramNotifier>,
    discord: Option<DiscordNotifier>,
    slack: Option<SlackNotifier>,
    webhook: Option<WebhookNotifier>,
    config: NotifyConfig,
    /// Rate limiter: tracks (event_key) -> (count, window_start)
    rate_limiter: Mutex<HashMap<String, (usize, Instant)>>,
}

impl NotificationManager {
    /// Create a new notification manager from config
    pub fn new(config: NotifyConfig) -> Self {
        let telegram = config.telegram.as_ref().and_then(|tc| {
            let token = tc.resolve_token()?;
            Some(TelegramNotifier::new(token, tc.chat_id.clone()))
        });

        let discord = config
            .discord
            .as_ref()
            .map(|dc| DiscordNotifier::new(dc.webhook_url.clone()));

        let slack = config
            .slack
            .as_ref()
            .map(|sc| SlackNotifier::new(sc.webhook_url.clone()));

        let webhook = config
            .webhook
            .as_ref()
            .map(|wc| WebhookNotifier::new(wc.url.clone(), wc.secret.clone()));

        Self {
            telegram,
            discord,
            slack,
            webhook,
            config,
            rate_limiter: Mutex::new(HashMap::new()),
        }
    }

    /// Create a notification manager by loading config from default path
    pub fn from_config_file() -> Result<Self> {
        let config = NotifyConfig::load()?;
        Ok(Self::new(config))
    }

    /// Send a process event to all configured channels
    pub async fn notify(&self, event: &ProcessEvent) -> Result<()> {
        if !self.should_notify(event) {
            return Ok(());
        }

        // Rate limiting: prevent spam during crash loops
        let rate_key = format!("{}:{}", event.name(), event.event_type());
        let rate_action = {
            if let Ok(mut limiter) = self.rate_limiter.lock() {
                let now = Instant::now();
                let entry = limiter.entry(rate_key.clone()).or_insert((0, now));

                if now.duration_since(entry.1) > RATE_LIMIT_WINDOW {
                    *entry = (0, now);
                }

                entry.0 += 1;

                if entry.0 > RATE_LIMIT_MAX {
                    if entry.0 == RATE_LIMIT_MAX + 1 {
                        Some(format!(
                            "Notifications for '{}' ({}) rate-limited — {} in {}s. Suppressing further alerts.",
                            event.name(), event.event_type(), RATE_LIMIT_MAX, RATE_LIMIT_WINDOW.as_secs()
                        ))
                    } else {
                        // Already rate limited, silently drop
                        return Ok(());
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }; // MutexGuard dropped here

        if let Some(msg) = rate_action {
            debug!("Rate limiting notifications for {}", rate_key);
            let _ = self.send_message(&msg).await;
            return Ok(());
        }

        if let Some(ref telegram) = self.telegram {
            if let Err(e) = telegram.send_process_event(event).await {
                warn!("Telegram notification failed: {}", e);
            }
        }
        if let Some(ref discord) = self.discord {
            if let Err(e) = discord.send_process_event(event).await {
                warn!("Discord notification failed: {}", e);
            }
        }
        if let Some(ref slack) = self.slack {
            if let Err(e) = slack.send_process_event(event).await {
                warn!("Slack notification failed: {}", e);
            }
        }
        if let Some(ref webhook) = self.webhook {
            if let Err(e) = webhook.send_process_event(event).await {
                warn!("Webhook notification failed: {}", e);
            }
        }

        Ok(())
    }

    /// Send a plain message to all configured channels
    pub async fn send_message(&self, message: &str) -> Result<()> {
        if let Some(ref telegram) = self.telegram {
            let _ = telegram.send(message).await;
        }
        if let Some(ref discord) = self.discord {
            let _ = discord.send(message).await;
        }
        if let Some(ref slack) = self.slack {
            let _ = slack.send(message).await;
        }
        if let Some(ref webhook) = self.webhook {
            let _ = webhook.send(message).await;
        }
        Ok(())
    }

    /// Check if any notification channel is configured
    pub fn is_configured(&self) -> bool {
        self.telegram.as_ref().map(|t| t.is_configured()).unwrap_or(false)
            || self.discord.as_ref().map(|d| d.is_configured()).unwrap_or(false)
            || self.slack.as_ref().map(|s| s.is_configured()).unwrap_or(false)
            || self.webhook.as_ref().map(|w| w.is_configured()).unwrap_or(false)
    }

    /// Check if this event type should trigger a notification
    fn should_notify(&self, event: &ProcessEvent) -> bool {
        // Check severity filter
        if let Some(ref min_sev_str) = self.config.min_severity {
            if let Ok(min_sev) = min_sev_str.parse::<Severity>() {
                if event.severity() < min_sev {
                    return false;
                }
            }
        }

        if self.config.events.is_empty() {
            // If no events specified, notify all (that pass severity)
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
                bot_token_env: None,
                chat_id: "123".to_string(),
            }),
            events: vec![],
            min_severity: None,
            ..Default::default()
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
                bot_token_env: None,
                chat_id: "123".to_string(),
            }),
            events: vec!["crash".to_string(), "memory_limit".to_string()],
            min_severity: None,
            ..Default::default()
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
