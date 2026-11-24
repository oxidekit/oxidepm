//! Ping command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_success};

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    match client.send(&Request::Ping).await {
        Ok(Response::Pong) => {
            print_success("Daemon is alive");
            Ok(())
        }
        Ok(Response::Error { message, .. }) => {
            print_error(&message);
            bail!(message)
        }
        Ok(_) => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
        Err(oxidepm_core::Error::DaemonNotRunning) => {
            print_error("Daemon is not running");
            bail!("Daemon not running")
        }
        Err(e) => {
            print_error(&format!("Could not reach the daemon: {}", e));
            bail!("Could not reach the daemon: {}", e)
        }
    }
}
