//! Typed `params` and `result` objects for protocol messages. Unknown fields are
//! ignored on input (forward compatibility); optional fields are left out on output.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::limits::InitializeLimits;
use crate::manifest::Kind;
use crate::permissions::{HostLevel, HostScope};

/// Method names.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const PING: &str = "ping";
    pub const SHUTDOWN: &str = "shutdown";
    pub const RUNTIME_PREPARE: &str = "runtime.prepare";
    pub const RUNTIME_PLAN: &str = "runtime.plan";
    pub const COMMAND_INVOKE: &str = "command.invoke";
    pub const EVENT: &str = "event";
    pub const CANCEL: &str = "cancel";
    pub const HOST_REQUEST: &str = "host.request";
    pub const HOST_AUDIT_NOTE: &str = "host.audit_note";
    pub const INVOKE_OUTPUT: &str = "invoke.output";
}

/// Namespaces reserved in protocol 1: `provider.*` and `gate.*`.
pub const RESERVED_METHOD_PREFIXES: &[&str] = &["provider.", "gate."];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameVersion {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub level: HostLevel,
    pub scope: HostScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitPaths {
    pub state: String,
}

/// `initialize` request (daemon to adapter).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocols: Vec<u32>,
    pub host: NameVersion,
    pub adapter: NameVersion,
    pub kind: Kind,
    pub grant: Grant,
    pub paths: InitPaths,
    pub limits: InitializeLimits,
}

/// `initialize` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol: u32,
    pub name: String,
    pub version: String,
}

/// The redacted view of an app. `env` holds the keys with the value `"<redacted>"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecView {
    pub name: String,
    pub runtime: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub instances: u32,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// That adapter's own `adapters.<name>` options (at most 4 KiB, checked at validation).
    #[serde(default)]
    pub options: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Start,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrepareParams {
    pub spec: SpecView,
    pub path_hint: String,
    pub phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capture {
    #[default]
    None,
    Stdout,
    Stderr,
    Both,
}

/// A build step. The core, not the adapter, runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_add: BTreeMap<String, String>,
    pub timeout_secs: u64,
    #[serde(default)]
    pub capture: Capture,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_max: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrepareResult {
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepDone {
    pub id: String,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanParams {
    pub spec: SpecView,
    #[serde(default)]
    pub steps_done: Vec<StepDone>,
}

/// The launch plan a runtime adapter returns (validated by the core).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanResult {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorView {
    pub class: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeParams {
    pub verb: String,
    #[serde(default)]
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub dry_run: bool,
    pub actor: ActorView,
    #[serde(default)]
    pub tty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResult {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventApp {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventParams {
    pub seq: u64,
    pub kind: String,
    pub family: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<EventApp>,
    #[serde(default)]
    pub data: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_before: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelParams {
    pub id: u64,
}

/// The longest `end_user` label of a `host.request`.
pub const MAX_END_USER_CHARS: usize = 64;

/// `host.request`: exactly the object the CLI would send. The IPC `Request` type lives in another
/// crate, so it travels as a JSON value here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostRequestParams {
    pub request: Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Who the adapter says is behind the call (its own end user), at most 64 characters. The
    /// daemon writes it to the audit log as an unverified label and never decides on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostRequestResult {
    pub response: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditNoteParams {
    /// The existing note payload. The daemon records a fixed audit message for the event.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeOutputParams {
    pub id: u64,
    pub stream: OutputStream,
    pub text: String,
}
