//! `oxidepm audit`: read the audit log.
//!
//! The files are read directly, like `logs` does, so the command works with the daemon down.
//! Every string that came from an actor goes through the terminal sanitizer before it is printed.

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use oxidepm_core::constants::{AUDIT_LOG_FILE, AUDIT_SECURITY_FILE};
use oxidepm_core::Paths;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use crate::cli::{parse_duration, AuditArgs};
use crate::output::{is_json_mode, print_info, sanitize_terminal};

/// Files kept per log by the daemon, the live one included.
const MAX_FILES: usize = 10;

/// What a line must match to be shown.
#[derive(Debug, Default)]
pub struct Filter {
    /// `class:label`, `class` or `label`.
    pub actor: Option<String>,
    pub op: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

impl Filter {
    /// Does a decision line match the actor and operation filters?
    fn matches_request(&self, line: &Value) -> bool {
        if let Some(op) = &self.op {
            if line["op"].as_str() != Some(op.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.actor {
            let class = line["actor"]["class"].as_str().unwrap_or("");
            let label = line["actor"]["label"].as_str().unwrap_or("");
            let hit = match wanted.split_once(':') {
                Some((c, l)) => c == class && l == label,
                None => wanted == class || wanted == label,
            };
            if !hit {
                return false;
            }
        }
        true
    }

    fn is_open(&self) -> bool {
        self.actor.is_none() && self.op.is_none()
    }

    fn since_ok(&self, line: &Value) -> bool {
        let Some(since) = self.since else { return true };
        line["ts"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t >= since)
    }
}

/// The time `--since` means: a length of time ago (`30m`) or an RFC 3339 time.
pub fn parse_since(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text.trim()) {
        return Ok(t.with_timezone(&Utc));
    }
    match parse_duration(text) {
        Ok(secs) => Ok(now - Duration::seconds(secs.min(i64::MAX as u64 / 2000) as i64)),
        Err(_) => bail!("--since wants a length of time such as 30m, 2h or 1d, or an RFC 3339 time, not '{text}'"),
    }
}

/// Every parsable line of the series of `file` (`audit.jsonl`, `audit.1.jsonl`, ...), oldest
/// first. A line that does not parse (a torn last line) is skipped.
pub fn read_series(dir: &Path, file: &str) -> Vec<Value> {
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, ""));
    let mut names: Vec<String> = (1..MAX_FILES)
        .rev()
        .map(|i| format!("{stem}.{i}.{ext}"))
        .collect();
    names.push(file.to_string());
    let mut out = Vec::new();
    for name in names {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            out.extend(
                text.lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok()),
            );
        }
    }
    out
}

/// The lines to show: those that pass the filters, the result lines of the requests that do, and
/// at most the newest `tail`.
pub fn select(lines: Vec<Value>, filter: &Filter, tail: usize) -> Vec<Value> {
    let wanted: HashSet<String> = if filter.is_open() {
        HashSet::new()
    } else {
        lines
            .iter()
            .filter(|l| l["actor"].is_object() && filter.matches_request(l))
            .filter_map(|l| l["req"].as_str().map(str::to_string))
            .collect()
    };
    let mut kept: Vec<Value> = lines
        .into_iter()
        .filter(|l| {
            if !filter.since_ok(l) {
                return false;
            }
            if filter.is_open() {
                return true;
            }
            // a line of a request that matched (its result), or a decision that matches itself
            if l["actor"].is_object() {
                return filter.matches_request(l);
            }
            l["req"].as_str().is_some_and(|r| wanted.contains(r))
        })
        .collect();
    if kept.len() > tail {
        kept.drain(..kept.len() - tail);
    }
    kept
}

