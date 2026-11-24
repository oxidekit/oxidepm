//! Relay commands such as `oxidepm check` to the adapters that provide them.
//!
//! When clap rejects an unknown subcommand, the CLI checks the daemon's verb table
//! (`adapters/verbs.json`) without contacting it. A blanket
//! `allow_external_subcommands` would switch off clap's "did you mean" suggestion and would
//! start a daemon for a typo; this never starts one. The table only chooses what to ask for: the
//! daemon resolves the verb again, checks the policy, runs the adapter in its sandbox and streams
//! the output back. The CLI never executes an adapter binary. A verb that needs the terminal is
//! a session (`session.rs`).

use anyhow::Context;
use oxidepm_core::runtime_hints::VERB_ADAPTER;
use oxidepm_core::{AppSpec, Error, ErrorCode, Paths};
use oxidepm_ipc::{AdapterRequest, CallOptions, InvokeArgs, Request, Response, VerbTable};
use serde::Deserialize;
use std::io::Write;
use std::path::Path;

/// What the command line of an unknown command holds.
#[derive(Debug, PartialEq, Eq)]
pub struct VerbLine {
    pub verb: String,
    pub argv: Vec<String>,
    pub json: bool,
    pub dry_run: bool,
}

/// Split the arguments (without the program name) into the global flags, the verb and what
/// follows it. `--json` and `--dry-run` are global flags and count wherever they stand, until a
/// literal `--`; `-v` flags are dropped. `None` when no verb is named.
pub fn split_line(args: &[String]) -> Option<VerbLine> {
    let (mut json, mut dry_run) = (false, false);
    let mut verb: Option<String> = None;
    let mut argv = Vec::new();
    let mut literal = false;
    for arg in args {
        if literal {
            argv.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--json" => json = true,
            "--dry-run" => dry_run = true,
            "--" if verb.is_some() => {
                literal = true;
                argv.push(arg.clone());
            }
            a if verb.is_none() && a.starts_with('-') => {
                // -v, -vv, -vvv: not part of the verb's arguments
            }
            a if verb.is_none() => verb = Some(a.to_string()),
            a => argv.push(a.to_string()),
        }
    }
    verb.map(|verb| VerbLine {
        verb,
        argv,
        json,
        dry_run,
    })
}

/// The verb table the daemon wrote, if there is one that can be read.
fn read_table() -> Option<VerbTable> {
    let paths = Paths::from_env().ok()?;
    let meta = std::fs::symlink_metadata(paths.adapter_verbs()).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(paths.adapter_verbs()).ok()?).ok()
}

/// The words printed after clap's own message for a command nobody provides: the official
/// adapter that would provide it, and how to get it.
pub fn missing_hint(verb: &str) -> Option<String> {
    VERB_ADAPTER.iter().find(|(v, _)| *v == verb).map(|(_, adapter)| {
        format!(
            "'{verb}' is provided by the adapter '{adapter}', which is not installed.\nInstall it with: oxidepm adapter import {adapter}   (see: oxidepm adapter list --available)"
        )
    })
}

/// Clap did not know the command. Run it as an adapter verb if the table has it; otherwise show
/// clap's error (with its suggestion) and, for a command an official adapter provides, how to
/// install that adapter. Never returns.
pub async fn fallback(error: clap::Error) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(line) = split_line(&args) else {
        error.exit()
    };
    let Some(entry) = find_verb(&line.verb) else {
        let _ = error.print();
        if let Some(hint) = missing_hint(&line.verb) {
            eprintln!("\n{hint}");
        }
        std::process::exit(2);
    };
    crate::output::set_json_mode(line.json);
    super::set_dry_run(line.dry_run);
    // a verb that needs the terminal is run by the daemon on a terminal this process hands over
    let result = if entry.mode == "session" {
        super::session::run(&line.verb, &line.argv, line.dry_run).await
    } else {
        relay(&line, &entry.adapter).await
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn find_verb(verb: &str) -> Option<oxidepm_ipc::VerbTableEntry> {
    read_table()?
        .verbs
        .into_iter()
        .find(|entry| entry.verb == verb)
}

#[derive(serde::Serialize)]
pub(crate) struct InvokeDone {
    pub exit_code: i32,
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<Response>,
}

async fn relay(line: &VerbLine, adapter: &str) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?.display().to_string();
    let mut done = invoke_verb(&line.verb, line.argv.clone(), cwd, line.dry_run, line.json)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(missing_hint(&line.verb)
                .unwrap_or_else(|| format!("No enabled adapter provides '{}'", line.verb)))
        })?;
    if line.verb == "git-start" && adapter == "git-start" && done.exit_code == 0 && !line.dry_run {
        if let Some(result) = done
            .result
            .as_ref()
            .filter(|result| result["auto_start"].as_bool() != Some(false))
        {
            // Use the CLI's connection so the start goes through the usual ingress checks.
            let response =
                if let Some(config) = result.get("config_file").and_then(|path| path.as_str()) {
                    let specs = super::start::load_config_specs(Path::new(config))?;
                    Some(super::start::start_batch(specs).await?)
                } else if let Some(spec) = result.get("suggested_spec") {
                    let spec =
                        AppSpec::deserialize(spec).context("Invalid launch spec from git-start")?;
                    Some(super::start::start_spec(spec).await?)
                } else {
                    None
                };
            if let Some(response) = response {
                done.exit_code = report_start(&response, line.json)?;
                done.start = Some(response);
            }
        }
    }
    if line.json {
        println!("{}", serde_json::to_string_pretty(&done)?);
    }
    Ok(done.exit_code)
}

