//! Notification configuration command

use anyhow::{bail, Result};
use oxidepm_notify::{NotificationManager, NotifyConfig, ProcessEvent};

use crate::cli::{NotifyArgs, NotifyCommand};
use crate::output::{print_error, print_info, print_success};

pub async fn execute(args: NotifyArgs) -> Result<()> {
    match args.command {
        NotifyCommand::Telegram { token, chat } => configure_telegram(token, chat).await,
        NotifyCommand::Remove { channel } => remove_channel(&channel).await,
        NotifyCommand::Events { set } => set_events(&set).await,
        NotifyCommand::Status => show_status().await,
        NotifyCommand::Test => test_notification().await,
    }
}

async fn configure_telegram(token: String, chat: String) -> Result<()> {
    let mut config = NotifyConfig::load().unwrap_or_default();
    config.set_telegram(token, chat);
    config.save()?;

    print_success("Telegram notifications configured successfully");
    print_info(&format!(
        "Config saved to: {}",
        oxidepm_notify::config::notify_config_path().display()
    ));

    Ok(())
}

async fn remove_channel(channel: &str) -> Result<()> {
    let mut config = NotifyConfig::load().unwrap_or_default();

    match channel.to_lowercase().as_str() {
        "telegram" => {
            config.remove_telegram();
            config.save()?;
            print_success("Telegram notifications removed");
        }
        _ => {
            print_error(&format!("Unknown notification channel: {}", channel));
            bail!("Unknown channel: {}", channel);
        }
    }

    Ok(())
}

async fn set_events(events_str: &str) -> Result<()> {
    let mut config = NotifyConfig::load().unwrap_or_default();

    let events: Vec<String> = events_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if events.is_empty() {
        print_info("Clearing event filter (will notify on all events)");
    }

    config.set_events(events.clone());

    // Validate events
    if let Err(e) = config.validate_events() {
        print_error(&format!("{}", e));
        bail!(e);
    }

    config.save()?;

    if events.is_empty() {
        print_success("Event filter cleared - will notify on all events");
    } else {
        print_success(&format!("Events set to: {}", events.join(", ")));
    }

    Ok(())
}

async fn show_status() -> Result<()> {
    let config = NotifyConfig::load().unwrap_or_default();

    println!();
    println!("Notification Configuration");
    println!("{}", "=".repeat(40));

    // Telegram status
    if let Some(ref telegram) = config.telegram {
        println!("Telegram: configured");
        println!("  Chat ID: {}", telegram.chat_id);
        println!(
            "  Bot Token: {}...{}",
            &telegram.bot_token[..8.min(telegram.bot_token.len())],
            &telegram.bot_token[telegram.bot_token.len().saturating_sub(4)..]
        );
    } else {
        println!("Telegram: not configured");
    }

    // Events filter
    println!();
    if config.events.is_empty() {
        println!("Events: all (no filter)");
    } else {
        println!("Events: {}", config.events.join(", "));
    }

    // Config file location
    println!();
    println!(
        "Config file: {}",
        oxidepm_notify::config::notify_config_path().display()
    );

    Ok(())
}

async fn test_notification() -> Result<()> {
    let config = NotifyConfig::load().unwrap_or_default();

    if !config.is_configured() {
        print_error("No notification channels configured");
        print_info("Run 'oxidepm notify telegram --token <TOKEN> --chat <CHAT_ID>' to configure");
        bail!("Not configured");
    }

    let manager = NotificationManager::new(config);

    print_info("Sending test notification...");

    // Send a test event
    let test_event = ProcessEvent::Started {
        name: "test-process".to_string(),
        id: 0,
    };

    match manager.notify(&test_event).await {
        Ok(_) => {
            print_success("Test notification sent successfully!");
            Ok(())
        }
        Err(e) => {
            print_error(&format!("Failed to send test notification: {}", e));
            bail!("Test notification failed: {}", e);
        }
    }
}
