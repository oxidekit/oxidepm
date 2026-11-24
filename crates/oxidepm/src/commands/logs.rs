//! Logs command implementation

use anyhow::{bail, Result};
use oxidepm_core::Selector;
use oxidepm_ipc::{Request, Response};
use oxidepm_logs::{tail_streams, LogFollower, LogStream};
use regex::Regex;
use std::io::Write;
use tokio::signal;

use crate::cli::LogsArgs;
use crate::output::{is_json_mode, print_error, print_logs};

pub async fn execute(args: LogsArgs) -> Result<()> {
    let selector = Selector::parse(&args.selector);
    if matches!(selector, Selector::All) {
        print_error("Cannot get logs for 'all'");
        bail!("Cannot get logs for 'all'");
    }

    // Compile grep pattern if provided
    let grep_regex = if let Some(pattern) = &args.grep {
        Some(Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex pattern: {}", e))?)
    } else {
        None
    };

    let app_name = match resolve_app_name(&selector).await {
        Ok(name) => name,
        Err(e) => {
            print_error(&e.to_string());
            return Err(e);
        }
    };

    let streams = selected_streams(&app_name, args.out, args.err);

    // Start following before reading the tail so no line falls in between
    // (a line written in that gap may be shown twice, never lost)
    let mut followers: Vec<(LogStream, LogFollower)> = if args.follow {
        streams
            .iter()
            .map(|(stream, path)| (*stream, LogFollower::new(path.clone())))
            .collect()
    } else {
        Vec::new()
    };

    // grep runs before the --lines cut
    let keep = grep_regex
        .as_ref()
        .map(|re| move |line: &str| re.is_match(line));
    let entries = tail_streams(
        &streams,
        args.lines,
        keep.as_ref().map(|f| f as &dyn Fn(&str) -> bool),
    )?;

    if !args.follow {
        let lines: Vec<String> = entries.into_iter().map(|e| e.line).collect();
        print_logs(&lines);
        return Ok(());
    }

    let json = is_json_mode();
    for entry in &entries {
        // Same rule as for followed lines: trimmed, blank lines skipped
        let Some(line) = visible(&entry.line) else {
            continue;
        };
        if !emit(entry.stream, line, json) {
            return Ok(());
        }
    }
    follow_logs(&mut followers, grep_regex.as_ref(), json).await
}

/// Log files to read: stdout unless only --err, stderr unless only --out
fn selected_streams(app_name: &str, out: bool, err: bool) -> Vec<(LogStream, std::path::PathBuf)> {
    let mut streams = Vec::new();
    if out || !err {
        streams.push((LogStream::Out, oxidepm_logs::stdout_path(app_name)));
    }
    if err || !out {
        streams.push((LogStream::Err, oxidepm_logs::stderr_path(app_name)));
    }
    streams
}

/// Resolve a selector to an app name by querying the daemon
async fn resolve_app_name(selector: &Selector) -> Result<String> {
    let client = super::get_client();
    let response = client
        .send_autostart(&Request::Show {
            selector: selector.clone(),
        })
        .await?;

    match response {
        Response::Show { app } => Ok(app.spec.name),
        Response::Error { message, .. } => bail!("Cannot resolve app: {}", message),
        _ => bail!("Unexpected response"),
    }
}

/// A line as shown while following: trailing whitespace removed, blank lines skipped
fn visible(line: &str) -> Option<&str> {
    let line = line.trim_end();
    (!line.is_empty()).then_some(line)
}

/// One NDJSON record for `--json` follow output
fn json_line(stream: LogStream, line: &str) -> String {
    serde_json::json!({ "stream": stream.as_str(), "line": line }).to_string()
}

/// Print one followed line; returns false when stdout is closed
fn emit(stream: LogStream, line: &str, json: bool) -> bool {
    let mut stdout = std::io::stdout().lock();
    let result = if json {
        writeln!(stdout, "{}", json_line(stream, line))
    } else {
        writeln!(stdout, "{}", line)
    };
    result.is_ok()
}

/// Print new lines as they appear (like tail -f); survives rotation and flush
async fn follow_logs(
    followers: &mut [(LogStream, LogFollower)],
    grep_regex: Option<&Regex>,
    json: bool,
) -> Result<()> {
    // Last poll error shown per follower: a persistent error is reported once,
    // not on every poll
    let mut last_error: Vec<Option<String>> = vec![None; followers.len()];
    loop {
        for ((stream, follower), last) in followers.iter_mut().zip(last_error.iter_mut()) {
            let lines = match follower.poll() {
                Ok(lines) => {
                    *last = None;
                    lines
                }
                Err(e) => {
                    let message = e.to_string();
                    if last.as_deref() != Some(message.as_str()) {
                        eprintln!("Error reading log: {}", message);
                        *last = Some(message);
                    }
                    continue;
                }
            };
            for line in lines {
                let Some(line) = visible(&line) else {
                    continue;
                };
                if let Some(regex) = grep_regex {
                    if !regex.is_match(line) {
                        continue;
                    }
                }
                if !emit(*stream, line, json) {
                    return Ok(());
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = signal::ctrl_c() => {
                // Flush any remaining output and exit cleanly
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_follows_both_streams() {
        let s = selected_streams("app", false, false);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, LogStream::Out);
        assert_eq!(s[1].0, LogStream::Err);
    }

    #[test]
    fn out_or_err_flag_selects_one_stream() {
        let s = selected_streams("app", true, false);
        assert_eq!(
            s.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![LogStream::Out]
        );
        let s = selected_streams("app", false, true);
        assert_eq!(
            s.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![LogStream::Err]
        );
        assert_eq!(selected_streams("app", true, true).len(), 2);
    }

    #[test]
    fn visible_trims_and_skips_blank_lines() {
        assert_eq!(visible("abc  \t"), Some("abc"));
        assert_eq!(visible("  \r"), None);
        assert_eq!(visible(""), None);
    }

    #[test]
    fn json_follow_line_is_one_object() {
        let line = json_line(LogStream::Err, "he said \"hi\"\tok");
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["stream"], "err");
        assert_eq!(v["line"], "he said \"hi\"\tok");
    }
}
