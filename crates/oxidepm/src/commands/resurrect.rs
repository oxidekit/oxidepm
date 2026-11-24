//! Resurrect command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{ItemOutcome, Request, Response};

use crate::output::{print_error, print_info, print_success};

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    let response = super::send_autostart(&client, &Request::Resurrect).await?;

    if let Some(done) = super::plan_shown(&response) {
        return done;
    }

    match response {
        Response::Resurrected {
            started, failed, ..
        } => {
            if started > 0 {
                print_success(&format!("Resurrected {} processes", started));
            } else if failed.is_empty() {
                print_info("No saved processes to resurrect");
            }
            for item in &failed {
                if let ItemOutcome::Err { message, .. } = &item.result {
                    print_error(&format!("{}: {}", item.name, message));
                }
            }
            if !failed.is_empty() {
                bail!("{} process(es) could not be resurrected", failed.len());
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
