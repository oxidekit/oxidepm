//! Declared permissions and how an update's permissions compare to the installed
//! ones. The daemon enforces the host part always; the OS part where the kernel allows it;
//! the rest is consent.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::manifest::{Invalid, Rule};
use crate::text::{has_control, is_env_name, is_program_name};

pub const DEFAULT_MEMORY_MB: u32 = 128;
pub const MAX_MEMORY_MB: u32 = 512;
pub const DEFAULT_HOST_IN_FLIGHT: u32 = 32;
pub const MAX_HOST_IN_FLIGHT: u32 = 128;
/// The only event families of protocol 1.
pub const EVENT_FAMILIES: &[&str] = &["lifecycle", "adapter"];

/// How much of the daemon's API an adapter may use through `host.request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostLevel {
    Read,
    Lifecycle,
    Admin,
}

/// Which apps the host level applies to. `opted-in` is the narrower one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HostScope {
    #[serde(rename = "opted-in")]
    OptedIn,
    #[serde(rename = "all")]
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    None,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpawnMode {
    None,
    Named,
    Any,
}

/// Things the manifest may require the OS layer to enforce (`requires_enforcement`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforce {
    Network,
    FsWrite,
    FsRead,
    Spawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPerm {
    pub level: HostLevel,
    pub scope: HostScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPerm {
    pub mode: NetworkMode,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub listen: Vec<String>,
}

impl Default for NetworkPerm {
    fn default() -> Self {
        NetworkPerm {
            mode: NetworkMode::None,
            hosts: Vec::new(),
            listen: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FsPerm {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnPerm {
    pub mode: SpawnMode,
    #[serde(default)]
    pub names: Vec<String>,
}

impl Default for SpawnPerm {
    fn default() -> Self {
        SpawnPerm {
            mode: SpawnMode::None,
            names: Vec::new(),
        }
    }
}

fn default_memory() -> u32 {
    DEFAULT_MEMORY_MB
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub host: HostPerm,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub env_pass: Vec<String>,
    /// Passed variables whose values name UNIX sockets the adapter may connect to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect_env_socket: Vec<String>,
    #[serde(default)]
    pub network: NetworkPerm,
    #[serde(default)]
    pub fs: FsPerm,
    #[serde(default)]
    pub spawn: SpawnPerm,
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    #[serde(default)]
    pub requires_enforcement: Vec<Enforce>,
}

/// One field of an update that gives the adapter more power than the installed version had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Growth {
    /// The dotted manifest path, e.g. `network.mode` or `provides.verbs`.
    pub field: &'static str,
    /// A short human sentence naming what grew.
    pub detail: String,
}

pub(crate) fn growth(field: &'static str, detail: impl Into<String>) -> Growth {
    Growth {
        field,
        detail: detail.into(),
    }
}

/// Entries of `new` that `old` does not have.
pub(crate) fn added<'a>(old: &[String], new: &'a [String]) -> Vec<&'a str> {
    let old: BTreeSet<&str> = old.iter().map(String::as_str).collect();
    let mut out: Vec<&str> = new
        .iter()
        .map(String::as_str)
        .filter(|s| !old.contains(s))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// A list of paths grows by any new path; `any` is above every list.
fn paths_grow(field: &'static str, old: &[String], new: &[String], out: &mut Vec<Growth>) {
    let old_any = old.iter().any(|p| p == "any");
    if old_any {
        return;
    }
    if new.iter().any(|p| p == "any") {
        out.push(growth(field, "now any path"));
        return;
    }
    let extra = added(old, new);
    if !extra.is_empty() {
        out.push(growth(field, format!("adds {}", extra.join(", "))));
    }
}

fn list_grows(field: &'static str, old: &[String], new: &[String], out: &mut Vec<Growth>) {
    let extra = added(old, new);
    if !extra.is_empty() {
        out.push(growth(field, format!("adds {}", extra.join(", "))));
    }
}

impl Permissions {
    /// Every way `self` (the new version) is more powerful than `old` (the installed one). An empty
    /// list means the existing grant can be kept. Shrinking a field is never growth.
    pub fn grows_over(&self, old: &Permissions) -> Vec<Growth> {
        let mut out = Vec::new();
        if self.host.level > old.host.level {
            out.push(growth(
                "host.level",
                format!("{:?} to {:?}", old.host.level, self.host.level).to_lowercase(),
            ));
        }
        if self.host.scope > old.host.scope {
            out.push(growth("host.scope", "from opted-in apps to all apps"));
        }
        let (a, b) = (
            old.host.max_in_flight.unwrap_or(DEFAULT_HOST_IN_FLIGHT),
            self.host.max_in_flight.unwrap_or(DEFAULT_HOST_IN_FLIGHT),
        );
        if b > a {
            out.push(growth("host.max_in_flight", format!("{a} to {b}")));
        }
        list_grows("events", &old.events, &self.events, &mut out);
        list_grows("env_pass", &old.env_pass, &self.env_pass, &mut out);
        list_grows(
            "connect_env_socket",
            &old.connect_env_socket,
            &self.connect_env_socket,
            &mut out,
        );
        if self.network.mode > old.network.mode {
            out.push(growth("network.mode", "from none to outbound"));
        }
        list_grows(
            "network.hosts",
            &old.network.hosts,
            &self.network.hosts,
            &mut out,
        );
        list_grows(
            "network.listen",
            &old.network.listen,
            &self.network.listen,
            &mut out,
        );
        paths_grow("fs.read", &old.fs.read, &self.fs.read, &mut out);
        paths_grow("fs.write", &old.fs.write, &self.fs.write, &mut out);
        if self.spawn.mode > old.spawn.mode {
            out.push(growth(
                "spawn.mode",
                format!("{:?} to {:?}", old.spawn.mode, self.spawn.mode).to_lowercase(),
            ));
        }
        list_grows("spawn.names", &old.spawn.names, &self.spawn.names, &mut out);
        if self.memory_mb > old.memory_mb {
            out.push(growth(
                "memory_mb",
                format!("{} to {} MiB", old.memory_mb, self.memory_mb),
            ));
        }
        // An enforcement requirement that disappears lets the adapter run where it could not.
        let dropped: Vec<String> = old
            .requires_enforcement
            .iter()
            .filter(|e| !self.requires_enforcement.contains(e))
            .map(|e| format!("{e:?}").to_lowercase())
            .collect();
        if !dropped.is_empty() {
            out.push(growth(
                "requires_enforcement",
                format!("no longer requires {}", dropped.join(", ")),
            ));
        }
        out
    }

    fn automatic_read_path(path: &str) -> bool {
        matches!(path, "$STATE" | "/etc/hosts")
    }

    /// Whether the declaration is eligible for automatic consent. The daemon checks the
    /// allowed paths and the available enforcement on this machine before enabling it.
    pub fn is_pure_class(&self) -> bool {
        self.host.level == HostLevel::Read
            && self.network.mode == NetworkMode::None
            && self.spawn.mode == SpawnMode::None
            && self.env_pass.is_empty()
            && self.connect_env_socket.is_empty()
            && self.fs.read.iter().all(|p| Self::automatic_read_path(p))
            && self.fs.write.iter().all(|p| p == "$STATE")
    }

    /// Broad permissions to highlight when showing what an adapter requests.
    pub fn broad_items(&self) -> Vec<String> {
        let mut out = Vec::new();
        for path in self
            .fs
            .read
            .iter()
            .filter(|p| !Self::automatic_read_path(p))
        {
            out.push(format!("fs.read = {path}"));
        }
        for path in self.fs.write.iter().filter(|p| *p != "$STATE") {
            out.push(format!("fs.write = {path}"));
        }
        match self.spawn.mode {
            SpawnMode::None => {}
            SpawnMode::Named => out.push(format!(
                "spawn = allowlist ({})",
                self.spawn.names.join(", ")
            )),
            SpawnMode::Any => out.push("spawn = any".to_string()),
        }
        if self.network.mode == NetworkMode::Outbound {
            out.push("network = outbound".to_string());
        }
        match self.host.level {
            HostLevel::Read => {}
            HostLevel::Lifecycle => out.push("host.level = lifecycle".to_string()),
            HostLevel::Admin => out.push("host.level = admin".to_string()),
        }
        for name in &self.env_pass {
            out.push(format!("env_pass {name}"));
        }
        for name in &self.connect_env_socket {
            out.push(format!("connect_env_socket {name}"));
        }
        out
    }

    /// The same permissions with every list sorted and without duplicates, so that two manifests
    /// that differ only in order are equal.
    pub fn normalized(&self) -> Permissions {
        fn sorted(v: &[String]) -> Vec<String> {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        }
        let mut enforce = self.requires_enforcement.clone();
        enforce.sort();
        enforce.dedup();
        Permissions {
            host: self.host.clone(),
            events: sorted(&self.events),
            env_pass: sorted(&self.env_pass),
            connect_env_socket: sorted(&self.connect_env_socket),
            network: NetworkPerm {
                mode: self.network.mode,
                hosts: sorted(&self.network.hosts),
                listen: sorted(&self.network.listen),
            },
            fs: FsPerm {
                read: sorted(&self.fs.read),
                write: sorted(&self.fs.write),
            },
            spawn: SpawnPerm {
                mode: self.spawn.mode,
                names: sorted(&self.spawn.names),
            },
            memory_mb: self.memory_mb,
            requires_enforcement: enforce,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        let bad = |rule: Rule, detail: String| Err(Invalid { rule, detail });
        if let Some(n) = self.host.max_in_flight {
            if n == 0 || n > MAX_HOST_IN_FLIGHT {
                return bad(
                    Rule::Permissions,
                    format!("host.max_in_flight must be 1 to {MAX_HOST_IN_FLIGHT}"),
                );
            }
        }
        if self.memory_mb == 0 || self.memory_mb > MAX_MEMORY_MB {
            return bad(
                Rule::Permissions,
                format!("memory_mb must be 1 to {MAX_MEMORY_MB}"),
            );
        }
        for e in &self.events {
            if !EVENT_FAMILIES.contains(&e.as_str()) {
                return bad(Rule::Permissions, format!("unknown event family {e:?}"));
            }
        }
        if self.env_pass.len() > 32 || !self.env_pass.iter().all(|n| is_env_name(n)) {
            return bad(
                Rule::Permissions,
                "env_pass must be at most 32 environment variable names".into(),
            );
        }
        if self.connect_env_socket.len() > 32
            || !self
                .connect_env_socket
                .iter()
                .all(|n| self.env_pass.contains(n))
        {
            return bad(
                Rule::Permissions,
                "connect_env_socket must list at most 32 names from env_pass".into(),
            );
        }
        if self.network.hosts.len() > 64 || self.network.listen.len() > 16 {
            return bad(
                Rule::Permissions,
                "too many network.hosts or network.listen entries".into(),
            );
        }
        for h in &self.network.hosts {
            let ok = !h.is_empty()
                && h.len() <= 253
                && !has_control(h, false)
                && !h.chars().any(char::is_whitespace);
            if !ok {
                return bad(
                    Rule::Permissions,
                    format!("network.hosts entry {h:?} is not a host name"),
                );
            }
        }
        for l in &self.network.listen {
            if l.parse::<std::net::SocketAddr>().is_err() {
                return bad(
                    Rule::Permissions,
                    format!("network.listen entry {l:?} is not an address:port"),
                );
            }
        }
        for (field, list) in [("fs.read", &self.fs.read), ("fs.write", &self.fs.write)] {
            if list.len() > 32 {
                return bad(
                    Rule::Permissions,
                    format!("{field} has more than 32 entries"),
                );
            }
            for p in list {
                if field == "fs.write" && p == "any" {
                    return bad(
                        Rule::Permissions,
                        "fs.write must use $STATE, $CWD or an absolute path without '..'".into(),
                    );
                }
                if !valid_fs_entry(p) {
                    return bad(
                        Rule::Permissions,
                        format!("{field} entry {p:?} must be $STATE, $CWD, any or an absolute path without '..'"),
                    );
                }
            }
        }
        if self.spawn.names.len() > 32 || !self.spawn.names.iter().all(|n| is_program_name(n)) {
            return bad(
                Rule::Permissions,
                "spawn.names must be at most 32 bare program names".into(),
            );
        }
        Ok(())
    }
}

fn valid_fs_entry(p: &str) -> bool {
    if matches!(p, "$STATE" | "$CWD" | "any") {
        return true;
    }
    p.starts_with('/')
        && p.len() <= 512
        && !p.contains('\0')
        && !has_control(p, false)
        && !p.split('/').any(|c| c == "..")
}
