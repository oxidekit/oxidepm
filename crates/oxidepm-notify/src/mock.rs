//! Mock implementations for testing

use crate::error::Result;
use crate::event::ProcessEvent;
use crate::Notifier;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A mock notifier for testing that records all sent messages
#[derive(Default)]
pub struct MockNotifier {
    /// Messages that have been sent
    messages: Arc<Mutex<Vec<String>>>,
    /// Events that have been sent
    events: Arc<Mutex<Vec<ProcessEvent>>>,
    /// Number of send calls
    call_count: AtomicUsize,
    /// Whether to simulate failures
    should_fail: bool,
}

impl MockNotifier {
    /// Create a new mock notifier
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a mock notifier that always fails
    pub fn failing() -> Self {
        Self {
            should_fail: true,
            ..Default::default()
        }
    }

    /// Get the number of times send was called
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Get all sent messages
    pub async fn messages(&self) -> Vec<String> {
        self.messages.lock().await.clone()
    }

    /// Get all sent events
    pub async fn events(&self) -> Vec<ProcessEvent> {
        self.events.lock().await.clone()
    }

    /// Check if a specific message was sent
    pub async fn was_message_sent(&self, message: &str) -> bool {
        self.messages.lock().await.iter().any(|m| m.contains(message))
    }

    /// Check if a specific event type was sent
    pub async fn was_event_type_sent(&self, event_type: &str) -> bool {
        self.events
            .lock()
            .await
            .iter()
            .any(|e| e.event_type() == event_type)
    }
}

#[async_trait]
impl Notifier for MockNotifier {
    async fn send(&self, message: &str) -> Result<()> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        if self.should_fail {
            return Err(crate::error::NotifyError::TelegramError(
                "Mock failure".to_string(),
            ));
        }

        self.messages.lock().await.push(message.to_string());
        Ok(())
    }

    async fn send_process_event(&self, event: &ProcessEvent) -> Result<()> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        if self.should_fail {
            return Err(crate::error::NotifyError::TelegramError(
                "Mock failure".to_string(),
            ));
        }

        let message = event.format_message();
        self.messages.lock().await.push(message);
        self.events.lock().await.push(event.clone());
        Ok(())
    }

    fn is_configured(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_notifier_records_messages() {
        let notifier = MockNotifier::new();
        notifier.send("Hello, world!").await.unwrap();
        notifier.send("Another message").await.unwrap();

        assert_eq!(notifier.call_count(), 2);
        let messages = notifier.messages().await;
        assert_eq!(messages.len(), 2);
        assert!(notifier.was_message_sent("Hello").await);
    }

    #[tokio::test]
    async fn test_mock_notifier_records_events() {
        let notifier = MockNotifier::new();

        let event = ProcessEvent::Started {
            name: "test-app".to_string(),
            id: 1,
        };
        notifier.send_process_event(&event).await.unwrap();

        assert_eq!(notifier.call_count(), 1);
        assert!(notifier.was_event_type_sent("start").await);

        let events = notifier.events().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn test_mock_notifier_fails_when_configured() {
        let notifier = MockNotifier::failing();
        let result = notifier.send("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_notifier_event_types() {
        let notifier = MockNotifier::new();

        // Send different event types
        notifier
            .send_process_event(&ProcessEvent::Started {
                name: "app".to_string(),
                id: 1,
            })
            .await
            .unwrap();

        notifier
            .send_process_event(&ProcessEvent::Crashed {
                name: "app".to_string(),
                id: 1,
                error: "test error".to_string(),
            })
            .await
            .unwrap();

        notifier
            .send_process_event(&ProcessEvent::MemoryLimit {
                name: "app".to_string(),
                id: 1,
                memory_mb: 512,
                limit_mb: 256,
            })
            .await
            .unwrap();

        assert_eq!(notifier.call_count(), 3);
        assert!(notifier.was_event_type_sent("start").await);
        assert!(notifier.was_event_type_sent("crash").await);
        assert!(notifier.was_event_type_sent("memory_limit").await);
    }
}
