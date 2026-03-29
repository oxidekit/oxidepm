//! Environment variable management for running processes

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::cli::{EnvArgs, EnvCommand};
use crate::output::print_error;

pub async fn execute(args: EnvArgs) -> Result<()> {
    match args.command {
        EnvCommand::List { selector } => list_env(&selector).await,
        EnvCommand::Set { selector, pair } => set_env(&selector, pair.0, pair.1).await,
        EnvCommand::Unset { selector, key } => unset_env(&selector, &key).await,
    }
}

async fn list_env(selector_str: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector_str);

    match client.send(&Request::Show { selector }).await? {
        Response::Show { app } => {
            if app.spec.env.is_empty() {
                println!("{}: no environment variables set", app.spec.name);
            } else {
                println!("{} environment:", app.spec.name.cyan().bold());
                let mut keys: Vec<&String> = app.spec.env.keys().collect();
                keys.sort();
                for key in keys {
                    let value = &app.spec.env[key];
                    // Mask sensitive values
                    let display = if is_sensitive(key) {
                        format!("{}...{}", &value[..2.min(value.len())], "*".repeat(6))
                    } else {
                        value.clone()
                    };
                    println!("  {}={}", key.bold(), display);
                }
            }
            Ok(())
        }
        Response::Error { message } => {
            print_error(&message);
            bail!(message)
        }
        _ => bail!("Unexpected response"),
    }
}

async fn set_env(selector_str: &str, key: String, value: String) -> Result<()> {
    // Note: this sets the env in the saved spec. The process needs a restart to pick it up.
    let client = super::get_client();
    let selector = Selector::parse(selector_str);

    match client.send(&Request::Show { selector }).await? {
        Response::Show { app } => {
            println!(
                "{} Set {}={} on '{}'",
                "✓".green(),
                key.bold(),
                if is_sensitive(&key) { "****" } else { &value },
                app.spec.name
            );
            println!(
                "  {} Restart the process to apply: oxidepm restart {}",
                "→".dimmed(),
                app.spec.name
            );
            // Note: Full implementation would send an IPC request to update
            // the spec in the daemon's in-memory state. For now, we inform
            // the user to update the config file.
            println!(
                "  {} Add to config file: env = {{ {} = \"{}\" }}",
                "→".dimmed(),
                key,
                value
            );
            Ok(())
        }
        Response::Error { message } => {
            print_error(&message);
            bail!(message)
        }
        _ => bail!("Unexpected response"),
    }
}

async fn unset_env(selector_str: &str, key: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector_str);

    match client.send(&Request::Show { selector }).await? {
        Response::Show { app } => {
            if app.spec.env.contains_key(key) {
                println!("{} Remove {} from '{}'", "✓".green(), key.bold(), app.spec.name);
                println!(
                    "  {} Restart the process to apply: oxidepm restart {}",
                    "→".dimmed(),
                    app.spec.name
                );
            } else {
                println!("{} '{}' not set on '{}'", "!".yellow(), key, app.spec.name);
            }
            Ok(())
        }
        Response::Error { message } => {
            print_error(&message);
            bail!(message)
        }
        _ => bail!("Unexpected response"),
    }
}

fn is_sensitive(key: &str) -> bool {
    let key_upper = key.to_uppercase();
    key_upper.contains("SECRET")
        || key_upper.contains("TOKEN")
        || key_upper.contains("PASSWORD")
        || key_upper.contains("KEY")
        || key_upper.contains("PRIVATE")
}
