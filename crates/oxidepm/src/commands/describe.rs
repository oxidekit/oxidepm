//! Describe command implementation - shows what command would run without starting

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::output::print_error;

pub async fn execute(target: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(target);

    let response = client.send(&Request::Describe { selector }).await?;

    match response {
        Response::Described {
            name,
            command,
            args,
            cwd,
            env,
            mode,
        } => {
            println!("Process: {}", name);
            println!("Mode: {}", mode);
            println!("Working Directory: {}", cwd);
            println!();
            println!("Command: {}", command);
            if !args.is_empty() {
                println!("Arguments: {}", args.join(" "));
            }
            println!();
            println!("Full Command:");
            if args.is_empty() {
                println!("  {}", command);
            } else {
                println!("  {} {}", command, args.join(" "));
            }

            if !env.is_empty() {
                println!();
                println!("Environment Variables:");
                let mut sorted_env: Vec<_> = env.iter().collect();
                sorted_env.sort_by(|a, b| a.0.cmp(b.0));
                for (key, value) in sorted_env {
                    println!("  {}={}", key, value);
                }
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
