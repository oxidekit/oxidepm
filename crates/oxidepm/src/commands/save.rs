//! Save command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_success};

pub async fn execute() -> Result<()> {
    let client = super::get_client();

    let response = client.send(&Request::Save).await?;

    match response {
        Response::Saved { count, path } => {
            print_success(&format!("Saved {} processes to {}", count, path));
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
