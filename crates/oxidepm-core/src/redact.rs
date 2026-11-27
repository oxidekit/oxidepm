//! What a client may see of an app spec.
//!
//! The daemon keeps the full spec (it needs every value to restart the app). Everything that
//! goes to a client, and the daemon's own logs, shows environment values, hook scripts and
//! probe scripts only as markers: keys stay visible so a user can see what is configured. The
//! same goes for the options an app gives an adapter (`adapters.<name>`): the option names stay,
//! the values do not. Arguments and URLs are not rewritten; no rule can tell a secret from any
//! other argument.

use crate::types::AppSpec;
use std::ops::Range;

/// Shown instead of every environment value.
pub const REDACTED: &str = "<redacted>";

/// Shown instead of a hook or probe script that is set.
pub const SET: &str = "<set>";

/// A copy of `spec` that is safe to send to a client. With `reveal` the copy is complete.
pub fn redact_spec(spec: &AppSpec, reveal: bool) -> AppSpec {
    let mut out = spec.clone();
    if reveal {
        return out;
    }
    for value in out.env.values_mut() {
        *value = REDACTED.to_string();
    }
    for hook in [
        &mut out.hooks.on_start,
        &mut out.hooks.on_stop,
        &mut out.hooks.on_restart,
        &mut out.hooks.on_crash,
        &mut out.hooks.on_error,
    ] {
        if hook.is_some() {
            *hook = Some(SET.to_string());
        }
    }
    if let Some(health) = out.health_check.as_mut() {
        if health.script.is_some() {
            health.script = Some(SET.to_string());
        }
    }
    for options in out.adapters.values_mut() {
        redact_options(options);
    }
    out
}

/// The options of one adapter with every value replaced by [`REDACTED`]: the names of the options
/// of a table stay (a nested value is replaced whole, its inner names are not shown); a value
/// that is not a table is replaced whole.
fn redact_options(options: &mut serde_json::Value) {
    match options {
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                *value = serde_json::Value::String(REDACTED.to_string());
            }
        }
        other => *other = serde_json::Value::String(REDACTED.to_string()),
    }
}

/// Environment values shorter than this are not looked for by [`scrub_text`]: a value such as
/// `1` or `on` would wreck every sentence it is found in, and protects nothing.
pub const MIN_SCRUBBED_VALUE_LEN: usize = 4;

/// The individual lines of environment values that are long enough to scrub.
pub fn env_value_lines(spec: &AppSpec, min: usize) -> impl Iterator<Item = &str> {
    spec.env
        .values()
        .flat_map(|value| value.lines())
        .filter(move |line| !line.is_empty() && line.len() >= min)
}

/// Replace occurrences in the original text, merging overlapping matches.
pub fn scrub_message(text: &str, values: &[&str]) -> String {
    let mut out = String::new();
    let mut end = 0;
    for range in scrub_ranges(text, values) {
        out.push_str(&text[end..range.start]);
        out.push_str(REDACTED);
        end = range.end;
    }
    out.push_str(&text[end..]);
    out
}

/// Byte ranges of occurrences in the original text, merging overlapping matches.
pub fn scrub_ranges(text: &str, values: &[&str]) -> Vec<Range<usize>> {
    let mut values: Vec<&str> = values
        .iter()
        .copied()
        .filter(|value| !value.is_empty())
        .collect();
    if values.is_empty() {
        return Vec::new();
    }
    values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));

    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (start, _) in text.char_indices() {
        if let Some(value) = values
            .iter()
            .find(|value| text[start..].starts_with(**value))
        {
            let end = start + value.len();
            if let Some(last) = ranges.last_mut().filter(|last| start < last.end) {
                last.end = last.end.max(end);
            } else {
                ranges.push(start..end);
            }
        }
    }
    ranges
}

