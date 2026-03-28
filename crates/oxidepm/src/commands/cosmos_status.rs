//! Cosmos node status command

use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use super::get_client;

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
        Response::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}
