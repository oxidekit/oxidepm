//! Pure validation of app specs, health checks and sizes.
//!
//! Nothing here touches the filesystem, the clock or the daemon: the same
//! rules run at config load, and (from a later step) at the daemon's single
//! entry point for new apps.

use std::collections::HashSet;

use chrono::Utc;

use crate::error::{Error, Result};
use crate::graph;
use crate::types::{AppSpec, HealthCheck};

/// Longest accepted app name.
pub const MAX_NAME_LEN: usize = 64;
/// Most instances one cluster may have.
pub const MAX_INSTANCES: u32 = 256;
/// Longest name of a cluster: its instances are called `{name}-{0..255}`, which must still fit
/// `MAX_NAME_LEN`.
pub const MAX_CLUSTER_NAME_LEN: usize = 59;
/// Accepted `kill_timeout_ms` range.
pub const MIN_KILL_TIMEOUT_MS: u64 = 100;
pub const MAX_KILL_TIMEOUT_MS: u64 = 600_000;
/// Accepted `depends_timeout_secs` range.
pub const MIN_DEPENDS_TIMEOUT_SECS: u64 = 1;
pub const MAX_DEPENDS_TIMEOUT_SECS: u64 = 3600;
/// Accepted `log_max_size` range in bytes (1 KiB to 1 TiB).
pub const MIN_LOG_MAX_SIZE: u64 = 1024;
pub const MAX_LOG_MAX_SIZE: u64 = 1 << 40;
/// Accepted `log_max_files` range.
pub const MAX_LOG_MAX_FILES: usize = 1000;
/// Longest `adapters.<name>` table of an app, as JSON text.
pub const MAX_ADAPTER_OPTIONS_BYTES: usize = 4096;

/// Upper bound for app timers and restart windows, including uptime deadlines.
pub const MAX_APP_TIMER_SECS: u64 = 100 * 366 * 24 * 60 * 60;
pub const MAX_APP_TIMER_MS: u64 = MAX_APP_TIMER_SECS * 1000;
const MAX_MEMORY_MB: u64 = u64::MAX / (1024 * 1024);

fn check_range(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name} must be between {min} and {max} (got {value})"
        )))
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidSpec(msg.into())
}

/// Check an app name: `^[A-Za-z0-9_][A-Za-z0-9_-]{0,63}$`, not all digits
/// (a selector would read it as an id) and not `all` in any case (a selector
/// would read it as "every app"). Keeps `../x`, absolute paths, `@x` and `-x`
/// out of file names.
pub fn check_app_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("app name must not be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "app name is too long ({} bytes, at most {})",
            name.len(),
            MAX_NAME_LEN
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "invalid app name '{}': only letters, digits, underscores and hyphens are allowed",
            name
        ));
    }
    if name.starts_with('-') {
        return Err(format!(
            "invalid app name '{}': must not start with a hyphen",
            name
        ));
    }
    if name.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "invalid app name '{}': a name made only of digits would be read as an id",
            name
        ));
    }
    if name.eq_ignore_ascii_case("all") {
        return Err("invalid app name 'all': reserved (it selects every app)".to_string());
    }
    Ok(())
}

/// Check a tag: the app-name charset and length, without the id/`all`
/// restrictions (it is used as `@tag`, which cannot be mistaken for either).
pub fn check_tag(tag: &str) -> std::result::Result<(), String> {
    let ok = !tag.is_empty()
        && tag.len() <= MAX_NAME_LEN
        && !tag.starts_with('-')
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid tag '{}': 1 to {} letters, digits, underscores or hyphens, not starting with a hyphen",
            tag, MAX_NAME_LEN
        ))
    }
}

/// Parse a size such as `10mb`, `500 KB`, `1g` or `4096` (bytes).
///
/// An integer with an optional unit (`b`, `k`/`kb`, `m`/`mb`, `g`/`gb`, any
/// case, optional space). No fractions, no signs; zero, empty input and
/// overflow are errors; the result must be within 1 KiB..=1 TiB.
pub fn parse_size(input: &str) -> Result<u64> {
    let bad = |why: &str| Error::ConfigError(format!("invalid size '{}': {}", input, why));
    let s = input.trim();
    if s.is_empty() {
        return Err(bad("empty"));
    }
    if s.starts_with('-') {
        return Err(bad("must not be negative"));
    }
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(digits_end);
    if digits.is_empty() {
        return Err(bad("expected a number"));
    }
    if unit.starts_with('.') {
        return Err(bad("fractions are not supported, use a smaller unit"));
    }
    let n: u64 = digits.parse().map_err(|_| bad("number too large"))?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        other => {
            return Err(bad(&format!(
                "unknown unit '{}', use b, kb, mb or gb",
                other
            )))
        }
    };
    let bytes = n.checked_mul(multiplier).ok_or_else(|| bad("too large"))?;
    if !(MIN_LOG_MAX_SIZE..=MAX_LOG_MAX_SIZE).contains(&bytes) {
        return Err(bad("must be between 1kb and 1tb"));
    }
    Ok(bytes)
}

