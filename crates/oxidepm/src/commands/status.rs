//! Status command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{is_json_mode, print_error, print_status_table, print_status_table_extended};

pub async fn execute(show_more: bool) -> Result<()> {
    let client = super::get_client();

    let response = super::send(&client, &Request::Status).await?;

    match response {
        Response::Status { apps } => {
            if show_more {
                print_status_table_extended(&apps);
            } else {
                print_status_table(&apps);
            }
            Ok(())
        }
        Response::Error { message, .. } => {
            print_error(&message);
            bail!(message)
        }
        _ => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
    }
}

/// `oxidepm status --daemon`: how the daemon itself is.
pub async fn execute_daemon() -> Result<()> {
    let client = super::get_client();

    let response = super::send(&client, &Request::DaemonStatus).await?;

    match response {
        Response::DaemonStatus {
            audit,
            audit_lost,
            agent_socket,
            adapters,
            warnings,
            sandbox,
        } => {
            if is_json_mode() {
                let json = serde_json::json!({
                    "audit": audit,
                    "audit_lost": audit_lost,
                    "agent_socket": agent_socket,
                    "adapters": adapters,
                    "warnings": warnings,
                    "sandbox": sandbox,
                });
                println!("{}", serde_json::to_string_pretty(&json)?);
                return Ok(());
            }
            println!("audit log:    {audit}");
            if audit_lost > 0 {
                println!("audit lines lost: {audit_lost}");
            }
            println!(
                "agent socket: {}",
                if agent_socket { "bound" } else { "not bound" }
            );
            if let Some(sandbox) = &sandbox {
                let launcher = if sandbox.launcher {
                    "launcher installed"
                } else {
                    "launcher NOT installed"
                };
                println!("sandbox:      {} ({launcher})", sandbox.summary);
            }
            for adapter in &adapters {
                match &adapter.note {
                    Some(note) => {
                        println!("adapter {}: {} ({})", adapter.name, adapter.state, note)
                    }
                    None => println!("adapter {}: {}", adapter.name, adapter.state),
                }
            }
            for warning in &warnings {
                println!("warning: {warning}");
            }
            Ok(())
        }
        Response::Error { message, .. } => {
            print_error(&message);
            bail!(message)
        }
        _ => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
    }
}