fn report_start(response: &Response, json: bool) -> anyhow::Result<i32> {
    use crate::output::{print_error, print_success};

    match response {
        Response::Started { id, name, .. } => {
            if !json {
                print_success(&format!("Started {name} (id: {id})"));
            }
            Ok(0)
        }
        Response::StartedBatch { items } => {
            let (_, failed) = super::start::report_batch(items, !json);
            Ok(i32::from(!failed.is_empty()))
        }
        Response::Error { message, .. } => {
            if !json {
                print_error(message);
            }
            Ok(1)
        }
        _ => anyhow::bail!("Unexpected start response from the daemon"),
    }
}

/// Ask the daemon to run the verb and relay its output. A missing adapter returns `None`.
pub(crate) async fn invoke_verb(
    verb: &str,
    argv: Vec<String>,
    cwd: String,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<Option<InvokeDone>> {
    let request = Request::Adapter(Box::new(AdapterRequest::Invoke(Box::new(InvokeArgs {
        verb: verb.to_string(),
        argv,
        cwd,
    }))));
    let client = super::get_client();
    let options = CallOptions {
        dry_run,
        ..Default::default()
    };
    let response = client
        .send_streaming(&request, &options, |part| {
            if let Response::InvokeOutput { stream, text } = part {
                if !json {
                    show(stream, text);
                }
            }
        })
        .await;
    match response {
        Err(Error::DaemonNotRunning) if find_verb(verb).is_none() => Ok(None),
        Err(Error::DaemonNotRunning) => anyhow::bail!(
            "Daemon is not running (start it with `oxidepm resurrect`, or start a process); verbs of adapters are run by the daemon"
        ),
        Err(error) => Err(error.into()),
        Ok(Response::InvokeDone { exit_code, result }) => Ok(Some(InvokeDone { exit_code, result, start: None })),
        Ok(Response::Error { code: ErrorCode::AdapterMissing, .. }) => Ok(None),
        Ok(Response::Error { code, message }) => Err(super::adapter::coded(code.as_str(), message)),
        Ok(_) => anyhow::bail!("Unexpected response from the daemon"),
    }
}

/// One piece of output on the terminal. What an adapter wrote is not trusted with the terminal:
/// control characters other than newline and tab are replaced.
fn show(stream: &str, text: &str) {
    let clean: String = text
        .chars()
        .map(|c| {
            if c == '\n' || c == '\t' || !c.is_control() {
                c
            } else {
                '\u{FFFD}'
            }
        })
        .collect();
    let line = if clean.ends_with('\n') {
        clean
    } else {
        format!("{clean}\n")
    };
    if stream == "stderr" || stream == "status" {
        let _ = std::io::stderr().write_all(line.as_bytes());
    } else {
        let _ = std::io::stdout().write_all(line.as_bytes());
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> Option<VerbLine> {
        split_line(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_verb_is_the_first_word_and_global_flags_are_taken_out() {
        let l = line(&["-v", "--json", "check", "--fix", "src", "--dry-run"]).unwrap();
        assert_eq!(
            l,
            VerbLine {
                verb: "check".into(),
                argv: vec!["--fix".into(), "src".into()],
                json: true,
                dry_run: true
            }
        );
        assert!(line(&["--json"]).is_none());
        assert!(line(&[]).is_none());
    }

    #[test]
    fn after_a_double_dash_everything_belongs_to_the_verb() {
        let l = line(&["deploy", "--", "--json", "--dry-run"]).unwrap();
        assert_eq!(l.argv, vec!["--", "--json", "--dry-run"]);
        assert!(!l.json && !l.dry_run);
    }

    #[test]
    fn only_commands_an_official_adapter_provides_get_an_install_hint() {
        assert!(missing_hint("git-start")
            .unwrap()
            .contains("oxidepm adapter import git-start"));
        assert!(missing_hint("nonesuch").is_none());
    }
}