/// The longest start period a health check may ask for (one hour).
const MAX_HEALTH_START_PERIOD_SECS: u64 = 3600;

impl HealthCheck {
    /// Validate the probe settings. Exactly one probe kind; interval 1..=86400
    /// s; timeout 1..=300 s and not above the interval; retries 1..=100;
    /// start period 0..=3600 s;
    /// expected statuses 100..=599 (the list may not be empty); an HTTP URL
    /// starts with `http://` or `https://` and has a host; a TCP address is
    /// `host:port` with a port 1..=65535; a script is not blank.
    pub fn validate(&self) -> Result<()> {
        match (&self.http_url, &self.tcp, &self.script) {
            (None, None, None) => return Err(invalid("set http_url, tcp or script")),
            (Some(url), None, None) => check_http_url(url)?,
            (None, Some(addr), None) => check_tcp_addr(addr)?,
            (None, None, Some(script)) => {
                if script.trim().is_empty() {
                    return Err(invalid("script must not be empty"));
                }
            }
            _ => {
                return Err(invalid(
                    "set only one of http_url, tcp and script, not several",
                ))
            }
        }
        if !(1..=86_400).contains(&self.interval_secs) {
            return Err(invalid(format!(
                "interval_secs must be between 1 and 86400 (got {})",
                self.interval_secs
            )));
        }
        if !(1..=300).contains(&self.timeout_secs) {
            return Err(invalid(format!(
                "timeout_secs must be between 1 and 300 (got {})",
                self.timeout_secs
            )));
        }
        if self.timeout_secs > self.interval_secs {
            return Err(invalid(format!(
                "timeout_secs ({}) must not exceed interval_secs ({})",
                self.timeout_secs, self.interval_secs
            )));
        }
        if !(1..=100).contains(&self.retries) {
            return Err(invalid(format!(
                "retries must be between 1 and 100 (got {})",
                self.retries
            )));
        }
        if self.start_period_secs > MAX_HEALTH_START_PERIOD_SECS {
            return Err(invalid(format!(
                "start_period_secs must be between 0 and {} (got {})",
                MAX_HEALTH_START_PERIOD_SECS, self.start_period_secs
            )));
        }
        if self.expected_status.is_empty() {
            return Err(invalid("expected_status must not be empty"));
        }
        if let Some(bad) = self
            .expected_status
            .iter()
            .find(|s| !(100..=599).contains(*s))
        {
            return Err(invalid(format!(
                "expected_status {} is not an HTTP status (100..=599)",
                bad
            )));
        }
        Ok(())
    }
}

/// `host:port`: a host name, an IPv4 address or a bracketed IPv6 address, and a port 1..=65535.
fn check_tcp_addr(addr: &str) -> Result<()> {
    let bad = |why: &str| invalid(format!("tcp '{}' {}", addr, why));
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| bad("must be host:port"))?;
    let host = match host.strip_prefix('[') {
        Some(v6) => v6
            .strip_suffix(']')
            .ok_or_else(|| bad("has an unclosed '['"))?,
        None if host.contains(':') => return Err(bad("needs [brackets] around an IPv6 address")),
        None => host,
    };
    if host.is_empty() || addr.contains(char::is_whitespace) {
        return Err(bad("has no valid host"));
    }
    if valid_port(port) {
        Ok(())
    } else {
        Err(bad("needs a port between 1 and 65535"))
    }
}

fn valid_port(port: &str) -> bool {
    port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p| p != 0)
}

fn check_http_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| {
            invalid(format!(
                "http_url '{}' must start with http:// or https://",
                url
            ))
        })?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let bad = |why: &str| invalid(format!("http_url '{url}' {why}"));
    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        let (host, suffix) = v6
            .split_once(']')
            .ok_or_else(|| bad("has an unclosed '['"))?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| bad("has an invalid authority"))?,
            )
        };
        (host, port)
    } else {
        let (host, port) = host_port
            .split_once(':')
            .map_or((host_port, None), |(h, p)| (h, Some(p)));
        (host, port)
    };
    if host.is_empty()
        || host.contains(['[', ']'])
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad("has no valid host"));
    }
    if port.is_some_and(|port| !valid_port(port)) {
        return Err(bad("needs a port between 1 and 65535"));
    }
    Ok(())
}

impl AppSpec {
    /// Reset the fields a client must not choose: `id`, `instance_id` and
    /// `created_at`.
    pub fn sanitize_ingress(&mut self) {
        self.id = 0;
        self.instance_id = None;
        self.created_at = Utc::now();
    }

