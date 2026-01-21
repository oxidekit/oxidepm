//! Delete command implementation

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_success};

pub async fn execute(selector: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector);

    let response = client.send(&Request::Delete { selector }).await?;

    match response {
        Response::Deleted { count } => {
            if count > 0 {
                print_success(&format!("Deleted {} process(es)", count));
            } else {
                print_success("No processes to delete");
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
