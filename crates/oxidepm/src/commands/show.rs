//! Show command implementation

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::output::{print_app_detail, print_error};

pub async fn execute(selector: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector);

    let response = client.send(&Request::Show { selector }).await?;

    match response {
        Response::Show { app } => {
            print_app_detail(&app);
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