    /// Pure validation of everything that can be checked without looking at
    /// the machine (no filesystem, no registry).
    pub fn validate(&self) -> Result<()> {
        check_app_name(&self.name).map_err(invalid)?;

        // the shape of the name, nothing else: whether an adapter provides it is asked at start
        if !crate::types::Runtime::is_valid_name(self.mode.as_str()) {
            return Err(invalid(format!(
                "runtime '{}' is not a runtime name (a lowercase letter, then lowercase letters and digits, 2 to 16 characters)",
                self.mode
            )));
        }

        if self.command.trim().is_empty() {
            return Err(invalid("command must not be empty"));
        }
        if !self.cwd.is_absolute() {
            return Err(invalid(format!(
                "cwd '{}' must be an absolute path",
                self.cwd.display()
            )));
        }
        if !(1..=MAX_INSTANCES).contains(&self.instances) {
            return Err(invalid(format!(
                "instances must be between 1 and {} (got {})",
                MAX_INSTANCES, self.instances
            )));
        }
        if self.instances > 1
            && self.instance_id.is_none()
            && self.name.len() > MAX_CLUSTER_NAME_LEN
        {
            return Err(invalid(format!(
                "a cluster name is at most {} characters so that its instance names fit (got {})",
                MAX_CLUSTER_NAME_LEN,
                self.name.len()
            )));
        }
        self.validate_ports()?;

        for tag in &self.tags {
            check_tag(tag).map_err(invalid)?;
        }

        if let Some(size) = self.log_max_size {
            if !(MIN_LOG_MAX_SIZE..=MAX_LOG_MAX_SIZE).contains(&size) {
                return Err(invalid(format!(
                    "log_max_size must be between {} and {} bytes (got {})",
                    MIN_LOG_MAX_SIZE, MAX_LOG_MAX_SIZE, size
                )));
            }
        }
        if let Some(files) = self.log_max_files {
            if !(1..=MAX_LOG_MAX_FILES).contains(&files) {
                return Err(invalid(format!(
                    "log_max_files must be between 1 and {} (got {})",
                    MAX_LOG_MAX_FILES, files
                )));
            }
        }
        if !(MIN_KILL_TIMEOUT_MS..=MAX_KILL_TIMEOUT_MS).contains(&self.kill_timeout_ms) {
            return Err(invalid(format!(
                "kill_timeout_ms must be between {} and {} (got {})",
                MIN_KILL_TIMEOUT_MS, MAX_KILL_TIMEOUT_MS, self.kill_timeout_ms
            )));
        }
        if let Some(memory) = self.max_memory_mb {
            check_range("max_memory_mb", memory, 1, MAX_MEMORY_MB)?;
        }
        if let Some(uptime) = self.max_uptime_secs {
            check_range("max_uptime_secs", uptime, 1, MAX_APP_TIMER_SECS)?;
        }
        if let Some(delay) = self.startup_delay_ms {
            check_range("startup_delay_ms", delay, 0, MAX_APP_TIMER_MS)?;
        }
        let policy = &self.restart_policy;
        check_range(
            "crash_window_secs",
            policy.crash_window_secs,
            1,
            MAX_APP_TIMER_SECS,
        )?;
        check_range(
            "stable_uptime_secs",
            policy.stable_uptime_secs,
            1,
            MAX_APP_TIMER_SECS,
        )?;
        check_range(
            "restart_delay_ms",
            policy.restart_delay_ms,
            0,
            MAX_APP_TIMER_MS,
        )?;
        check_range(
            "max_restart_delay_ms",
            policy.max_restart_delay_ms,
            0,
            MAX_APP_TIMER_MS,
        )?;

        for (key, value) in &self.env {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(invalid(
                    "environment keys must be non-empty and contain no '=' or NUL",
                ));
            }
            if value.contains('\0') {
                return Err(invalid("environment values must not contain NUL"));
            }
        }

        let mut seen = HashSet::new();
        for dep in &self.depends_on {
            check_app_name(dep).map_err(|e| invalid(format!("depends_on: {}", e)))?;
            if dep == &self.name {
                return Err(invalid(format!("depends_on: '{}' depends on itself", dep)));
            }
            if !seen.insert(dep.as_str()) {
                return Err(invalid(format!("depends_on: '{}' is listed twice", dep)));
            }
        }

        if !(MIN_DEPENDS_TIMEOUT_SECS..=MAX_DEPENDS_TIMEOUT_SECS)
            .contains(&self.depends_timeout_secs)
        {
            return Err(invalid(format!(
                "depends_timeout_secs must be between {} and {} (got {})",
                MIN_DEPENDS_TIMEOUT_SECS, MAX_DEPENDS_TIMEOUT_SECS, self.depends_timeout_secs
            )));
        }

