//! Environment variable management for running processes

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};

use crate::cli::{EnvArgs, EnvCommand};
use crate::output::print_error;

pub async fn execute(args: EnvArgs) -> Result<()> {
    match args.command {
        EnvCommand::List { selector } => list_env(&selector).await,
        EnvCommand::Set { selector, pair } => set_env(&selector, pair.0, pair.1).await,
        EnvCommand::Unset { selector, key } => unset_env(&selector, &key).await,
    }
}

async fn list_env(selector_str: &str) -> Result<()> {
    let client = super::get_client();
    let selector = Selector::parse(selector_str);

    match super::send(&client, &Request::Show { selector }).await? {
        Response::Show { app } => {
            if app.spec.env.is_empty() {
                println!("{}: no environment variables set", app.spec.name);
            } else {
                println!("{} environment:", app.spec.name.cyan().bold());
                let mut keys: Vec<&String> = app.spec.env.keys().collect();
                keys.sort();
                for key in keys {
                    // The daemon already replaced every value with a marker.
                    println!("  {}={}", key.bold(), app.spec.env[key]);
                }
            }
            Ok(())
        }
        Response::Error { message, .. } => {
            print_error(&message);
            bail!(message)
        }
        _ => bail!("Unexpected response"),
    }
}

/// The daemon has no request for changing a spec's environment yet, so these
/// commands refuse instead of reporting a change that never happened. The
/// value is deliberately never echoed.
async fn set_env(_selector: &str, _key: String, _value: String) -> Result<()> {
    bail!(
        "`env set` is not supported yet: the daemon cannot persist environment changes. \
         Pass --env KEY=VALUE when starting the process, or set `env` in the config file, \
         then restart"
    )
}

async fn unset_env(_selector: &str, _key: &str) -> Result<()> {
    bail!(
        "`env unset` is not supported yet: the daemon cannot persist environment changes. \
         Remove the variable from the config file (or start the process without it), then restart"
    )
}
