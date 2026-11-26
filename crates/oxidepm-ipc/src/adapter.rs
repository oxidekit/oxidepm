//! The adapter requests of the CLI: install what the CLI staged, enable with
//! a grant, disable, remove, update, roll back, and the reads that list and describe. Everything
//! that changes something is a person's request (dangerous tier, never an adapter's or an AI's).
//!
//! The network, the signature check of what was downloaded, and the archive run in the CLI. The
//! daemon is handed a staged directory (by name, under its own `adapters/staging`) and the bytes of
//! the index the CLI used, and it checks again what it can.

use serde::{Deserialize, Serialize};

/// `Adapter(Install)`: move the directory the CLI staged into place and record the adapter,
/// disabled. A name that is installed already is an update.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallArgs {
    /// The directory name under `adapters/staging` (one plain component).
    pub staging: String,
    /// `index`, `bundled` or `pinned:<origin>`.
    pub source: String,
    /// The index as received and its detached signature: the daemon checks them again. Required
    /// for `index` and `bundled`.
    #[serde(default)]
    pub index_bytes: Option<String>,
    #[serde(default)]
    pub index_sig: Option<String>,
    /// `--allow-reserved-name`: an unsigned adapter may use a name the core reserves.
    #[serde(default)]
    pub allow_reserved_name: bool,
    /// `--allow-downgrade` (update).
    #[serde(default)]
    pub allow_downgrade: bool,
    /// `--switch-source` (update): the new release comes from another source than the installed one.
    #[serde(default)]
    pub switch_source: bool,
    /// The fingerprint the user accepted for an update that adds permissions.
    #[serde(default)]
    pub accept: Option<String>,
    /// The subcommands of the CLI, which no adapter verb may take.
    #[serde(default)]
    pub builtin_verbs: Vec<String>,
}

/// `Adapter(Enable)`: give the adapter its grant and start it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnableArgs {
    pub name: String,
    /// The fingerprint of the release the user accepted.
    #[serde(default)]
    pub accept: Option<String>,
    /// `--yes`: honoured only for the pure class.
    #[serde(default)]
    pub yes: bool,
    /// Lower the host level (`read`, `lifecycle`); never raised above the manifest's.
    #[serde(default)]
    pub level: Option<String>,
    /// Lower the scope (`opted-in`).
    #[serde(default)]
    pub scope: Option<String>,
    /// `--allow-unenforced`: accept a `requires_enforcement` item the self-test could not enforce.
    #[serde(default)]
    pub allow_unenforced: bool,
    /// The CLI runs on a terminal (an `admin` grant needs one).
    #[serde(default)]
    pub interactive: bool,
}

/// `Adapter(Invoke)`: run a verb that an adapter provides. The answer is streamed:
/// `Response::InvokeOutput` lines with `more` set, then `Response::InvokeDone`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InvokeArgs {
    /// The verb, as typed (`check`, `deploy`).
    pub verb: String,
    /// What follows the verb on the command line.
    #[serde(default)]
    pub argv: Vec<String>,
    /// The CLI's working directory, absolute. It becomes `$CWD` of the adapter's profile, so the
    /// daemon checks it again (an existing directory, not `/`, not above the home).
    pub cwd: String,
}

/// An `adapter` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdapterRequest {
    Install(Box<InstallArgs>),
    Enable(Box<EnableArgs>),
    Disable {
        name: String,
    },
    /// Delete the installed files and the row; keep the adapter's state directory unless `purge`.
    Remove {
        name: String,
        #[serde(default)]
        purge: bool,
    },
    /// Go back to the version before the last update, without the network.
    Rollback {
        name: String,
    },
    /// The installed adapters.
    List,
    /// One installed adapter.
    Info {
        name: String,
    },
    /// A fetched index: check it, remember its sequence, keep it as the cached index and disable
    /// what it revokes.
    IndexUpdate {
        index_bytes: String,
        index_sig: String,
    },
    /// Run a verb of an adapter (a relayed verb; streamed answer). Its tier is the verb's.
    Invoke(Box<InvokeArgs>),
    /// Open a session for a verb that needs the terminal (the answer is a
    /// [`AdapterReply::Session`]; the rest happens on the socket it names).
    SessionOpen(Box<InvokeArgs>),
}