/// One line of text for a record. Everything an actor could have written is sanitized.
pub fn format_line(line: &Value) -> String {
    let text = |v: &Value| sanitize_terminal(v.as_str().unwrap_or(""));
    let ts = text(&line["ts"]);
    let seq = line["seq"].as_u64().unwrap_or(0);
    let head = format!("{ts} #{seq:<6}");
    let actor = if line["actor"].is_object() {
        format!(
            "{}:{}",
            text(&line["actor"]["class"]),
            text(&line["actor"]["label"])
        )
    } else {
        String::new()
    };
    let target = if let Some(targets) = line["targets"].as_array() {
        let names: Vec<String> = targets.iter().map(|t| text(&t["name"])).collect();
        let more = line["targets_more"]
            .as_u64()
            .map(|n| format!(" (+{n} more)"))
            .unwrap_or_default();
        format!("{}{}", names.join(","), more)
    } else {
        text(&line["selector"])
    };
    match line["phase"].as_str().unwrap_or("") {
        "decision" => {
            let verdict = match line["decision"].as_str() {
                Some("deny") => format!(
                    "DENIED {}: {}",
                    text(&line["deny"]["code"]),
                    text(&line["deny"]["why"])
                ),
                _ => "allow".to_string(),
            };
            let mut out = format!(
                "{head} {actor} {} [{}] {verdict}",
                text(&line["op"]),
                text(&line["tier"])
            );
            if !target.is_empty() {
                out.push_str(&format!(" {target}"));
            }
            if let Some(count) = line["count"].as_u64() {
                out.push_str(&format!(
                    " x{count} ({} .. {})",
                    text(&line["first"]),
                    text(&line["last"])
                ));
            }
            if line["dry_run"].as_bool() == Some(true) {
                out.push_str(" (dry run)");
            }
            if let Some(program) = line["program"].as_str() {
                out.push_str(&format!(
                    " program={} argc={}",
                    sanitize_terminal(program),
                    line["argc"].as_u64().unwrap_or(0)
                ));
            }
            out
        }
        "result" => {
            let ok = if line["ok"].as_bool() == Some(true) {
                "ok"
            } else {
                "FAILED"
            };
            let code = line["code"]
                .as_str()
                .map(|c| format!(" {}", sanitize_terminal(c)))
                .unwrap_or_default();
            format!(
                "{head} result {} {ok}{code} changed={} failed={} {}ms",
                text(&line["req"]),
                line["changed"].as_u64().unwrap_or(0),
                line["failed"].as_u64().unwrap_or(0),
                line["ms"].as_u64().unwrap_or(0)
            )
        }
        "rate_limited" => format!(
            "{head} {actor} rate limited x{} ({} .. {})",
            line["count"].as_u64().unwrap_or(0),
            text(&line["first"]),
            text(&line["last"])
        ),
        "note" => {
            let name = text(&line["adapter"]["name"]);
            let dropped = line["dropped"]
                .as_u64()
                .map(|n| format!(" {n} line(s) could not be written"))
                .unwrap_or_default();
            format!("{head} note {name} {}{dropped}", text(&line["text"]))
        }
        "adapter" => format!(
            "{head} adapter {} {} {} {}",
            text(&line["adapter"]["name"]),
            text(&line["adapter"]["event"]),
            text(&line["adapter"]["version"]),
            text(&line["adapter"]["sha12"])
        ),
        "plan" => format!(
            "{head} plan {} program={} argc={}",
            text(&line["req"]),
            text(&line["program"]),
            line["argc"].as_u64().unwrap_or(0)
        ),
        other => format!("{head} {}", sanitize_terminal(other)),
    }
}

pub fn execute(args: AuditArgs) -> Result<()> {
    let filter = Filter {
        actor: args.actor.filter(|a| !a.is_empty()),
        op: args.op.filter(|o| !o.is_empty()),
        since: args
            .since
            .as_deref()
            .map(|s| parse_since(s, Utc::now()))
            .transpose()?,
    };
    let paths = Paths::from_env()?;
    let file = if args.security {
        AUDIT_SECURITY_FILE
    } else {
        AUDIT_LOG_FILE
    };
    let lines = read_series(&paths.audit_dir(), file);
    let shown = select(lines, &filter, args.tail);

    if is_json_mode() {
        // one object per line; JSON escapes control characters, so this is safe on a terminal
        for line in &shown {
            println!("{}", serde_json::to_string(line)?);
        }
        return Ok(());
    }
    if shown.is_empty() {
        print_info(&format!(
            "No audit records in {}",
            paths.audit_dir().join(file).display()
        ));
        return Ok(());
    }
    for line in &shown {
        println!("{}", format_line(line));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn since_takes_a_length_of_time_or_a_time() {
        let now = DateTime::parse_from_rfc3339("2026-11-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_since("30m", now).unwrap().to_rfc3339(),
            "2026-11-02T11:30:00+00:00"
        );
        assert_eq!(
            parse_since("1d", now).unwrap().to_rfc3339(),
            "2026-11-01T12:00:00+00:00"
        );
        assert_eq!(
            parse_since("2026-11-02T08:00:00Z", now)
                .unwrap()
                .to_rfc3339(),
            "2026-11-02T08:00:00+00:00"
        );
        assert!(parse_since("yesterday", now).is_err());
    }

    #[test]
    fn a_filter_keeps_the_result_of_a_request_it_keeps() {
        let lines = vec![
            json!({"seq":1,"ts":"2026-11-02T10:00:00.000Z","phase":"decision","req":"r1","op":"stop","actor":{"class":"ai","label":"a"}}),
            json!({"seq":2,"ts":"2026-11-02T10:00:00.100Z","phase":"result","req":"r1","ok":true}),
            json!({"seq":3,"ts":"2026-11-02T10:00:01.000Z","phase":"decision","req":"r2","op":"stop","actor":{"class":"human","label":"cli"}}),
            json!({"seq":4,"ts":"2026-11-02T10:00:01.100Z","phase":"result","req":"r2","ok":true}),
        ];
        let filter = Filter {
            actor: Some("ai".into()),
            ..Filter::default()
        };
        let kept = select(lines.clone(), &filter, 10);
        let seqs: Vec<u64> = kept.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [1, 2]);
        let kept = select(lines.clone(), &Filter::default(), 3);
        let seqs: Vec<u64> = kept.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [2, 3, 4], "the newest three");
        let filter = Filter {
            actor: Some("human:cli".into()),
            op: Some("stop".into()),
            ..Filter::default()
        };
        assert_eq!(select(lines, &filter, 10).len(), 2);
    }
}
