//! Flush command implementation - clears/truncates log files

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::output::{print_error, print_success};

pub async fn execute(selector: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector);

    let response = super::send(&client, &Request::Flush { selector }).await?;

    if let Some(done) = super::plan_shown(&response) {
        return done;
    }

    match response {
        Response::Flushed { count, .. } => {
            if count == 0 {
                print_error("No matching processes found");
            } else {
                print_success(&format!("Flushed logs for {} process(es)", count));
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
