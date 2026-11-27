//! The manifest: `manifest.json` in the archive. The archive's sha256 is in the signed
//! index, so the manifest is authenticated through it. Validation here runs at import and again
//! at every load in the daemon.

use std::io::Read as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::codes;
use crate::limits::{MANIFEST_MAX_BYTES, SUPPORTED_PROTOCOLS};
use crate::permissions::{added, growth, Growth, NetworkMode, Permissions, SpawnMode};
use crate::text::{env_key_denied, has_control, hex, is_env_name, is_name, is_program_name};

/// Names no adapter may take, whoever signed it.
pub const FIXED_RESERVED_NAMES: &[&str] = &[
    "core", "oxidepm", "oxidepmd", "all", "adapter", "daemon", "cmd", "system", "host",
];

const MAX_SUMMARY: usize = 120;
const MAX_DESCRIPTION: usize = 4096;
const MAX_VERB_RUNTIME_SECS: u32 = 3600;
const DEFAULT_VERB_RUNTIME_SECS: u32 = 600;

/// Which rule a manifest or index broke. All of them are `E_ADAPTER_INVALID` except the two
/// signature and freshness ones, which have their own codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Size,
    Parse,
    Schema,
    NameCharset,
    FixedReservedName,
    ReservedName,
    ControlCharacters,
    TextLength,
    Version,
    Protocol,
    MinDaemon,
    ExecRelative,
    ExecFile,
    ExecNotNative,
    VerbName,
    VerbCollision,
    VerbWritesTier,
    VerbSafeButSpawning,
    VerbRuntime,
    RuntimeName,
    RuntimeNeedsPrograms,
    ProgramName,
    EnvSet,
    KindMismatch,
    Permissions,
    Limit,
    // Index rules (index.rs).
    IndexSchema,
    IndexUrl,
    IndexHash,
    IndexDate,
    IndexStale,
    /// The signature of the index (signing.rs).
    Signature,
}

impl Rule {
    /// The stable error code that goes with the rule.
    pub fn error_code(self) -> &'static str {
        match self {
            Rule::IndexStale => codes::E_INDEX_STALE,
            Rule::Signature => codes::E_SIGNATURE_INVALID,
            _ => codes::E_ADAPTER_INVALID,
        }
    }
}

/// A manifest or index that breaks a rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct Invalid {
    pub rule: Rule,
    pub detail: String,
}

