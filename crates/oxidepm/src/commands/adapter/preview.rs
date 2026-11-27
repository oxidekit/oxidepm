//! What the CLI prints about an adapter before anything runs: the consent screen.
//!
//! The preview is a security control and it prints strings that a publisher wrote, so every such
//! string goes through [`sanitize`]. The permission block is printed after the description and
//! repeated as the last lines, so that nothing in the description can push it out of sight.

use oxidepm_adapter_proto::permissions::{NetworkMode, SpawnMode};
use oxidepm_adapter_proto::{Growth, HostLevel, HostScope, Kind, Manifest, Permissions};
use oxidepm_sandbox::SandboxCaps;

/// `text` with every control character (C0 and C1, ESC and DEL included) replaced by U+FFFD. A
/// newline is kept when `allow_newline` is set (descriptions only).
pub fn sanitize(text: &str, allow_newline: bool) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && !(allow_newline && c == '\n') {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// What this machine can enforce, as far as the preview can tell.
#[derive(Debug, Clone)]
pub struct Machine {
    pub caps: SandboxCaps,
    /// The launcher `oxidepm-sandbox` is installed next to the executable.
    pub launcher: bool,
}

impl Machine {
    /// Probe this machine, as the daemon does.
    pub fn probe() -> Machine {
        let launcher = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("oxidepm-sandbox")))
            .is_some_and(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()));
        Machine {
            caps: oxidepm_sandbox::probe(),
            launcher,
        }
    }

    fn files(&self) -> (bool, String) {
        match (self.launcher, self.caps.landlock_abi) {
            (true, Some(abi)) => (true, format!("yes (Landlock {abi})")),
            (false, _) => (
                false,
                "no (the sandbox launcher is not installed)".to_string(),
            ),
            (true, None) => (false, "no (this kernel has no Landlock)".to_string()),
        }
    }

    fn network(&self) -> (bool, String) {
        match (self.launcher, self.caps.seccomp) {
            (true, true) => (true, "yes (seccomp)".to_string()),
            (false, _) => (
                false,
                "no (the sandbox launcher is not installed)".to_string(),
            ),
            (true, false) => (false, "no (this kernel has no seccomp)".to_string()),
        }
    }
}

/// Everything the preview shows.
pub struct Preview<'a> {
    pub manifest: &'a Manifest,
    pub source_line: String,
    pub trust_line: String,
    pub fingerprint: String,
    pub machine: Machine,
    /// For an update: what grew over the installed version.
    pub growth: Vec<Growth>,
    /// For an update: the installed version.
    pub installed_version: Option<String>,
    /// The command that comes next.
    pub next: String,
}

fn level_text(p: &Permissions) -> String {
    let level = match p.host.level {
        HostLevel::Read => "read only (status)",
        HostLevel::Lifecycle => "may stop, restart and reload apps",
        HostLevel::Admin => "ADMIN: may start arbitrary commands, delete apps and flush logs",
    };
    let scope = match p.host.scope {
        HostScope::All => "",
        HostScope::OptedIn => " Only the apps that list it.",
    };
    let events = if p.events.is_empty() {
        String::new()
    } else {
        format!(" Events: {}.", p.events.join(", "))
    };
    format!("{level}.{scope}{events}")
}

fn grows(growth: &[Growth], field: &str) -> &'static str {
    if growth
        .iter()
        .any(|g| g.field == field || g.field.starts_with(&format!("{field}.")))
    {
        "  [GROWS]"
    } else {
        ""
    }
}

