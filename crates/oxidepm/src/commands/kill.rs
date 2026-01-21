//! Kill command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_success};

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    match client.send(&Request::Kill).await {
        Ok(Response::Ok { message }) => {
            print_success(&message);
            Ok(())
        }
        Ok(Response::Error { message }) => {
            print_error(&message);
            bail!(message)
        }
        Ok(_) => {
            print_success("Daemon killed");
            Ok(())
        }
        Err(e) => {
            // Connection closed is expected when daemon is killed
            if e.to_string().contains("DaemonNotRunning") {
                print_success("Daemon is not running");
                Ok(())
            } else {
                print_success("Daemon killed");
                Ok(())
            }
        }
    }
}