        if let Some(expr) = &self.restart_cron {
            crate::cron::normalize_cron(expr)?;
        }
        if let Some(health) = &self.health_check {
            health
                .validate()
                .map_err(|e| invalid(format!("health_check: {}", inner(&e))))?;
        }
        for (name, options) in &self.adapters {
            if !options.is_object() {
                return Err(invalid(format!(
                    "adapters.{name} must be a table of options"
                )));
            }
            // the size of the options as they are sent to the adapter
            let size = options.to_string().len();
            if size > MAX_ADAPTER_OPTIONS_BYTES {
                return Err(invalid(format!(
                    "adapters.{name} is {size} bytes; at most {MAX_ADAPTER_OPTIONS_BYTES} are allowed"
                )));
            }
        }
        Ok(())
    }

    fn validate_ports(&self) -> Result<()> {
        if self.port.is_some() && self.port_range.is_some() {
            return Err(invalid("set only one of port and port_range, not both"));
        }
        if let Some(port) = self.port {
            if port == 0 {
                return Err(invalid("port must be between 1 and 65535"));
            }
            if u32::from(port) + self.instances - 1 > 65_535 {
                return Err(invalid(format!(
                    "port {} leaves no room for {} instances (the last port would exceed 65535)",
                    port, self.instances
                )));
            }
        }
        if let Some((start, end)) = self.port_range {
            if start == 0 {
                return Err(invalid("port_range must start at 1 or above"));
            }
            if start > end {
                return Err(invalid(format!(
                    "port_range start {} is above its end {}",
                    start, end
                )));
            }
            let size = u32::from(end) - u32::from(start) + 1;
            if size < self.instances {
                return Err(invalid(format!(
                    "port_range {}-{} holds {} ports but instances is {}",
                    start, end, size, self.instances
                )));
            }
        }
        Ok(())
    }
}