/// The "Needs" block, as lines.
fn needs_block(p: &Permissions, machine: &Machine, growth: &[Growth]) -> Vec<String> {
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let broad = |on: bool| if on { "  [BROAD]" } else { "" };

    rows.push((
        "host".into(),
        format!(
            "{}{}{}",
            level_text(p),
            broad(p.host.level == HostLevel::Admin),
            grows(growth, "host")
        ),
        String::new(),
    ));

    let net = match p.network.mode {
        NetworkMode::None => "none.".to_string(),
        NetworkMode::Outbound => {
            let hosts = if p.network.hosts.is_empty() {
                "It does not say which hosts.".to_string()
            } else {
                format!(
                    "It says it uses {}. Not enforceable per host.",
                    p.network.hosts.join(", ")
                )
            };
            let listen = if p.network.listen.is_empty() {
                String::new()
            } else {
                format!(" Listens on {}.", p.network.listen.join(", "))
            };
            format!("outbound. {hosts}{listen}")
        }
    };
    let (_, net_text) = machine.network();
    rows.push((
        "network".into(),
        format!(
            "{net}{}{}",
            broad(p.network.mode == NetworkMode::Outbound),
            grows(growth, "network")
        ),
        format!("Enforced on this machine: {net_text}"),
    ));

    let read_any = p.fs.read.iter().any(|x| x == "any");
    let write_any = p.fs.write.iter().any(|x| x == "any");
    let writes = if p.fs.write.is_empty() {
        "writes nothing".to_string()
    } else if write_any {
        "writes anywhere".to_string()
    } else if p.fs.write.iter().all(|x| x == "$STATE") {
        "writes only its own directory".to_string()
    } else {
        format!("writes {}", p.fs.write.join(", "))
    };
    let reads = if read_any {
        ", reads any file you can read".to_string()
    } else if p.fs.read.iter().any(|x| x != "$STATE") {
        let others: Vec<&str> =
            p.fs.read
                .iter()
                .map(String::as_str)
                .filter(|x| *x != "$STATE")
                .collect();
        format!(", reads {}", others.join(", "))
    } else {
        String::new()
    };
    let (_, fs_text) = machine.files();
    rows.push((
        "files".into(),
        format!(
            "{writes}{reads}.{}{}",
            broad(read_any || write_any),
            grows(growth, "fs")
        ),
        format!("Enforced on this machine: {fs_text}"),
    ));

    let spawn = match p.spawn.mode {
        SpawnMode::None => "none.".to_string(),
        SpawnMode::Named => format!("only {}.", p.spawn.names.join(", ")),
        SpawnMode::Any => "ANY program.".to_string(),
    };
    rows.push((
        "programs".into(),
        format!(
            "{spawn}{}{}",
            broad(p.spawn.mode == SpawnMode::Any),
            grows(growth, "spawn")
        ),
        format!("Enforced on this machine: {}", machine.files().1),
    ));

    if !p.env_pass.is_empty() {
        rows.push((
            "environment".into(),
            format!(
                "receives {} from your environment.  [BROAD]{}",
                p.env_pass.join(", "),
                grows(growth, "env_pass")
            ),
            String::new(),
        ));
    }
    if !p.connect_env_socket.is_empty() {
        let limits = if machine.launcher && machine.caps.landlock_unix_connect {
            "Connection limits: Landlock pathname rules"
        } else {
            "Pathname connection limits unavailable"
        };
        rows.push((
            "sockets".into(),
            format!(
                "connects to UNIX sockets from {}.{}",
                p.connect_env_socket.join(", "),
                grows(growth, "connect_env_socket")
            ),
            limits.into(),
        ));
    }
    rows.push((
        "memory".into(),
        format!("{} MiB.{}", p.memory_mb, grows(growth, "memory_mb")),
        String::new(),
    ));

    let width = rows
        .iter()
        .map(|(_, left, _)| left.chars().count())
        .max()
        .unwrap_or(0)
        .min(70);
    rows.into_iter()
        .map(|(label, left, right)| {
            if right.is_empty() {
                format!("{label:<9} {left}")
            } else {
                format!("{label:<9} {left:<width$}  {right}")
            }
        })
        .collect()
}

fn adds_line(m: &Manifest) -> String {
    let mut parts = Vec::new();
    if !m.provides.verbs.is_empty() {
        let verbs: Vec<String> = m
            .provides
            .verbs
            .iter()
            .map(|v| {
                format!(
                    "{} ({:?}{})",
                    v.name,
                    v.tier,
                    if v.writes { ", writes" } else { "" }
                )
                .to_lowercase()
            })
            .collect();
        parts.push(format!("commands: {}", verbs.join(", ")));
    } else {
        parts.push("no commands".to_string());
    }
    if !m.provides.runtimes.is_empty() {
        parts.push(format!(
            "runtimes: {}. A runtime adapter makes the core run the programs it names ({}) with your app's environment",
            m.provides.runtimes.join(", "),
            m.provides.programs.join(", ")
        ));
    } else if !m.provides.programs.is_empty() {
        parts.push(format!("programs: {}", m.provides.programs.join(", ")));
    }
    parts.join(".   ")
}

