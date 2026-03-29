//! Webhook notification backends (Discord, Slack, generic HTTP)

use crate::error::{NotifyError, Result};
use crate::event::ProcessEvent;
use crate::Notifier;
use async_trait::async_trait;
use serde::Serialize;
use tracing::{debug, error, info};

/// Discord webhook notifier (uses Discord webhook URL format)
pub struct DiscordNotifier {
    webhook_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct DiscordPayload<'a> {
    content: &'a str,
}

impl DiscordNotifier {
    pub fn new(webhook_url: String) -> Self {
        Self {
            webhook_url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    async fn send(&self, message: &str) -> Result<()> {
        if self.webhook_url.is_empty() {
            return Err(NotifyError::NotConfigured);
        }

        debug!("Sending Discord notification");

        let payload = DiscordPayload { content: message };
        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await?;

        if response.status().is_success() || response.status().as_u16() == 204 {
            info!("Discord notification sent");
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Discord webhook error: {} {}", status, body);
            Err(NotifyError::webhook(format!("Discord HTTP {}: {}", status, body)))
        }
    }

    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()> {
        self.send(&event.format_message()).await
    }

    fn is_configured(&self) -> bool {
        !self.webhook_url.is_empty()
    }
}

/// Slack webhook notifier (uses Slack incoming webhook URL)
pub struct SlackNotifier {
    webhook_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct SlackPayload<'a> {
    text: &'a str,
}

impl SlackNotifier {
    pub fn new(webhook_url: String) -> Self {
        Self {
            webhook_url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Notifier for SlackNotifier {
    async fn send(&self, message: &str) -> Result<()> {
        if self.webhook_url.is_empty() {
            return Err(NotifyError::NotConfigured);
        }

        debug!("Sending Slack notification");

        let payload = SlackPayload { text: message };
        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await?;

        if response.status().is_success() {
            info!("Slack notification sent");
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Slack webhook error: {} {}", status, body);
            Err(NotifyError::webhook(format!("Slack HTTP {}: {}", status, body)))
        }
    }

    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()> {
        self.send(&event.format_message()).await
    }

    fn is_configured(&self) -> bool {
        !self.webhook_url.is_empty()
    }
}

/// Generic HTTP webhook notifier (POST JSON to any URL)
pub struct WebhookNotifier {
    url: String,
    secret: Option<String>,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct WebhookPayload<'a> {
    event: &'a str,
    severity: &'a str,
    message: &'a str,
    process: &'a str,
    id: u32,
    timestamp: String,
}

impl WebhookNotifier {
    pub fn new(url: String, secret: Option<String>) -> Self {
        Self {
            url,
            secret,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    async fn send(&self, message: &str) -> Result<()> {
        if self.url.is_empty() {
            return Err(NotifyError::NotConfigured);
        }

        debug!("Sending webhook notification to {}", self.url);

        let mut req = self.client.post(&self.url).json(&serde_json::json!({
            "message": message,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }));

        if let Some(ref secret) = self.secret {
            req = req.header("X-Webhook-Secret", secret);
        }

        let response = req.send().await?;

        if response.status().is_success() {
            info!("Webhook notification sent");
            Ok(())
        } else {
            let status = response.status();
            error!("Webhook error: {}", status);
            Err(NotifyError::webhook(format!("HTTP {}", status)))
        }
    }

    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()> {
        if self.url.is_empty() {
            return Err(NotifyError::NotConfigured);
        }

        let payload = WebhookPayload {
            event: event.event_type(),
            severity: event.severity().as_str(),
            message: &event.format_message(),
            process: event.name(),
            id: event.id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let mut req = self.client.post(&self.url).json(&payload);

        if let Some(ref secret) = self.secret {
            req = req.header("X-Webhook-Secret", secret);
        }

        let response = req.send().await?;

        if response.status().is_success() {
            info!("Webhook notification sent");
            Ok(())
        } else {
            let status = response.status();
            error!("Webhook error: {}", status);
            Err(NotifyError::webhook(format!("HTTP {}", status)))
        }
    }

    fn is_configured(&self) -> bool {
        !self.url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_not_configured_empty() {
        let notifier = DiscordNotifier::new(String::new());
        assert!(!notifier.is_configured());
    }

    #[test]
    fn test_discord_configured() {
        let notifier = DiscordNotifier::new("https://discord.com/api/webhooks/123/abc".to_string());
        assert!(notifier.is_configured());
    }

    #[test]
    fn test_slack_not_configured_empty() {
        let notifier = SlackNotifier::new(String::new());
        assert!(!notifier.is_configured());
    }

    #[test]
    fn test_slack_configured() {
        let notifier = SlackNotifier::new("https://hooks.slack.com/services/T00/B00/xxx".to_string());
        assert!(notifier.is_configured());
    }

    #[test]
    fn test_webhook_not_configured_empty() {
        let notifier = WebhookNotifier::new(String::new(), None);
        assert!(!notifier.is_configured());
    }

    #[test]
    fn test_webhook_configured() {
        let notifier = WebhookNotifier::new("https://example.com/hook".to_string(), Some("secret".to_string()));
        assert!(notifier.is_configured());
    }

    #[tokio::test]
    async fn test_discord_send_not_configured() {
        let notifier = DiscordNotifier::new(String::new());
        let result = notifier.send("test").await;
        assert!(matches!(result, Err(crate::NotifyError::NotConfigured)));
    }

    #[tokio::test]
    async fn test_slack_send_not_configured() {
        let notifier = SlackNotifier::new(String::new());
        let result = notifier.send("test").await;
        assert!(matches!(result, Err(crate::NotifyError::NotConfigured)));
    }

    #[tokio::test]
    async fn test_webhook_send_not_configured() {
        let notifier = WebhookNotifier::new(String::new(), None);
        let result = notifier.send("test").await;
        assert!(matches!(result, Err(crate::NotifyError::NotConfigured)));
    }
}
