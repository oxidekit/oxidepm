//! Cosmos node status command

use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};
use serde::Serialize;

use super::get_client;
use crate::output::is_json_mode;

#[derive(Serialize)]
struct CosmosStatusJson {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chain_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catching_up: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds_since_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upgrade_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upgrade_halt_height: Option<u64>,
}

pub async fn run(selector_str: &str) -> anyhow::Result<()> {
    let mut client = get_client();
    let selector = Selector::parse(selector_str);

    match client.send(&Request::CosmosStatus { selector }).await? {
        Response::CosmosStatus {
            name,
            lifecycle_state,
            catching_up,
            block_height,
            seconds_since_block,
            chain_id,
            node_mode,
            upgrade_name,
            upgrade_halt_height,
        } => {
            if is_json_mode() {
                let json = CosmosStatusJson {
                    name,
                    chain_id,
                    node_mode,
                    lifecycle_state,
                    block_height,
                    catching_up,
                    seconds_since_block,
                    upgrade_name,
                    upgrade_halt_height,
                };
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                println!("Cosmos Node: {}", name);
                if let Some(chain) = chain_id {
                    println!("  Chain ID:        {}", chain);
                }
                if let Some(mode) = node_mode {
                    println!("  Node Mode:       {}", mode);
                }
                if let Some(state) = lifecycle_state {
                    println!("  Lifecycle:       {}", state);
                }
                if let Some(height) = block_height {
                    println!("  Block Height:    {}", height);
                }
                if let Some(catching) = catching_up {
                    let status = if catching { "yes" } else { "no" };
                    println!("  Catching Up:     {}", status);
                }
                if let Some(secs) = seconds_since_block {
                    println!("  Since Block:     {}s", secs);
                }
                if let Some(ref upgrade) = upgrade_name {
                    println!("  UPGRADE HALTED:  '{}' at height {}",
                        upgrade,
                        upgrade_halt_height.unwrap_or(0)
                    );
                    println!("  Action:          Run: monarch upgrade --version {}", upgrade);
                }
            }
        }
        Response::Error { message } => {
            if is_json_mode() {
                println!("{}", serde_json::json!({"error": message}));
            } else {
                eprintln!("Error: {}", message);
                if !message.contains("Usage:") {
                    eprintln!("\nUsage: oxidepm cosmos-status <name>");
                    eprintln!("Example: oxidepm cosmos-status monod");
                }
            }
            std::process::exit(1);
        }
        _ => {
            if is_json_mode() {
                println!("{}", serde_json::json!({"error": "Unexpected response from daemon"}));
            } else {
                eprintln!("Unexpected response from daemon");
                eprintln!("\nUsage: oxidepm cosmos-status <name>");
                eprintln!("Example: oxidepm cosmos-status monod");
            }
            std::process::exit(1);
        }
    }

    Ok(())
}