pub(crate) fn invalid<T>(rule: Rule, detail: impl Into<String>) -> Result<T, Invalid> {
    Err(Invalid {
        rule,
        detail: detail.into(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Service,
    Tool,
}

/// The policy tier of a verb. A higher tier is guarded more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerbTier {
    Safe,
    Caution,
    Dangerous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerbMode {
    #[default]
    Relayed,
    Session,
}

fn default_verb_runtime() -> u32 {
    DEFAULT_VERB_RUNTIME_SECS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verb {
    pub name: String,
    pub summary: String,
    pub tier: VerbTier,
    #[serde(default)]
    pub mode: VerbMode,
    #[serde(default)]
    pub writes: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_verb_runtime")]
    pub max_runtime_secs: u32,
    #[serde(default)]
    pub args_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Provides {
    #[serde(default)]
    pub runtimes: Vec<String>,
    #[serde(default)]
    pub verbs: Vec<Verb>,
    #[serde(default)]
    pub serves: Vec<String>,
    #[serde(default)]
    pub programs: Vec<String>,
    #[serde(default)]
    pub capture_programs: Vec<String>,
    #[serde(default)]
    pub env_set: Vec<String>,
}

impl Provides {
    fn normalized(&self) -> Provides {
        fn sorted(v: &[String]) -> Vec<String> {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        }
        let mut verbs = self.verbs.clone();
        verbs.sort_by(|a, b| a.name.cmp(&b.name));
        Provides {
            runtimes: sorted(&self.runtimes),
            verbs,
            serves: sorted(&self.serves),
            programs: sorted(&self.programs),
            capture_programs: sorted(&self.capture_programs),
            env_set: sorted(&self.env_set),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    pub summary: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub protocol: Vec<u32>,
    pub min_daemon: String,
    pub exec: String,
    #[serde(default)]
    pub provides: Provides,
    pub permissions: Permissions,
    /// The adapter acts on behalf of an AI. Only the field exists in protocol 1; the rules that
    /// use it arrive with the AI adapters.
    #[serde(default)]
    pub ai_driven: bool,
}

/// What validation needs to know about the machine and the source. The reserved-name lists are
/// the core's hint tables; the core passes them in.
#[derive(Debug, Clone)]
pub struct ValidateCtx<'a> {
    pub daemon_version: semver::Version,
    pub protocols: &'a [u32],
    /// The archive is covered by a signature from an embedded key.
    pub signed_by_embedded_key: bool,
    /// The human passed `--allow-reserved-name`.
    pub allow_reserved_name: bool,
    pub reserved_adapters: &'a [&'a str],
    pub reserved_runtimes: &'a [&'a str],
    pub reserved_verbs: &'a [&'a str],
    /// Subcommands of the CLI, generated from its definition.
    pub builtin_verbs: &'a [&'a str],
    /// `(adapter, verb)` of the adapters already installed.
    pub installed_verbs: &'a [(&'a str, &'a str)],
}

impl<'a> ValidateCtx<'a> {
    pub fn new(daemon_version: semver::Version) -> Self {
        ValidateCtx {
            daemon_version,
            protocols: SUPPORTED_PROTOCOLS,
            signed_by_embedded_key: false,
            allow_reserved_name: false,
            reserved_adapters: &[],
            reserved_runtimes: &[],
            reserved_verbs: &[],
            builtin_verbs: &[],
            installed_verbs: &[],
        }
    }
}

fn check_text(field: &str, s: &str, max: usize, allow_newline: bool) -> Result<(), Invalid> {
    if s.chars().count() > max {
        return invalid(
            Rule::TextLength,
            format!("{field} is longer than {max} characters"),
        );
    }
    if has_control(s, allow_newline) {
        return invalid(
            Rule::ControlCharacters,
            format!("{field} holds a control character"),
        );
    }
    Ok(())
}

impl Manifest {
    /// Parse `manifest.json`: the size cap, the JSON, the schema number. No other rule.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, Invalid> {
        if bytes.len() > MANIFEST_MAX_BYTES {
            return invalid(
                Rule::Size,
                format!("the manifest is larger than {MANIFEST_MAX_BYTES} bytes"),
            );
        }
        let m: Manifest = serde_json::from_slice(bytes).map_err(|e| Invalid {
            rule: Rule::Parse,
            detail: e.to_string(),
        })?;
        if m.schema != 1 {
            return invalid(
                Rule::Schema,
                format!("manifest schema {} is not supported", m.schema),
            );
        }
        Ok(m)
    }

    /// Parse and validate.
    pub fn parse_validated(bytes: &[u8], ctx: &ValidateCtx) -> Result<Manifest, Invalid> {
        let m = Manifest::parse(bytes)?;
        m.validate(ctx)?;
        Ok(m)
    }

    /// Validate the manifest without inspecting the archive. [`Manifest::check_exec_file`]
    /// does the one that needs it.
    pub fn validate(&self, ctx: &ValidateCtx) -> Result<(), Invalid> {
        if self.schema != 1 {
            return invalid(
                Rule::Schema,
                format!("manifest schema {} is not supported", self.schema),
            );
        }
        if !is_name(&self.name, 2, 32, true) {
            return invalid(
                Rule::NameCharset,
                format!(
                    "adapter name {:?} must match ^[a-z][a-z0-9-]{{1,31}}$",
                    self.name
                ),
            );
        }
        if FIXED_RESERVED_NAMES.contains(&self.name.as_str()) {
            return invalid(
                Rule::FixedReservedName,
                format!("adapter name {:?} is reserved", self.name),
            );
        }
        if semver::Version::parse(&self.version).is_err() {
            return invalid(
                Rule::Version,
                format!("version {:?} is not a semantic version", self.version),
            );
        }
        check_text("summary", &self.summary, MAX_SUMMARY, false)?;
        if self.summary.trim().is_empty() {
            return invalid(Rule::TextLength, "summary is empty");
        }
        check_text("description", &self.description, MAX_DESCRIPTION, true)?;
        if self.tags.len() > 10 {
            return invalid(Rule::Limit, "more than 10 tags");
        }
        for t in &self.tags {
            if t.is_empty() {
                return invalid(Rule::TextLength, "a tag is empty");
            }
            check_text("tag", t, 32, false)?;
        }

        if self.protocol.is_empty() || !self.protocol.iter().any(|p| ctx.protocols.contains(p)) {
            return invalid(
                Rule::Protocol,
                format!(
                    "the adapter speaks protocol {:?}; this daemon speaks {:?}",
                    self.protocol, ctx.protocols
                ),
            );
        }
        match semver::Version::parse(&self.min_daemon) {
            Err(_) => {
                return invalid(
                    Rule::MinDaemon,
                    format!("min_daemon {:?} is not a semantic version", self.min_daemon),
                )
            }
            Ok(min) if min > ctx.daemon_version => {
                return invalid(
                    Rule::MinDaemon,
                    format!(
                        "it needs daemon {min} or newer; this is {}",
                        ctx.daemon_version
                    ),
                )
            }
            Ok(_) => {}
        }
        self.check_exec_path()?;

        self.validate_provides()?;
        self.permissions.validate()?;
        self.validate_kind_rules()?;
        self.validate_verbs(ctx)?;
        self.validate_reserved(ctx)?;
        Ok(())
    }

    fn check_exec_path(&self) -> Result<(), Invalid> {
        let e = &self.exec;
        let bad = |why: &str| invalid(Rule::ExecRelative, format!("exec {e:?} {why}"));
        if e.is_empty() || e.len() > 256 || e.contains('\0') || has_control(e, false) {
            return bad("is not a usable path");
        }
        if e.starts_with('/') {
            return bad("must be a relative path");
        }
        // Path::components would quietly drop a '.' in the middle, so look at the text itself.
        if e.split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return bad("must not contain empty, '.' or '..' components");
        }
        Ok(())
    }

    fn validate_provides(&self) -> Result<(), Invalid> {
        let p = &self.provides;
        if p.runtimes.len() > 16
            || p.verbs.len() > 32
            || p.serves.len() > 16
            || p.programs.len() > 32
            || p.env_set.len() > 32
        {
            return invalid(Rule::Limit, "too many entries in provides");
        }
        for r in &p.runtimes {
            if !is_name(r, 2, 16, false) || r == "cmd" {
                return invalid(
                    Rule::RuntimeName,
                    format!("runtime name {r:?} must match ^[a-z][a-z0-9]{{1,15}}$ and not be cmd"),
                );
            }
        }
        if has_dup(&p.runtimes) {
            return invalid(Rule::RuntimeName, "a runtime is listed twice");
        }
        if !p.runtimes.is_empty() && p.programs.is_empty() {
            return invalid(
                Rule::RuntimeNeedsPrograms,
                "a runtime adapter must list provides.programs",
            );
        }
        if let Some(bad) = p.programs.iter().find(|n| !is_program_name(n)) {
            return invalid(
                Rule::ProgramName,
                format!("program {bad:?} is not a bare file name"),
            );
        }
        if let Some(bad) = p.capture_programs.iter().find(|n| !p.programs.contains(n)) {
            return invalid(
                Rule::ProgramName,
                format!("capture program {bad:?} is not in provides.programs"),
            );
        }
        for k in &p.env_set {
            if !is_env_name(k) {
                return invalid(
                    Rule::EnvSet,
                    format!("env_set key {k:?} is not an environment variable name"),
                );
            }
            if env_key_denied(k) {
                return invalid(
                    Rule::EnvSet,
                    format!("env_set key {k:?} may never be set by an adapter"),
                );
            }
        }
        for s in &p.serves {
            let ok = !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_');
            if !ok {
                return invalid(
                    Rule::Limit,
                    format!("serves entry {s:?} is not a method name"),
                );
            }
        }
        Ok(())
    }

    fn validate_kind_rules(&self) -> Result<(), Invalid> {
        match self.kind {
            Kind::Tool if !self.permissions.events.is_empty() => invalid(
                Rule::KindMismatch,
                "a tool receives no events; only a service lists events",
            ),
            Kind::Service if !self.provides.runtimes.is_empty() => invalid(
                Rule::KindMismatch,
                "a service provides no runtimes; only a tool lists runtimes",
            ),
            _ => Ok(()),
        }
    }

    fn validate_verbs(&self, ctx: &ValidateCtx) -> Result<(), Invalid> {
        let external_access = self.permissions.spawn.mode != SpawnMode::None
            || self.permissions.network.mode != NetworkMode::None
            || !self.permissions.connect_env_socket.is_empty();
        let mut seen: Vec<&str> = Vec::new();
        for v in &self.provides.verbs {
            if !is_name(&v.name, 2, 24, true) {
                return invalid(
                    Rule::VerbName,
                    format!("verb name {:?} must match ^[a-z][a-z0-9-]{{1,23}}$", v.name),
                );
            }
            if seen.contains(&v.name.as_str()) {
                return invalid(Rule::VerbName, format!("verb {:?} is listed twice", v.name));
            }
            seen.push(&v.name);
            check_text("verb summary", &v.summary, MAX_SUMMARY, false)?;
            check_text("verb args_summary", &v.args_summary, MAX_SUMMARY, false)?;
            if v.max_runtime_secs == 0 || v.max_runtime_secs > MAX_VERB_RUNTIME_SECS {
                return invalid(
                    Rule::VerbRuntime,
                    format!(
                        "verb {:?}: max_runtime_secs must be 1 to {MAX_VERB_RUNTIME_SECS}",
                        v.name
                    ),
                );
            }
            if ctx.builtin_verbs.contains(&v.name.as_str()) {
                return invalid(
                    Rule::VerbCollision,
                    format!("verb {:?} is a built-in command", v.name),
                );
            }
            if let Some((other, _)) = ctx
                .installed_verbs
                .iter()
                .find(|(a, n)| *n == v.name && *a != self.name)
            {
                return invalid(
                    Rule::VerbCollision,
                    format!(
                        "verb {:?} is already provided by the adapter {other:?}",
                        v.name
                    ),
                );
            }
            if v.writes && v.tier == VerbTier::Safe {
                return invalid(
                    Rule::VerbWritesTier,
                    format!(
                        "verb {:?} writes, so its tier must be caution or dangerous",
                        v.name
                    ),
                );
            }
            if external_access && v.tier == VerbTier::Safe {
                return invalid(
                    Rule::VerbSafeButSpawning,
                    format!("verb {:?}: an adapter that may spawn programs, use the network or connect to environment sockets may not declare a safe verb", v.name),
                );
            }
        }
        Ok(())
    }

    fn validate_reserved(&self, ctx: &ValidateCtx) -> Result<(), Invalid> {
        if ctx.signed_by_embedded_key || ctx.allow_reserved_name {
            return Ok(());
        }
        if ctx.reserved_adapters.contains(&self.name.as_str()) {
            return invalid(
                Rule::ReservedName,
                format!(
                    "the adapter name {:?} is reserved to adapters signed by an official key",
                    self.name
                ),
            );
        }
        if let Some(r) = self
            .provides
            .runtimes
            .iter()
            .find(|r| ctx.reserved_runtimes.contains(&r.as_str()))
        {
            return invalid(
                Rule::ReservedName,
                format!("the runtime name {r:?} is reserved to adapters signed by an official key"),
            );
        }
        if let Some(v) = self
            .provides
            .verbs
            .iter()
            .find(|v| ctx.reserved_verbs.contains(&v.name.as_str()))
        {
            return invalid(
                Rule::ReservedName,
                format!(
                    "the verb name {:?} is reserved to adapters signed by an official key",
                    v.name
                ),
            );
        }
        Ok(())
    }

    /// The executable must exist under `root` (the extracted archive), be a regular file, not
    /// reached through a symbolic link, and start with the ELF magic. A `#!`
    /// script cannot be executed from a descriptor.
    pub fn check_exec_file(&self, root: &Path) -> Result<(), Invalid> {
        self.check_exec_path()?;
        let mut path = root.to_path_buf();
        for c in Path::new(&self.exec).components() {
            path.push(c);
            let meta = std::fs::symlink_metadata(&path).map_err(|e| Invalid {
                rule: Rule::ExecFile,
                detail: format!("exec {:?}: {e}", self.exec),
            })?;
            if meta.file_type().is_symlink() {
                return invalid(
                    Rule::ExecFile,
                    format!("exec {:?} goes through a symbolic link", self.exec),
                );
            }
        }
        let meta = std::fs::symlink_metadata(&path).map_err(|e| Invalid {
            rule: Rule::ExecFile,
            detail: e.to_string(),
        })?;
        if !meta.is_file() {
            return invalid(
                Rule::ExecFile,
                format!("exec {:?} is not a regular file", self.exec),
            );
        }
        let mut magic = [0u8; 4];
        let read = std::fs::File::open(&path).and_then(|mut f| f.read_exact(&mut magic));
        if read.is_err() || magic != *b"\x7fELF" {
            return invalid(
                Rule::ExecNotNative,
                format!("exec {:?} is not a native executable (a script cannot be executed from a descriptor)", self.exec),
            );
        }
        Ok(())
    }

    /// Every way `self` (the new version) is more powerful than `old` (the installed one): the
    /// permission fields and the fields whose change widens power less obviously. An empty
    /// list means the existing grant can be kept.
    pub fn grows_over(&self, old: &Manifest) -> Vec<Growth> {
        let mut out = self.permissions.grows_over(&old.permissions);
        if self.kind != old.kind {
            out.push(growth(
                "kind",
                format!("{:?} to {:?}", old.kind, self.kind).to_lowercase(),
            ));
        }
        if self.exec != old.exec {
            out.push(growth(
                "exec",
                format!("{} instead of {}", self.exec, old.exec),
            ));
        }
        let new_protocols: Vec<String> = self.protocol.iter().map(u32::to_string).collect();
        let old_protocols: Vec<String> = old.protocol.iter().map(u32::to_string).collect();
        let extra = added(&old_protocols, &new_protocols);
        if !extra.is_empty() {
            out.push(growth(
                "protocol",
                format!("adds protocol {}", extra.join(", ")),
            ));
        }
        for (field, o, n) in [
            (
                "provides.runtimes",
                &old.provides.runtimes,
                &self.provides.runtimes,
            ),
            (
                "provides.serves",
                &old.provides.serves,
                &self.provides.serves,
            ),
            (
                "provides.programs",
                &old.provides.programs,
                &self.provides.programs,
            ),
            (
                "provides.capture_programs",
                &old.provides.capture_programs,
                &self.provides.capture_programs,
            ),
            (
                "provides.env_set",
                &old.provides.env_set,
                &self.provides.env_set,
            ),
        ] {
            let extra = added(o, n);
            if !extra.is_empty() {
                out.push(growth(field, format!("adds {}", extra.join(", "))));
            }
        }
        let old_verb_names: Vec<String> =
            old.provides.verbs.iter().map(|v| v.name.clone()).collect();
        let new_verb_names: Vec<String> =
            self.provides.verbs.iter().map(|v| v.name.clone()).collect();
        let extra = added(&old_verb_names, &new_verb_names);
        if !extra.is_empty() {
            out.push(growth(
                "provides.verbs",
                format!("adds {}", extra.join(", ")),
            ));
        }
        for v in &self.provides.verbs {
            if let Some(o) = old.provides.verbs.iter().find(|o| o.name == v.name) {
                if v.tier < o.tier {
                    out.push(growth(
                        "verb.tier",
                        format!(
                            "verb {} is now {:?} instead of {:?}",
                            v.name, v.tier, o.tier
                        )
                        .to_lowercase(),
                    ));
                }
                if v.writes && !o.writes {
                    out.push(growth("verb.writes", format!("verb {} now writes", v.name)));
                }
            }
        }
        if old.ai_driven && !self.ai_driven {
            out.push(growth(
                "ai_driven",
                "no longer marked ai_driven, which drops the ai_control requirement",
            ));
        }
        out
    }

    /// The consent fingerprint: the first 16 hex digits of the sha256 of the serialised
    /// `{name, version, exe_sha256, kind, exec, protocol, provides, permissions}` (and `ai_driven`,
    /// which changes who may drive the adapter). Lists are sorted first, so the order of entries in
    /// the manifest does not matter; the release is covered, not only the permissions, so a
    /// different release with the same permissions does not satisfy an old `--accept`.
    pub fn fingerprint(&self, exe_sha256: &str) -> String {
        #[derive(Serialize)]
        struct View<'a> {
            name: &'a str,
            version: &'a str,
            exe_sha256: String,
            kind: Kind,
            exec: &'a str,
            protocol: Vec<u32>,
            provides: Provides,
            permissions: Permissions,
            ai_driven: bool,
        }
        let mut protocol = self.protocol.clone();
        protocol.sort_unstable();
        protocol.dedup();
        let view = View {
            name: &self.name,
            version: &self.version,
            exe_sha256: exe_sha256.to_ascii_lowercase(),
            kind: self.kind,
            exec: &self.exec,
            protocol,
            provides: self.provides.normalized(),
            permissions: self.permissions.normalized(),
            ai_driven: self.ai_driven,
        };
        let bytes = serde_json::to_vec(&view).unwrap_or_default();
        hex(&Sha256::digest(&bytes))[..16].to_string()
    }
}

fn has_dup(v: &[String]) -> bool {
    let mut s: Vec<&String> = v.iter().collect();
    s.sort();
    s.windows(2).any(|w| w[0] == w[1])
}
