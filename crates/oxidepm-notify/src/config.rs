//! Configuration types for notification system

use crate::error::{NotifyError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, info};

/// Get the default notification config path
pub fn notify_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".oxidepm")
        .join("notify.toml")
}

/// Ensure the config directory exists
fn ensure_config_dir() -> Result<()> {
    let path = notify_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Notification configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotifyConfig {
    /// Telegram notification settings
    pub telegram: Option<TelegramConfig>,

    /// Discord webhook settings
    pub discord: Option<DiscordConfig>,

    /// Slack webhook settings
    pub slack: Option<SlackConfig>,

    /// Generic HTTP webhook settings
    pub webhook: Option<WebhookConfig>,

    /// Events to notify on (empty = all events)
    #[serde(default)]
    pub events: Vec<String>,

    /// Minimum severity level to trigger notifications (info, warning, critical)
    /// Events below this severity are silently dropped.
    pub min_severity: Option<String>,
}

impl NotifyConfig {
    /// Load config from the default path
    pub fn load() -> Result<Self> {
        let path = notify_config_path();
        Self::load_from(&path)
    }

    /// Load config from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            debug!("Notify config not found at {:?}, using defaults", path);
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: NotifyConfig = toml::from_str(&content)?;

        debug!("Loaded notify config from {:?}", path);
        Ok(config)
    }

    /// Save config to the default path
    pub fn save(&self) -> Result<()> {
        let path = notify_config_path();
        self.save_to(&path)
    }

    /// Save config to a specific path
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        ensure_config_dir()?;

        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, &content)?;

        // Set file permissions to owner-only (0600) for security
        // Config may contain sensitive data like API tokens
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("Failed to set config file permissions: {}", e);
            }
        }

        info!("Saved notify config to {:?}", path);
        Ok(())
    }

    /// Check if any notification channel is configured
    pub fn is_configured(&self) -> bool {
        self.telegram.is_some()
            || self.discord.is_some()
            || self.slack.is_some()
            || self.webhook.is_some()
    }

    /// Configure Telegram notifications
    pub fn set_telegram(&mut self, bot_token: String, chat_id: String) {
        self.telegram = Some(TelegramConfig {
            bot_token,
            bot_token_env: None,
            chat_id,
        });
    }

    /// Remove Telegram configuration
    pub fn remove_telegram(&mut self) {
        self.telegram = None;
    }

    /// Set events to notify on
    pub fn set_events(&mut self, events: Vec<String>) {
        self.events = events;
    }

    /// Validate event names
    pub fn validate_events(&self) -> Result<()> {
        const VALID_EVENTS: &[&str] = &[
            "start",
            "stop",
            "crash",
            "restart",
            "memory_limit",
            "health_check",
            "validator_activated",
            "catching_up",
            "missed_blocks",
            "jailed",
            "stale_node",
            "upgrade_halted",
            "double_sign_risk",
        ];

        for event in &self.events {
            if !VALID_EVENTS.contains(&event.as_str()) {
                return Err(NotifyError::config(format!(
                    "Invalid event type '{}'. Valid types: {:?}",
                    event, VALID_EVENTS
                )));
            }
        }
        Ok(())
    }
}

/// Telegram notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather (prefer bot_token_env for security)
    #[serde(default)]
    pub bot_token: String,

    /// Environment variable name containing the bot token (preferred over bot_token)
    pub bot_token_env: Option<String>,

    /// Chat ID to send messages to (can be user, group, or channel)
    pub chat_id: String,
}

impl TelegramConfig {
    /// Resolve the bot token — prefer env var, fall back to direct value
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(ref env_var) = self.bot_token_env {
            if let Ok(token) = std::env::var(env_var) {
                if !token.is_empty() {
                    return Some(token);
                }
            }
            tracing::warn!(
                "bot_token_env '{}' is not set or empty, falling back to bot_token",
                env_var
            );
        }
        if !self.bot_token.is_empty() {
            Some(self.bot_token.clone())
        } else {
            None
        }
    }
}

/// Discord notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Discord webhook URL (from channel settings → Integrations → Webhooks)
    pub webhook_url: String,
}

/// Slack notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackConfig {
    /// Slack incoming webhook URL
    pub webhook_url: String,
}

