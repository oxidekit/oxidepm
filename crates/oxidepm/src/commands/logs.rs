//! Logs command implementation

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};
use regex::Regex;

use crate::cli::LogsArgs;
use crate::output::{print_error, print_logs};

pub async fn execute(args: LogsArgs) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(&args.selector);

    // Compile grep pattern if provided
    let grep_regex = if let Some(pattern) = &args.grep {
        Some(Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex pattern: {}", e))?)
    } else {
        None
    };

    let response = client
        .send(&Request::Logs {
            selector,
            lines: args.lines,
            follow: args.follow,
            stdout: args.out,
            stderr: args.err,
        })
        .await?;

    match response {
        Response::LogLines { lines } => {
            // Filter lines by grep pattern if provided
            let filtered_lines: Vec<String> = if let Some(ref regex) = grep_regex {
                lines.into_iter().filter(|line| regex.is_match(line)).collect()
            } else {
                lines
            };

            print_logs(&filtered_lines);

            if args.follow {
                // TODO: Implement follow mode with streaming
                println!("(follow mode not yet implemented)");
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