/// The message of an `InvalidSpec` without its "Invalid spec: " prefix.
fn inner(e: &Error) -> String {
    match e {
        Error::InvalidSpec(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Validate the set as a whole, for specs that may refer to apps outside the
/// set (for example apps that are already registered): names must be unique
/// and `depends_on` must not form a cycle.
pub fn validate_set_allow_external(specs: &[AppSpec]) -> Result<()> {
    let mut names = HashSet::new();
    for spec in specs {
        if !names.insert(spec.name.as_str()) {
            return Err(invalid(format!(
                "app '{}' is defined more than once",
                spec.name
            )));
        }
    }
    let nodes: Vec<graph::Node> = specs
        .iter()
        .map(|s| (s.name.clone(), s.depends_on.clone()))
        .collect();
    if let Some(cycle) = graph::find_cycle(&nodes) {
        return Err(invalid(format!("depends_on cycle: {}", cycle.join(" -> "))));
    }
    Ok(())
}

/// Validate a self-contained set: everything in
/// [`validate_set_allow_external`], and every dependency must be in the set.
pub fn validate_set(specs: &[AppSpec]) -> Result<()> {
    validate_set_allow_external(specs)?;
    let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    for spec in specs {
        if let Some(missing) = spec.depends_on.iter().find(|d| !names.contains(d.as_str())) {
            return Err(invalid(format!(
                "app '{}' depends on unknown app '{}'",
                spec.name, missing
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Runtime;
    use std::path::PathBuf;

    #[test]
    fn app_and_dependency_name_errors_have_distinct_codes() {
        use crate::ErrorCode;
        let mut app = base();
        app.depends_on = vec!["bad name".into()];
        assert_eq!(app.validate().unwrap_err().code(), ErrorCode::InvalidSpec);
        app.depends_on.clear();
        app.name = "a".repeat(MAX_CLUSTER_NAME_LEN + 1);
        app.instances = 2;
        assert_eq!(app.validate().unwrap_err().code(), ErrorCode::InvalidName);
    }

    #[test]
    fn probe_addresses_require_a_valid_port_and_a_closed_ipv6_literal() {
        for url in [
            "http://[::1",
            "http://localhost:abc",
            "http://localhost:99999",
            "http://localhost:0",
        ] {
            assert!(HealthCheck::http(url).validate().is_err(), "{url}");
        }
        assert!(check_tcp_addr("localhost:+80").is_err());
    }

    #[test]
    fn unknown_runtime_string_is_accepted_by_validate() {
        let mut s = spec("rt");
        s.mode = "ruby".parse().unwrap();
        assert!(
            s.validate().is_ok(),
            "a runtime no built-in runner serves is not a validation error"
        );
        s.mode = "z9".parse().unwrap();
        assert!(s.validate().is_ok());
        // the shape is checked and nothing else
        for bad in [
            "",
            "x",
            "Node",
            "node-js",
            "9lives",
            "has space",
            "averyveryverylongruntimename",
        ] {
            let mut s = spec("rt");
            s.mode = serde_json::from_value(serde_json::json!(bad)).unwrap();
            let e = s.validate().expect_err(bad).to_string();
            assert!(e.contains("not a runtime name"), "{bad}: {e}");
        }
    }

    fn spec(name: &str) -> AppSpec {
        AppSpec::try_new(
            name.to_string(),
            Runtime::CMD,
            "sleep".to_string(),
            PathBuf::from("/srv/app"),
        )
        .unwrap()
    }

    fn base() -> AppSpec {
        spec("web")
    }

    /// Mutate a valid spec and assert it is rejected with a message that
    /// mentions `needle`.
    fn rejects(needle: &str, f: impl FnOnce(&mut AppSpec)) {
        let mut s = base();
        f(&mut s);
        match s.validate() {
            Err(Error::InvalidSpec(m)) => {
                assert!(
                    m.contains(needle),
                    "message {m:?} should mention {needle:?}"
                )
            }
            other => panic!("expected InvalidSpec mentioning {needle:?}, got {other:?}"),
        }
    }

    fn rejects_name(needle: &str, f: impl FnOnce(&mut AppSpec)) {
        let mut spec = base();
        f(&mut spec);
        let e = spec.validate().unwrap_err();
        assert!(
            matches!(&e, Error::InvalidSpec(message) if message.contains(needle)),
            "{e}"
        );
        assert_eq!(e.code(), crate::ErrorCode::InvalidName);
    }

    fn raw_name(name: &str) -> impl FnOnce(&mut AppSpec) + '_ {
        move |s| s.name = name.to_string()
    }

    #[test]
    fn a_plain_spec_is_valid() {
        base().validate().unwrap();
    }

    #[test]
    fn validate_rejects_instances_0() {
        rejects("instances", |s| s.instances = 0);
    }

    #[test]
    fn validate_rejects_instances_1e9() {
        rejects("instances", |s| s.instances = 1_000_000_000);
    }

    #[test]
    fn validate_caps_a_cluster_name_so_its_instances_fit() {
        let long = "a".repeat(MAX_CLUSTER_NAME_LEN + 1);
        rejects_name("cluster name", |s| {
            s.name = long.clone();
            s.instances = 2;
        });
        // a single app may use the whole length, and so may an instance of a cluster
        let mut single = base();
        single.name = "a".repeat(MAX_NAME_LEN);
        assert!(single.validate().is_ok());
        let mut cluster = base();
        cluster.name = "a".repeat(MAX_CLUSTER_NAME_LEN);
        cluster.instances = MAX_INSTANCES;
        assert!(cluster.validate().is_ok());
        for index in [0, 9, 255] {
            let child = cluster.for_instance(index, None);
            assert!(
                child.validate().is_ok(),
                "instance {index}: {:?}",
                child.name
            );
            assert_eq!(child.instances, 1);
        }
    }

    #[test]
    fn an_instance_of_a_port_range_cluster_holds_one_port_and_is_valid() {
        let mut parent = base();
        parent.instances = 3;
        parent.port_range = Some((5000, 5002));
        assert!(parent.validate().is_ok());
        let child = parent.for_instance(1, Some(5001));
        assert_eq!(
            (child.port, child.port_range, child.instances),
            (Some(5001), None, 1)
        );
        assert_eq!(child.env.get("PORT").map(String::as_str), Some("5001"));
        assert!(child.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_command() {
        rejects("command", |s| s.command = "  ".into());
    }

    #[test]
    fn validate_rejects_relative_cwd() {
        rejects("cwd", |s| s.cwd = PathBuf::from("app/dir"));
        rejects("cwd", |s| s.cwd = PathBuf::new());
    }

    #[test]
    fn validate_rejects_port_range_reversed() {
        rejects("port_range", |s| s.port_range = Some((3010, 3000)));
    }

    #[test]
    fn validate_rejects_both_port_and_range() {
        rejects("port", |s| {
            s.port = Some(3000);
            s.port_range = Some((3000, 3010));
        });
    }

    #[test]
    fn validate_rejects_port_range_smaller_than_instances() {
        rejects("instances", |s| {
            s.instances = 3;
            s.port_range = Some((4000, 4001));
        });
        let mut ok = base();
        ok.instances = 2;
        ok.port_range = Some((4000, 4001));
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_port_overflow_and_zero() {
        rejects("port", |s| s.port = Some(0));
        rejects("65535", |s| {
            s.port = Some(65_535);
            s.instances = 2;
        });
        rejects("port_range", |s| s.port_range = Some((0, 10)));
    }

    #[test]
    fn validate_rejects_bad_cron() {
        rejects("cron", |s| s.restart_cron = Some("nonsense".into()));
        let mut ok = base();
        ok.restart_cron = Some("0 4 * * *".into());
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_crash_window() {
        rejects("crash_window_secs", |s| {
            s.restart_policy.crash_window_secs = 0
        });
    }

    #[test]
    fn validate_checks_the_depends_timeout_range() {
        for ok in [1, 60, 3600] {
            let mut s = base();
            s.depends_timeout_secs = ok;
            s.validate().unwrap();
        }
        for bad in [0, 3601] {
            rejects("depends_timeout_secs", |s| s.depends_timeout_secs = bad);
        }
    }

    #[test]
    fn validate_rejects_log_max_size_0() {
        rejects("log_max_size", |s| s.log_max_size = Some(0));
        rejects("log_max_size", |s| s.log_max_size = Some(u64::MAX));
        rejects("log_max_files", |s| s.log_max_files = Some(0));
        rejects("log_max_files", |s| s.log_max_files = Some(5000));
    }

    #[test]
    fn validate_rejects_tag_with_at_sign() {
        rejects("tag", |s| s.tags = vec!["@web".into()]);
        rejects("tag", |s| s.tags = vec!["a b".into()]);
        rejects("tag", |s| s.tags = vec![String::new()]);
        let mut ok = base();
        ok.tags = vec!["backend".into(), "v2".into(), "123".into()];
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_name_traversal() {
        rejects_name("name", raw_name("../../evil"));
        rejects_name("name", raw_name("/etc/passwd"));
        rejects_name("name", raw_name("a/b"));
        rejects_name("name", raw_name("a.b"));
        rejects_name("name", raw_name("@x"));
        rejects_name("name", raw_name(""));
        rejects_name("name", raw_name("white space"));
    }

    #[test]
    fn validate_rejects_name_all_any_case() {
        for n in ["all", "ALL", "All", "aLl"] {
            rejects_name("all", raw_name(n));
        }
        let mut ok = base();
        ok.name = "allies".into();
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_name_numeric() {
        rejects_name("digits", raw_name("123"));
        let mut ok = base();
        ok.name = "123a".into();
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_name_leading_dash() {
        rejects_name("hyphen", raw_name("-x"));
        let mut ok = base();
        ok.name = "_x-y".into();
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_name_too_long() {
        rejects_name("too long", raw_name(&"a".repeat(65)));
        let mut ok = base();
        ok.name = "a".repeat(64);
        ok.validate().unwrap();
    }

    #[test]
    fn validate_rejects_kill_timeout_out_of_range() {
        rejects("kill_timeout_ms", |s| s.kill_timeout_ms = 0);
        rejects("kill_timeout_ms", |s| s.kill_timeout_ms = 10_000_000);
    }

    #[test]
    fn validate_rejects_environment_entries_that_cannot_be_spawned() {
        for (key, value) in [("", "x"), ("A=B", "x"), ("A\0B", "x"), ("A", "x\0y")] {
            rejects("environment", |s| {
                s.env.insert(key.into(), value.into());
            });
        }
        let mut spec = base();
        spec.env.insert("A".into(), "A=B".into());
        spec.validate().unwrap();
    }

    #[test]
    fn validate_rejects_restart_and_limit_values_outside_the_supported_range() {
        rejects("stable_uptime_secs", |s| {
            s.restart_policy.stable_uptime_secs = 0
        });
        rejects("max_uptime_secs", |s| s.max_uptime_secs = Some(u64::MAX));
        rejects("startup_delay_ms", |s| s.startup_delay_ms = Some(u64::MAX));
        rejects("max_memory_mb", |s| s.max_memory_mb = Some(u64::MAX));
        rejects("restart_delay_ms", |s| {
            s.restart_policy.restart_delay_ms = u64::MAX
        });
        rejects("max_restart_delay_ms", |s| {
            s.restart_policy.max_restart_delay_ms = u64::MAX
        });
        rejects("crash_window_secs", |s| {
            s.restart_policy.crash_window_secs = u64::MAX
        });
    }

    #[test]
    fn validate_rejects_zero_limits() {
        rejects("max_memory_mb", |s| s.max_memory_mb = Some(0));
        rejects("max_uptime_secs", |s| s.max_uptime_secs = Some(0));
    }

    #[test]
    fn validate_rejects_depends_self() {
        rejects("itself", |s| s.depends_on = vec!["web".into()]);
    }

    #[test]
    fn validate_rejects_depends_duplicate() {
        rejects("twice", |s| s.depends_on = vec!["db".into(), "db".into()]);
        rejects("depends_on", |s| s.depends_on = vec!["../db".into()]);
    }

    #[test]
    fn validate_accepts_adapter_options_and_bounds_them() {
        let mut ok = base();
        ok.adapters
            .insert("watch".into(), serde_json::json!({"paths": ["src"]}));
        ok.adapters.insert("quiet".into(), serde_json::json!({}));
        ok.validate().unwrap();
        // an option table is a table
        rejects("must be a table", |s| {
            s.adapters
                .insert("watch".into(), serde_json::json!(["src"]));
        });
        // and at most 4 KiB
        rejects("at most 4096", |s| {
            s.adapters.insert(
                "watch".into(),
                serde_json::json!({"blob": "x".repeat(MAX_ADAPTER_OPTIONS_BYTES)}),
            );
        });
        let mut edge = base();
        let overhead = serde_json::json!({"blob": ""}).to_string().len();
        edge.adapters.insert(
            "watch".into(),
            serde_json::json!({"blob": "x".repeat(MAX_ADAPTER_OPTIONS_BYTES - overhead)}),
        );
        assert_eq!(
            edge.adapters["watch"].to_string().len(),
            MAX_ADAPTER_OPTIONS_BYTES
        );
        edge.validate().unwrap();
    }

    #[test]
    fn validate_checks_the_health_check() {
        rejects("health_check", |s| {
            s.health_check = Some(HealthCheck::http("http://localhost/h"));
            s.health_check.as_mut().unwrap().retries = 0;
        });
        let mut ok = base();
        ok.health_check = Some(HealthCheck::script("true"));
        ok.validate().unwrap();
    }

    #[test]
    fn sanitize_ingress_resets_client_chosen_fields() {
        let mut s = base();
        s.id = 99;
        s.instance_id = Some(3);
        let old = chrono::DateTime::from_timestamp(0, 0).unwrap();
        s.created_at = old;
        s.sanitize_ingress();
        assert_eq!(s.id, 0);
        assert_eq!(s.instance_id, None);
        assert!(s.created_at > old);
    }

    #[test]
    fn validate_set_rejects_cycle_a_b() {
        let mut a = spec("a");
        a.depends_on = vec!["b".into()];
        let mut b = spec("b");
        b.depends_on = vec!["a".into()];
        let e = validate_set(&[a.clone(), b.clone()])
            .unwrap_err()
            .to_string();
        assert!(e.contains("cycle") && e.contains("a -> b -> a"), "{e}");
        assert!(validate_set_allow_external(&[a, b]).is_err());
    }

    #[test]
    fn validate_set_rejects_unknown_dependency() {
        let mut a = spec("a");
        a.depends_on = vec!["ghost".into()];
        let e = validate_set(std::slice::from_ref(&a))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown app 'ghost'"), "{e}");
        // The lenient form leaves registry lookups to the caller.
        validate_set_allow_external(&[a]).unwrap();
    }

    #[test]
    fn validate_set_rejects_duplicate_names() {
        let e = validate_set(&[spec("a"), spec("a")])
            .unwrap_err()
            .to_string();
        assert!(e.contains("more than once"), "{e}");
    }

    #[test]
    fn validate_set_accepts_a_chain() {
        let mut web = spec("web");
        web.depends_on = vec!["db".into()];
        validate_set(&[web, spec("db")]).unwrap();
    }

    fn health() -> HealthCheck {
        HealthCheck::http("http://localhost:3000/health")
    }

    fn health_rejects(needle: &str, f: impl FnOnce(&mut HealthCheck)) {
        let mut h = health();
        f(&mut h);
        match h.validate() {
            Err(Error::InvalidSpec(m)) => assert!(m.contains(needle), "{m:?} vs {needle:?}"),
            other => panic!("expected InvalidSpec mentioning {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn health_validate_accepts_defaults_and_scripts() {
        health().validate().unwrap();
        HealthCheck::script("exit 0").validate().unwrap();
        HealthCheck::http("https://user@example.com:8443/h?x=1")
            .validate()
            .unwrap();
        HealthCheck::http("http://[::1]:80/").validate().unwrap();
    }

    #[test]
    fn health_validate_rejects_interval_0() {
        health_rejects("interval_secs", |h| h.interval_secs = 0);
        health_rejects("interval_secs", |h| h.interval_secs = 100_000);
    }

    #[test]
    fn health_validate_rejects_timeout_0() {
        health_rejects("timeout_secs", |h| h.timeout_secs = 0);
        health_rejects("timeout_secs", |h| {
            h.timeout_secs = 301;
            h.interval_secs = 600;
        });
    }

    #[test]
    fn health_validate_rejects_start_period_out_of_range() {
        health_rejects("start_period_secs", |h| h.start_period_secs = 3601);
        health_rejects("start_period_secs", |h| h.start_period_secs = u64::MAX);
        let mut h = health();
        h.start_period_secs = 0;
        h.validate().unwrap();
        h.start_period_secs = 3600;
        h.validate().unwrap();
    }

    #[test]
    fn health_validate_rejects_retries_0() {
        health_rejects("retries", |h| h.retries = 0);
        health_rejects("retries", |h| h.retries = 101);
    }

    #[test]
    fn health_validate_rejects_timeout_gt_interval() {
        health_rejects("must not exceed", |h| {
            h.interval_secs = 2;
            h.timeout_secs = 3;
        });
    }

    #[test]
    fn health_validate_rejects_both_http_and_script() {
        health_rejects("only one", |h| h.script = Some("true".into()));
    }

    #[test]
    fn health_validate_rejects_none() {
        health_rejects("http_url, tcp or script", |h| h.http_url = None);
    }

    #[test]
    fn health_validate_accepts_tcp() {
        for addr in [
            "127.0.0.1:8080",
            "localhost:80",
            "db.internal:5432",
            "[::1]:6379",
        ] {
            HealthCheck::tcp(addr)
                .validate()
                .unwrap_or_else(|e| panic!("{addr}: {e}"));
        }
    }

    #[test]
    fn health_validate_rejects_bad_tcp() {
        for (addr, needle) in [
            ("localhost", "host:port"),
            ("localhost:", "port"),
            ("localhost:0", "port"),
            ("localhost:65536", "port"),
            ("localhost:http", "port"),
            (":8080", "host"),
            ("a b:80", "host"),
            ("::1:80", "brackets"),
            ("[::1:80", "'['"),
            ("[]:80", "host"),
        ] {
            match HealthCheck::tcp(addr).validate() {
                Err(Error::InvalidSpec(m)) => {
                    assert!(m.contains(needle), "{addr}: {m:?} vs {needle:?}")
                }
                other => {
                    panic!("{addr}: expected InvalidSpec mentioning {needle:?}, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn health_validate_rejects_tcp_with_another_probe() {
        let mut h = HealthCheck::tcp("127.0.0.1:80");
        h.script = Some("true".into());
        assert!(matches!(h.validate(), Err(Error::InvalidSpec(m)) if m.contains("only one")));
        let mut h = HealthCheck::tcp("127.0.0.1:80");
        h.http_url = Some("http://localhost/".into());
        assert!(matches!(h.validate(), Err(Error::InvalidSpec(m)) if m.contains("only one")));
    }

    #[test]
    fn health_validate_rejects_status_99() {
        health_rejects("99", |h| h.expected_status = vec![200, 99]);
        health_rejects("600", |h| h.expected_status = vec![600]);
        health_rejects("expected_status", |h| h.expected_status = vec![]);
    }

    #[test]
    fn health_validate_rejects_bad_urls_and_blank_script() {
        health_rejects("http://", |h| h.http_url = Some("ftp://x/".into()));
        health_rejects("host", |h| h.http_url = Some("http://".into()));
        health_rejects("host", |h| h.http_url = Some("http:///path".into()));
        health_rejects("host", |h| h.http_url = Some("http://:8080/".into()));
        health_rejects("host", |h| h.http_url = Some("http://a b/".into()));
        let mut h = HealthCheck::script("  ");
        h.timeout_secs = 5;
        assert!(h.validate().is_err());
    }

    #[test]
    fn parse_size_mb() {
        assert_eq!(parse_size("10mb").unwrap(), 10 << 20);
        assert_eq!(parse_size("10m").unwrap(), 10 << 20);
        assert_eq!(parse_size("10MB").unwrap(), 10 << 20);
    }

    #[test]
    fn parse_size_kb_space() {
        assert_eq!(parse_size("500 kb").unwrap(), 500 << 10);
        assert_eq!(parse_size(" 500K ").unwrap(), 500 << 10);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("2048b").unwrap(), 2048);
    }

    #[test]
    fn parse_size_gb() {
        assert_eq!(parse_size("1gb").unwrap(), 1 << 30);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("1024g").unwrap(), 1 << 40);
    }

    #[test]
    fn parse_size_rejects_zero() {
        assert!(parse_size("0").is_err());
        assert!(parse_size("0mb").is_err());
        assert!(parse_size("1023").is_err(), "below 1 KiB");
    }

    #[test]
    fn parse_size_rejects_negative() {
        assert!(parse_size("-5mb").is_err());
        assert!(parse_size("-1").is_err());
    }

    #[test]
    fn parse_size_rejects_float() {
        assert!(parse_size("1.5mb").is_err());
        assert!(parse_size("0.5gb").is_err());
    }

    #[test]
    fn parse_size_rejects_overflow() {
        assert!(parse_size("99999999999gb").is_err());
        assert!(parse_size("18446744073709551616").is_err());
        assert!(parse_size("1025g").is_err(), "above 1 TiB");
        assert!(
            parse_size("17179869184gb").is_err(),
            "u64 overflow when multiplied"
        );
    }

    #[test]
    fn parse_size_rejects_empty() {
        assert!(parse_size("").is_err());
        assert!(parse_size("   ").is_err());
        assert!(parse_size("mb").is_err());
    }

    #[test]
    fn parse_size_rejects_unknown_unit() {
        assert!(parse_size("10xyz").is_err());
        assert!(parse_size("10tb").is_err());
        assert!(parse_size("abc").is_err());
    }
}
