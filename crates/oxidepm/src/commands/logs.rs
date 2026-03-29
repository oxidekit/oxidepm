//! Logs command implementation

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::signal;

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
            selector: selector.clone(),
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
                // Resolve the app name from the selector to find the log file
                let app_name = resolve_app_name(&selector).await?;
                let log_path = if args.err && !args.out {
                    oxidepm_logs::stderr_path(&app_name)
                } else {
                    oxidepm_logs::stdout_path(&app_name)
                };

                follow_log_file(&log_path, grep_regex.as_ref()).await?;
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

/// Resolve a selector to an app name by querying the daemon
async fn resolve_app_name(selector: &Selector) -> Result<String> {
    let client = super::get_client();
    let response = client
        .send(&Request::Show {
            selector: selector.clone(),
        })
        .await?;

    match response {
        Response::Show { app } => Ok(app.spec.name),
        Response::Error { message } => bail!("Cannot resolve app: {}", message),
        _ => bail!("Unexpected response"),
    }
}

/// Tail a log file, printing new lines as they appear (like tail -f)
async fn follow_log_file(
    path: &std::path::Path,
    grep_regex: Option<&Regex>,
) -> Result<()> {
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        anyhow::anyhow!("Cannot follow log file {}: {}", path.display(), e)
    })?;

    let mut reader = BufReader::new(file);

    // Seek to end — we already printed the tail above
    reader.seek(std::io::SeekFrom::End(0)).await?;

    let mut line = String::new();

    loop {
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => {
                        // No new data — poll again after a short sleep
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if !trimmed.is_empty() {
                            if let Some(regex) = grep_regex {
                                if regex.is_match(trimmed) {
                                    println!("{}", trimmed);
                                }
                            } else {
                                println!("{}", trimmed);
                            }
                        }
                        line.clear();
                    }
                    Err(e) => {
                        eprintln!("Error reading log: {}", e);
                        break;
                    }
                }
            }
            _ = signal::ctrl_c() => {
                break;
            }
        }
    }

    Ok(())
}