/// The whole preview.
pub fn render(p: &Preview<'_>) -> String {
    let m = p.manifest;
    let mut out = Vec::new();
    let kind = match m.kind {
        Kind::Service => "service",
        Kind::Tool => "tool",
    };
    out.push(format!(
        "Adapter   {} {} ({})   {}",
        sanitize(&m.name, false),
        sanitize(&m.version, false),
        kind,
        sanitize(&m.summary, false)
    ));
    if let Some(v) = &p.installed_version {
        out.push(format!(
            "Update    from {} to {}",
            sanitize(v, false),
            sanitize(&m.version, false)
        ));
    }
    if !m.description.trim().is_empty() {
        for line in sanitize(&m.description, true).lines() {
            out.push(format!("          {line}"));
        }
    }
    out.push(format!("Source    {}", sanitize(&p.source_line, false)));
    out.push(format!("Trust     {}", sanitize(&p.trust_line, false)));
    let needs = needs_block(&m.permissions, &p.machine, &p.growth);
    for (i, line) in needs.iter().enumerate() {
        out.push(if i == 0 {
            format!("Needs     {line}")
        } else {
            format!("          {line}")
        });
    }
    if !p.growth.is_empty() {
        out.push("Grows     compared with the installed version:".to_string());
        for g in &p.growth {
            out.push(format!(
                "          {}: {}",
                sanitize(g.field, false),
                sanitize(&g.detail, false)
            ));
        }
    }
    out.push(format!("Adds      {}", sanitize(&adds_line(m), false)));
    out.push(format!("Next      {}", p.next));
    // the permission block again, last
    out.push("Again, what it needs:".to_string());
    for line in &needs {
        out.push(format!("  {line}"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_c0_c1_and_escape_and_keeps_newlines_only_when_asked() {
        let nasty = "a\u{1b}[2Kb\u{9b}2Jc\u{7}d\u{7f}e\nf";
        assert_eq!(
            sanitize(nasty, false),
            "a\u{FFFD}[2Kb\u{FFFD}2Jc\u{FFFD}d\u{FFFD}e\u{FFFD}f"
        );
        assert_eq!(
            sanitize(nasty, true),
            "a\u{FFFD}[2Kb\u{FFFD}2Jc\u{FFFD}d\u{FFFD}e\nf"
        );
        assert_eq!(
            sanitize("plain text, ünïcode", false),
            "plain text, ünïcode"
        );
    }

    fn manifest(perms: serde_json::Value) -> Manifest {
        let value = serde_json::json!({
            "schema": 1, "name": "demo", "version": "1.2.3", "kind": "tool", "summary": "A demo.",
            "protocol": [1], "min_daemon": "0.1.0", "exec": "demo", "provides": {}, "permissions": perms
        });
        Manifest::parse(value.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn the_permission_block_comes_after_the_description_and_again_at_the_end() {
        let mut m = manifest(
            serde_json::json!({"host": {"level": "read", "scope": "all"}, "memory_mb": 32}),
        );
        m.description = "Line one.\nLine two with \u{1b}[31mcolour.".to_string();
        let machine = Machine {
            caps: SandboxCaps::default(),
            launcher: false,
        };
        let p = Preview {
            manifest: &m,
            source_line: "pinned".into(),
            trust_line: "pinned by you".into(),
            fingerprint: "f".into(),
            machine,
            growth: Vec::new(),
            installed_version: None,
            next: "oxidepm adapter enable demo --accept f".into(),
        };
        let text = render(&p);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        let desc = text.find("Line two").unwrap();
        let first_block = text.find("Needs     host").unwrap();
        let again = text.find("Again, what it needs:").unwrap();
        assert!(desc < first_block && first_block < again, "{text}");
        assert!(
            text.contains("no (the sandbox launcher is not installed)"),
            "an honest 'not enforced': {text}"
        );
        assert!(text.trim_end().ends_with("32 MiB."), "{text}");
    }

    #[test]
    fn what_grew_is_marked_in_the_block_and_listed() {
        let m = manifest(
            serde_json::json!({"host": {"level": "read", "scope": "all"}, "network": {"mode": "outbound"}}),
        );
        let growth = vec![Growth {
            field: "network.mode",
            detail: "none to outbound".into(),
        }];
        let p = Preview {
            manifest: &m,
            source_line: "s".into(),
            trust_line: "t".into(),
            fingerprint: "f".into(),
            machine: Machine {
                caps: SandboxCaps::default(),
                launcher: true,
            },
            growth,
            installed_version: Some("1.0.0".into()),
            next: "n".into(),
        };
        let text = render(&p);
        assert!(text.contains("Update    from 1.0.0 to 1.2.3"), "{text}");
        assert!(
            text.contains("[GROWS]") && text.contains("network.mode: none to outbound"),
            "{text}"
        );
    }
}