impl AdapterRequest {
    /// The operation's name, for the audit log and the plan.
    pub fn op(&self) -> &'static str {
        match self {
            AdapterRequest::Install(_) => "adapter install",
            AdapterRequest::Enable(_) => "adapter enable",
            AdapterRequest::Disable { .. } => "adapter disable",
            AdapterRequest::Remove { .. } => "adapter remove",
            AdapterRequest::Rollback { .. } => "adapter rollback",
            AdapterRequest::List => "adapter list",
            AdapterRequest::Info { .. } => "adapter info",
            AdapterRequest::IndexUpdate { .. } => "adapter index",
            AdapterRequest::Invoke(_) => "adapter invoke",
            AdapterRequest::SessionOpen(_) => "adapter session",
        }
    }

    /// Whether the request only reads.
    pub fn is_read(&self) -> bool {
        matches!(self, AdapterRequest::List | AdapterRequest::Info { .. })
    }

    /// Whether the request can take a long time (the CLI waits for the slow timeout).
    pub fn is_slow(&self) -> bool {
        !self.is_read()
    }
}

/// One installed adapter, as `list` and `info` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub name: String,
    pub kind: String,
    pub version: String,
    pub summary: String,
    /// `index`, `bundled` or `pinned:<origin>`.
    pub source: String,
    /// `signed:<key id>` or `pinned`.
    pub trust: String,
    pub enabled: bool,
    /// The state the host reports: `disabled`, `ready`, `failed`, ... or `needs_reinstall`.
    pub state: String,
    #[serde(default)]
    pub note: Option<String>,
    /// The fingerprint of the installed release (what `enable --accept` takes).
    pub fingerprint: String,
    pub exe_sha256: String,
    /// The manifest as stored (the CLI renders the permissions from it).
    pub manifest_json: String,
    #[serde(default)]
    pub grant_json: Option<String>,
    #[serde(default)]
    pub enforced_json: Option<String>,
    #[serde(default)]
    pub prev_version: Option<String>,
    /// The cached index says this version is yanked, with the reason.
    #[serde(default)]
    pub yanked: Option<String>,
    /// The last lines the adapter wrote to stderr (not sanitized; the CLI does that).
    #[serde(default)]
    pub stderr_tail: Vec<String>,
    /// Crashes the host recorded and protocol strikes of the current connection.
    #[serde(default)]
    pub crashes: u32,
    #[serde(default)]
    pub strikes: u64,
    pub installed_at: String,
}

/// The daemon's answer to an `adapter` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdapterReply {
    /// Installed (or updated). Nothing is running: an installed adapter is disabled.
    Installed {
        name: String,
        version: String,
        fingerprint: String,
        /// The version it replaced, for an update.
        #[serde(default)]
        replaced: Option<String>,
        message: String,
        #[serde(default)]
        warnings: Vec<String>,
    },
    /// Enabled: the grant stored and what the self-test enforced.
    Enabled {
        name: String,
        fingerprint: String,
        level: String,
        scope: String,
        #[serde(default)]
        enforced: Vec<String>,
        #[serde(default)]
        unix_note: Option<String>,
        message: String,
    },
    /// Done (disable, remove, rollback).
    Done {
        message: String,
        #[serde(default)]
        warnings: Vec<String>,
    },
    List {
        adapters: Vec<AdapterInfo>,
    },
    Info {
        adapter: Box<AdapterInfo>,
    },
    /// An index was accepted: its sequence, the key that signed it, and the adapters it revoked.
    IndexAccepted {
        sequence: u64,
        key_id: String,
        #[serde(default)]
        disabled: Vec<String>,
    },
    /// A session was opened: attach to it as the offer says.
    Session(crate::session::SessionOffer),
}

/// One verb in the table the daemon writes for the CLI (`adapters/verbs.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbTableEntry {
    pub verb: String,
    pub adapter: String,
    #[serde(default)]
    pub summary: String,
    /// `safe`, `caution` or `dangerous`.
    #[serde(default)]
    pub tier: String,
    /// `relayed` or `session`.
    #[serde(default)]
    pub mode: String,
}

/// The file `{home}/adapters/verbs.json`: the verbs of the enabled adapters. The CLI reads it when
/// clap does not know a command, without asking the daemon; it only chooses what to ask for, and
/// the daemon resolves the verb again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbTable {
    #[serde(default)]
    pub verbs: Vec<VerbTableEntry>,
}