/// Generic HTTP webhook configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// URL to POST event JSON to
    pub url: String,
    /// Optional secret sent as X-Webhook-Secret header
    pub secret: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = NotifyConfig::default();
        assert!(config.telegram.is_none());
        assert!(config.events.is_empty());
        assert!(!config.is_configured());
    }

    #[test]
    fn test_load_missing_config() {
        let path = PathBuf::from("/nonexistent/notify.toml");
        let config = NotifyConfig::load_from(&path).unwrap();
        assert!(config.telegram.is_none());
    }

    #[test]
    fn test_load_config() {
        let content = r#"
events = ["crash", "restart"]

[telegram]
bot_token = "123456:ABC-DEF"
chat_id = "-100123456789"
"#;
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let config = NotifyConfig::load_from(&file.path().to_path_buf()).unwrap();

        assert!(config.telegram.is_some());
        let telegram = config.telegram.unwrap();
        assert_eq!(telegram.bot_token, "123456:ABC-DEF");
        assert_eq!(telegram.chat_id, "-100123456789");
        assert_eq!(config.events, vec!["crash", "restart"]);
    }

    #[test]
    fn test_save_and_load_config() {
        let mut config = NotifyConfig::default();
        config.set_telegram("test_token".to_string(), "test_chat".to_string());
        config.set_events(vec!["crash".to_string()]);

        let file = NamedTempFile::with_suffix(".toml").unwrap();
        let path = file.path().to_path_buf();

        config.save_to(&path).unwrap();

        let loaded = NotifyConfig::load_from(&path).unwrap();
        assert!(loaded.telegram.is_some());
        let telegram = loaded.telegram.unwrap();
        assert_eq!(telegram.bot_token, "test_token");
        assert_eq!(telegram.chat_id, "test_chat");
        assert_eq!(loaded.events, vec!["crash"]);
    }

    #[test]
    fn test_validate_events_valid() {
        let config = NotifyConfig {
            events: vec!["crash".to_string(), "restart".to_string()],
            ..Default::default()
        };
        assert!(config.validate_events().is_ok());
    }

    #[test]
    fn test_validate_events_invalid() {
        let config = NotifyConfig {
            events: vec!["invalid_event".to_string()],
            ..Default::default()
        };
        assert!(config.validate_events().is_err());
    }

    #[test]
    fn test_set_and_remove_telegram() {
        let mut config = NotifyConfig::default();
        assert!(!config.is_configured());

        config.set_telegram("token".to_string(), "chat".to_string());
        assert!(config.is_configured());

        config.remove_telegram();
        assert!(!config.is_configured());
    }

    #[test]
    fn test_discord_config() {
        let config = NotifyConfig {
            discord: Some(DiscordConfig {
                webhook_url: "https://discord.com/api/webhooks/123/abc".to_string(),
            }),
            ..Default::default()
        };
        assert!(config.is_configured());
    }

    #[test]
    fn test_slack_config() {
        let config = NotifyConfig {
            slack: Some(SlackConfig {
                webhook_url: "https://hooks.slack.com/services/T00/B00/xxx".to_string(),
            }),
            ..Default::default()
        };
        assert!(config.is_configured());
    }

    #[test]
    fn test_webhook_config() {
        let config = NotifyConfig {
            webhook: Some(WebhookConfig {
                url: "https://example.com/hook".to_string(),
                secret: Some("s3cret".to_string()),
            }),
            ..Default::default()
        };
        assert!(config.is_configured());
    }

    #[test]
    fn test_multi_channel_config_toml() {
        let content = r#"
events = ["crash"]

[telegram]
bot_token = "123:ABC"
chat_id = "-100123"

[discord]
webhook_url = "https://discord.com/api/webhooks/123/abc"

[slack]
webhook_url = "https://hooks.slack.com/services/T00/B00/xxx"

[webhook]
url = "https://example.com/hook"
secret = "s3cret"
"#;
        let config: NotifyConfig = toml::from_str(content).unwrap();
        assert!(config.telegram.is_some());
        assert!(config.discord.is_some());
        assert!(config.slack.is_some());
        assert!(config.webhook.is_some());
        assert_eq!(config.webhook.unwrap().secret, Some("s3cret".to_string()));
    }
}
