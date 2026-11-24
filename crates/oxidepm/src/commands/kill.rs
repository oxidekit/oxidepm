//! Kill command implementation

use anyhow::{bail, Result};
use oxidepm_core::Error;
use oxidepm_ipc::{IpcClient, Request, Response};
use std::time::Duration;

use crate::output::{print_error, print_success};

/// How long to wait for the daemon to go away after a Kill request whose
/// reply we could not read.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    let sent = client
        .send_with(&Request::Kill, &super::call_options(false))
        .await;
    if let Ok(response) = &sent {
        if let Some(done) = super::plan_shown(response) {
            return done;
        }
    }
    match sent {
        Ok(Response::Ok { message }) => {
            print_success(&message);
            Ok(())
        }
        Ok(Response::Error { message, .. }) => {
            print_error(&message);
            bail!(message)
        }
        Ok(_) => {
            print_success("Daemon killed");
            Ok(())
        }
        Err(Error::DaemonNotRunning) => {
            print_success("Daemon is not running");
            Ok(())
        }
        // a rehearsal does not shut anything down: there is nothing to wait for
        Err(e) if super::is_dry_run() => bail!("{}", e),
        Err(e) => {
            // The daemon may close the connection while shutting down, so a
            // lost reply is only a success if the daemon is really gone.
            if wait_until_gone(&client).await {
                print_success("Daemon killed");
                Ok(())
            } else {
                print_error(&format!("Failed to kill daemon: {}", e));
                bail!("Daemon did not shut down: {}", e)
            }
        }
    }
}

async fn wait_until_gone(client: &IpcClient) -> bool {
    let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
    loop {
        if matches!(client.connect().await, Err(Error::DaemonNotRunning)) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
