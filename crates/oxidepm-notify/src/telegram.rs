//! Telegram notification backend

use crate::error::{NotifyError, Result};
use crate::event::ProcessEvent;
use crate::Notifier;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

/// Telegram API response
#[derive(Debug, Deserialize)]
struct TelegramResponse {
    ok: bool,
    description: Option<String>,
}

/// Request body for sendMessage
#[derive(Debug, Serialize)]
struct SendMessageRequest<'a> {
    chat_id: &'a str,
    text: &'a str,
    parse_mode: &'a str,
}

/// Telegram notification backend
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl TelegramNotifier {
    /// Create a new Telegram notifier
    pub fn new(bot_token: String, chat_id: String) -> Self {
        Self {
            bot_token,
            chat_id,
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom HTTP client (useful for testing)
    pub fn with_client(bot_token: String, chat_id: String, client: reqwest::Client) -> Self {
        Self {
            bot_token,
            chat_id,
            client,
        }
    }

    /// Get the Telegram API URL for sendMessage
    fn api_url(&self) -> String {
        format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        )
    }

    /// Send a message using the Telegram Bot API
    async fn send_telegram_message(&self, text: &str) -> Result<()> {
        if self.bot_token.is_empty() || self.chat_id.is_empty() {
            return Err(NotifyError::NotConfigured);
        }

        let request = SendMessageRequest {
            chat_id: &self.chat_id,
            text,
            parse_mode: "Markdown",
        };

        debug!("Sending Telegram message to chat {}", self.chat_id);

        let response = self
            .client
            .post(self.api_url())
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        let body: TelegramResponse = response.json().await?;

        if body.ok {
            info!("Telegram notification sent successfully");
            Ok(())
        } else {
            let error_msg = body
                .description
                .unwrap_or_else(|| format!("HTTP {}", status));
            error!("Telegram API error: {}", error_msg);
            Err(NotifyError::telegram(error_msg))
        }
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn send(&self, message: &str) -> Result<()> {
        self.send_telegram_message(message).await
    }

    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()> {
        let message = event.format_message();
        self.send_telegram_message(&message).await
    }

    fn is_configured(&self) -> bool {
        !self.bot_token.is_empty() && !self.chat_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notifier_not_configured_empty_token() {
        let notifier = TelegramNotifier::new(String::new(), "123".to_string());
        assert!(!notifier.is_configured());
    }

    #[test]
    fn test_notifier_not_configured_empty_chat() {
        let notifier = TelegramNotifier::new("token".to_string(), String::new());
        assert!(!notifier.is_configured());
    }

    #[test]
    fn test_notifier_configured() {
        let notifier = TelegramNotifier::new("token".to_string(), "123".to_string());
        assert!(notifier.is_configured());
    }

    #[test]
    fn test_api_url() {
        let notifier = TelegramNotifier::new("my_bot_token".to_string(), "123".to_string());
        assert_eq!(
            notifier.api_url(),
            "https://api.telegram.org/botmy_bot_token/sendMessage"
        );
    }

    #[tokio::test]
    async fn test_send_not_configured() {
        let notifier = TelegramNotifier::new(String::new(), String::new());
        let result = notifier.send("test").await;
        assert!(matches!(result, Err(NotifyError::NotConfigured)));
    }
}
