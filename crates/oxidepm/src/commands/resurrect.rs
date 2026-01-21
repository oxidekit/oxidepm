//! Resurrect command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_info, print_success};

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    let response = client.send(&Request::Resurrect).await?;

    match response {
        Response::Resurrected { count } => {
            if count > 0 {
                print_success(&format!("Resurrected {} processes", count));
            } else {
                print_info("No saved processes to resurrect");
            }
            Ok(())
        }
        Response::Error { message } => {
            print_error(&message);
            bail!(message)
        }
        _ => {
            print_error("Unexpected response from daemon");
            bail!("Unexpected response")
        }
    }
}
