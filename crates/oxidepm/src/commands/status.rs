//! Status command implementation

use anyhow::{bail, Result};
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_status_table, print_status_table_extended};

pub async fn execute(show_more: bool) -> Result<()> {
    let client = super::get_client();

    let response = client.send(&Request::Status).await?;

    match response {
        Response::Status { apps } => {
            if show_more {
                print_status_table_extended(&apps);
            } else {
                print_status_table(&apps);
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
