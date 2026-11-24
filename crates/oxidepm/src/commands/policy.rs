//! `oxidepm policy set`: change the markers of apps without restarting them.

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{ItemOutcome, Request, Response};

use crate::output::{is_json_mode, print_error, print_success};

pub async fn execute_set(
    selector: &str,
    protected: Option<bool>,
    ai_control: Option<bool>,
) -> Result<()> {
    let client = super::get_client();
    let request = Request::SetPolicy {
        selector: Selector::parse(selector),
        protected,
        ai_control,
    };

    let response = super::send(&client, &request).await?;

    if let Some(done) = super::plan_shown(&response) {
        return done;
    }

    match response {
        Response::PolicySet { count, items } => {
            if is_json_mode() {
                let rows: Vec<serde_json::Value> = items
                    .iter()
                    .map(|item| match &item.result {
                        ItemOutcome::Ok { changed, detail } => serde_json::json!({
                            "id": item.id, "name": item.name, "changed": changed, "markers": detail,
                        }),
                        ItemOutcome::Err { code, message } => serde_json::json!({
                            "id": item.id, "name": item.name, "error": code.as_str(), "message": message,
                        }),
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if items.is_empty() {
                print_success("No processes to change");
                return Ok(());
            }
            for item in &items {
                if let ItemOutcome::Ok { changed, detail } = &item.result {
                    let now = detail.as_deref().unwrap_or("");
                    if *changed {
                        print_success(&format!("{}: {}", item.name, now));
                    } else {
                        print_success(&format!("{}: {} (unchanged)", item.name, now));
                    }
                }
            }
            if count == 0 {
                print_success("Nothing needed changing");
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