/// Scrub whole environment values and their individual lines from free text about this app.
/// Matches must be at least [`MIN_SCRUBBED_VALUE_LEN`] bytes long.
pub fn scrub_text(text: &str, spec: &AppSpec) -> String {
    let values: Vec<&str> = spec
        .env
        .values()
        .map(String::as_str)
        .filter(|v| v.len() >= MIN_SCRUBBED_VALUE_LEN && v.contains('\n'))
        .chain(env_value_lines(spec, MIN_SCRUBBED_VALUE_LEN))
        .collect();
    scrub_message(text, &values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HealthCheck, Runtime};
    use std::path::PathBuf;

    fn spec_with_secrets() -> AppSpec {
        let mut spec = AppSpec::new(
            "web".to_string(),
            Runtime::CMD,
            "server".to_string(),
            PathBuf::from("/srv/web"),
        );
        spec.args = vec!["--port".to_string(), "8080".to_string()];
        spec.env
            .insert("SECRET_X".to_string(), "s3cr3t-value".to_string());
        spec.env
            .insert("MNEMONIC".to_string(), "abandon ability able".to_string());
        spec.env
            .insert("DB_DSN".to_string(), "postgres://u:pw@db/app".to_string());
        spec.env.insert("HOME".to_string(), "/home/web".to_string());
        spec.hooks.on_start = Some("curl -H 'token: hunter2' https://example.test".to_string());
        spec.hooks.on_crash = Some("notify.sh --key abc".to_string());
        spec.health_check = Some(HealthCheck {
            script: Some("check.sh --password hunter2".to_string()),
            ..HealthCheck::default()
        });
        spec
    }

    #[test]
    fn scrub_message_replaces_overlapping_secrets_and_never_rewrites_markers() {
        assert_eq!(
            scrub_message("long-secret redacted", &["long-secret", "redacted"]),
            "<redacted> <redacted>"
        );
        assert_eq!(scrub_message("xabcdef", &["xabc", "abcdef"]), "<redacted>");
    }

    #[test]
    fn redact_hides_every_env_value_keeps_keys() {
        let spec = spec_with_secrets();
        let redacted = redact_spec(&spec, false);

        let mut keys: Vec<_> = redacted.env.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["DB_DSN", "HOME", "MNEMONIC", "SECRET_X"]);
        assert!(
            redacted.env.values().all(|v| v == REDACTED),
            "{:?}",
            redacted.env
        );

        let json = serde_json::to_string(&redacted).unwrap();
        for secret in [
            "s3cr3t-value",
            "abandon ability able",
            "postgres://u:pw@db/app",
            "/home/web",
        ] {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        // everything else is untouched
        assert_eq!(redacted.name, spec.name);
        assert_eq!(redacted.args, spec.args);
        assert_eq!(redacted.cwd, spec.cwd);
    }

    #[test]
    fn redact_hooks_become_set() {
        let redacted = redact_spec(&spec_with_secrets(), false);
        assert_eq!(redacted.hooks.on_start.as_deref(), Some(SET));
        assert_eq!(redacted.hooks.on_crash.as_deref(), Some(SET));
        assert_eq!(redacted.hooks.on_stop, None, "an absent hook stays absent");
        assert_eq!(redacted.hooks.on_restart, None);
        assert_eq!(redacted.hooks.on_error, None);
        assert_eq!(redacted.health_check.unwrap().script.as_deref(), Some(SET));

        let json = serde_json::to_string(&redact_spec(&spec_with_secrets(), false)).unwrap();
        assert!(!json.contains("hunter2") && !json.contains("abc"), "{json}");
    }

    #[test]
    fn redact_hides_every_adapter_option_value_keeps_names() {
        let mut spec = spec_with_secrets();
        spec.adapters.insert(
            "notifier".to_string(),
            serde_json::json!({"chat": "ops-room", "token": {"inner": "tok-123"}, "n": 5}),
        );
        spec.adapters
            .insert("other".to_string(), serde_json::json!("plain-secret"));
        let redacted = redact_spec(&spec, false);
        assert_eq!(
            redacted.adapters["notifier"],
            serde_json::json!({"chat": REDACTED, "token": REDACTED, "n": REDACTED})
        );
        assert_eq!(redacted.adapters["other"], serde_json::json!(REDACTED));
        let json = serde_json::to_string(&redacted).unwrap();
        for secret in ["ops-room", "tok-123", "inner", "plain-secret"] {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        // the original is untouched, and a reveal shows everything
        assert_eq!(spec.adapters["notifier"]["chat"], "ops-room");
        assert_eq!(redact_spec(&spec, true).adapters, spec.adapters);
    }

    #[test]
    fn reveal_returns_the_full_spec() {
        let spec = spec_with_secrets();
        let revealed = redact_spec(&spec, true);
        assert_eq!(revealed.env, spec.env);
        assert_eq!(revealed.hooks.on_start, spec.hooks.on_start);
        assert_eq!(
            revealed.health_check.unwrap().script,
            spec.health_check.unwrap().script
        );
    }

    #[test]
    fn appspec_debug_never_prints_env_values() {
        let text = format!("{:?}", spec_with_secrets());
        for secret in [
            "s3cr3t-value",
            "abandon ability able",
            "postgres://u:pw@db/app",
            "hunter2",
            "/home/web",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        for key in ["SECRET_X", "MNEMONIC", "DB_DSN", "HOME"] {
            assert!(text.contains(key), "key {key} missing: {text}");
        }
        assert!(text.contains("web"), "the name is still printed: {text}");
    }

    #[test]
    fn scrub_message_removes_listed_values_longest_first() {
        // `abc` is inside `abc123`: scrubbing the short one first would leave `<redacted>123`
        let out = scrub_message(
            "key abc123 and abc and abc123 again",
            &["abc", "abc123", ""],
        );
        assert_eq!(out, "key <redacted> and <redacted> and <redacted> again");
        assert!(!out.contains("123"));
        assert_eq!(scrub_message("nothing here", &["zzz"]), "nothing here");
        assert_eq!(scrub_message("keep", &[]), "keep");
    }

    #[test]
    fn scrub_text_uses_the_env_values_of_the_spec() {
        let out = scrub_text("failed with hunter2 for 8080 and on", &{
            let mut spec = spec_with_secrets();
            spec.env.insert("PORT".to_string(), "8080".to_string());
            spec.env.insert("FLAG".to_string(), "on".to_string());
            spec.env.insert("PW".to_string(), "hunter2".to_string());
            spec
        });
        assert_eq!(
            out, "failed with <redacted> for <redacted> and on",
            "short values stay"
        );
    }

    #[test]
    fn scrub_text_redacts_individual_lines_of_environment_values() {
        let mut spec = spec_with_secrets();
        spec.env.insert(
            "BUILD_LABELS".to_string(),
            "first-label\r\non\nsecond-label\n".to_string(),
        );
        assert_eq!(
            scrub_text("labels: second-label then first-label and on", &spec),
            "labels: <redacted> then <redacted> and on"
        );
    }
}
