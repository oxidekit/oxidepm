//! IPC Protocol - Request/Response types

use chrono::{DateTime, Utc};
use oxidepm_core::{AppInfo, AppSpec, AppStatus, ErrorCode, Selector};
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 2;

/// The oldest protocol version this build still accepts for every request.
pub const PROTOCOL_MIN: u32 = 2;

/// The oldest protocol number a daemon accepts for [`Request::Hello`], [`Request::Ping`] and
/// [`Request::Kill`], so that a new CLI can still ask an older daemon to go away (and a new
/// daemon answers an older tool's `ping`). Every other request needs [`PROTOCOL_MIN`].
pub const PROTOCOL_CROSS_VERSION_MIN: u32 = 1;

/// What the daemon lists in `Hello.features`.
pub const FEATURES: &[&str] = &[
    "envelope",
    "item-results",
    "tagged-selector",
    "start-batch",
    "actor",
    "audit",
    "dry-run",
    "streaming",
];

/// One request line on the wire: the request plus who sent it and with what rights.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: u32,
    /// Chosen by the client and echoed in the response.
    pub request_id: String,
    /// What the caller says it is: `human:<label>` or `ai:<label>`. Only a claim; the daemon
    /// decides the class of the caller from the connection and uses the claim to be stricter.
    #[serde(default)]
    pub actor: Option<String>,
    /// Ask for environment values and hook scripts to be shown in `Show` and `Describe`.
    #[serde(default)]
    pub reveal: bool,
    /// Ask for a plan of what the request would do instead of doing it (`Response::DryRun`).
    /// A read-only request ignores it. Absent on the wire when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
    pub request: Request,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Envelope {
    pub fn new(request_id: impl Into<String>, request: Request) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.into(),
            actor: None,
            reveal: false,
            dry_run: false,
            request,
        }
    }
}

/// One response line on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol: u32,
    pub request_id: String,
    pub response: Response,
    /// More lines follow for this request: this one is a part of a streamed answer (the output
    /// of a relayed adapter verb). The last line of the answer has it false, and so has every
    /// ordinary answer. Absent on the wire when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub more: bool,
}

impl ResponseEnvelope {
    pub fn new(request_id: impl Into<String>, response: Response) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.into(),
            response,
            more: false,
        }
    }

    /// A part of a streamed answer; the client keeps reading until a line without `more`.
    pub fn part(request_id: impl Into<String>, response: Response) -> Self {
        Self {
            more: true,
            ..Self::new(request_id, response)
        }
    }
}

/// What the daemon is doing, as far as a client can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonPhase {
    /// Starting up: lock, database and supervisor are being prepared. A client cannot see this
    /// (the socket is bound after it); it is the daemon's own initial value.
    Booting,
    /// The socket answers while the saved apps are started in the background.
    Resurrecting { done: usize, total: usize },
    /// Serving requests.
    Ready,
    /// A shutdown has begun: new work is refused.
    ShuttingDown,
}

/// What happened to one app in a multi-app operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: u32,
    pub name: String,
    pub result: ItemOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ItemOutcome {
    /// The operation ran; `changed` is false when there was nothing to do.
    Ok {
        changed: bool,
        #[serde(default)]
        detail: Option<String>,
    },
    Err {
        code: ErrorCode,
        message: String,
    },
}

impl ItemResult {
    pub fn changed(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            result: ItemOutcome::Ok {
                changed: true,
                detail: None,
            },
        }
    }

    pub fn unchanged(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            result: ItemOutcome::Ok {
                changed: false,
                detail: None,
            },
        }
    }

    pub fn failed(id: u32, name: impl Into<String>, error: &oxidepm_core::Error) -> Self {
        Self {
            id,
            name: name.into(),
            result: ItemOutcome::Err {
                code: error.code(),
                message: error.to_string(),
            },
        }
    }

    pub fn is_changed(&self) -> bool {
        matches!(self.result, ItemOutcome::Ok { changed: true, .. })
    }
}

/// One installed adapter as `DaemonStatus` reports it: its state (`disabled`, `stopped`,
/// `starting`, `ready`, `backoff`, `degraded`, `stopping`, `failed`) and a note (for `failed`,
/// the reason and what the host found).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterStatus {
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What the kernel offers the adapter sandbox, as `DaemonStatus` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxStatus {
    /// The highest Landlock ABI, `None` without Landlock.
    pub landlock_abi: Option<u32>,
    /// Landlock can restrict connecting to pathname sockets.
    pub landlock_unix_connect: bool,
    pub seccomp: bool,
    pub no_new_privs: bool,
    /// The Yama `ptrace_scope`.
    pub ptrace_scope: Option<u8>,
    pub cgroup_delegated: bool,
    /// Whether the launcher `oxidepm-sandbox` is installed next to the daemon.
    pub launcher: bool,
    /// One line for people.
    pub summary: String,
}

/// How many items changed (what `count` reports).
pub fn changed_count(items: &[ItemResult]) -> usize {
    items.iter().filter(|i| i.is_changed()).count()
}

/// IPC Request from CLI to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Keep payloads inline in the typed IPC interface.
pub enum Request {
    /// Ask what the daemon is (version, pid, protocol range, features)
    Hello { client: String },

    /// Check if daemon is alive
    Ping,

    /// Start a new process
    Start { spec: AppSpec },

    /// Start a set of processes together: validated and checked as a graph first (a
    /// `depends_on` cycle or an unknown dependency refuses the apps concerned), registered, and
    /// then started, with the dependency wait ordering the launches. One result per spec.
    StartBatch { specs: Vec<AppSpec> },

    /// Stop process(es)
    Stop { selector: Selector },

    /// Restart process(es)
    Restart { selector: Selector },

    /// Delete process(es) from registry
    Delete { selector: Selector },

    /// Get status of all processes
    Status,

    /// Get detailed info for a process
    Show { selector: Selector },

    /// Get log lines
    Logs {
        selector: Selector,
        lines: usize,
        follow: bool,
        stdout: bool,
        stderr: bool,
    },

    /// Save current process list. Without `force`, a save that would replace a non-empty
    /// saved file with an empty list is refused (`E_SAVE_WOULD_EMPTY`).
    Save {
        #[serde(default)]
        force: bool,
    },

    /// Restore saved processes
    Resurrect,

    /// Stop daemon and all processes
    Kill,

    /// Graceful reload (zero-downtime restart)
    Reload { selector: Selector },

    /// Flush/truncate log files for process(es)
    Flush { selector: Selector },

    /// Describe a process (get what command would run)
    Describe { selector: Selector },

    /// Change the policy markers of apps without a restart (people only). A marker left out
    /// stays as it is; a cluster's instances follow their cluster.
    SetPolicy {
        selector: Selector,
        #[serde(default)]
        protected: Option<bool>,
        #[serde(default)]
        ai_control: Option<bool>,
    },

    /// How the daemon itself is: the state of the audit log, whether the agent socket is bound,
    /// and what the operator should know.
    DaemonStatus,

    /// Install, enable, disable, update, remove and list adapters. Everything that changes
    /// something is a person's request.
    Adapter(Box<crate::adapter::AdapterRequest>),
}

impl Request {
    /// The request's name on the wire (the `type` tag), for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Hello { .. } => "hello",
            Request::Ping => "ping",
            Request::Start { .. } => "start",
            Request::StartBatch { .. } => "start_batch",
            Request::Stop { .. } => "stop",
            Request::Restart { .. } => "restart",
            Request::Delete { .. } => "delete",
            Request::Status => "status",
            Request::Show { .. } => "show",
            Request::Logs { .. } => "logs",
            Request::Save { .. } => "save",
            Request::Resurrect => "resurrect",
            Request::Kill => "kill",
            Request::Reload { .. } => "reload",
            Request::Flush { .. } => "flush",
            Request::Describe { .. } => "describe",
            Request::SetPolicy { .. } => "set_policy",
            Request::DaemonStatus => "daemon_status",
            Request::Adapter(_) => "adapter",
        }
    }
}

impl Request {
    /// Whether a daemon accepts this request from a client that speaks an older protocol than
    /// its own (down to [`PROTOCOL_CROSS_VERSION_MIN`]).
    pub fn crosses_versions(&self) -> bool {
        matches!(self, Request::Hello { .. } | Request::Ping | Request::Kill)
    }
}

/// What the policy said about a request that was asked for as a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    /// The request would be carried out (as far as the plan says).
    Allow,
    /// The policy refuses the request; nothing else is planned.
    Deny { code: ErrorCode, message: String },
}

/// One thing a dry-run request would do (or refuse to do, or find already done).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedItem {
    /// The app's id; absent for an app that does not exist yet or a file.
    #[serde(default)]
    pub id: Option<u32>,
    pub name: String,
    /// What would happen: `stop`, `start`, `restart`, `delete`, `flush`, `noop`, `refuse`,
    /// `would_create`, `would_start`, `would_import`, `set_policy`, `save`.
    pub action: String,
    /// A sentence of detail. For `refuse` it starts with the error code (`E_APP_EXISTS: ...`).
    #[serde(default)]
    pub detail: Option<String>,
    /// The step in which this happens, counted from 0, for requests that act in layers
    /// (stop, kill, a batch start). Absent when there is no order.
    #[serde(default)]
    pub order: Option<u32>,
}

impl PlannedItem {
    pub fn new(id: Option<u32>, name: impl Into<String>, action: &str) -> Self {
        Self {
            id,
            name: name.into(),
            action: action.to_string(),
            detail: None,
            order: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn order(mut self, order: u32) -> Self {
        self.order = Some(order);
        self
    }
}

/// What a request would do, built from read-only state (`Response::DryRun`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunPlan {
    /// The request's name on the wire (`stop`, `start_batch`, ...).
    pub op: String,
    pub decision: PolicyDecision,
    #[serde(default)]
    pub items: Vec<PlannedItem>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl DryRunPlan {
    pub fn allowed(op: &str, items: Vec<PlannedItem>, warnings: Vec<String>) -> Self {
        Self {
            op: op.to_string(),
            decision: PolicyDecision::Allow,
            items,
            warnings,
        }
    }

    pub fn denied(op: &str, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            op: op.to_string(),
            decision: PolicyDecision::Deny {
                code,
                message: message.into(),
            },
            items: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// IPC Response from daemon to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Keep payloads inline in the typed IPC interface.
pub enum Response {
    /// Answer to `Hello`
    Hello {
        protocol_min: u32,
        protocol_max: u32,
        daemon_version: String,
        pid: u32,
        started_at: DateTime<Utc>,
        phase: DaemonPhase,
        features: Vec<String>,
    },

    /// Ping response
    Pong,

    /// Success with message
    Ok { message: String },

    /// Error with a code and a message
    Error {
        /// Absent in an older daemon's errors.
        #[serde(default)]
        code: ErrorCode,
        message: String,
    },

    /// Status response with all app info
    Status { apps: Vec<AppInfo> },

    /// Show response with single app detail
    Show { app: AppInfo },

    /// Log lines response
    LogLines { lines: Vec<String> },

    /// Start response with app ID. `child_ids` lists a cluster's instances.
    Started {
        id: u32,
        name: String,
        status: AppStatus,
        #[serde(default)]
        child_ids: Vec<u32>,
    },

    /// StartBatch response: one item per spec, in the order sent. An item that worked says in
    /// `detail` what status the app is in (`waiting` when it is held back by dependencies).
    StartedBatch { items: Vec<ItemResult> },

    /// Stop response
    Stopped {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
    },

    /// Restart response
    Restarted {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
    },

    /// Delete response
    Deleted {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
    },

    /// Save response
    Saved { count: usize, path: String },

    /// Resurrect response: apps started, apps imported from the saved file that were not in the
    /// registry, and the apps that could not be started (each with its reason).
    Resurrected {
        started: usize,
        #[serde(default)]
        imported: usize,
        #[serde(default)]
        failed: Vec<ItemResult>,
    },

    /// Reload response. `kind` says what a reload did: today always "restart".
    Reloaded {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
        kind: String,
    },

    /// Flush response
    Flushed {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
    },

    /// Describe response with app details
    Described {
        name: String,
        command: String,
        args: Vec<String>,
        cwd: String,
        env: std::collections::HashMap<String, String>,
        /// The runtime of the app (an older daemon called this field `mode`).
        #[serde(alias = "mode")]
        runtime: String,
    },

    /// SetPolicy response: one item per app named (a cluster is one item), `changed` when a
    /// marker was different, with the markers it has now in `detail`.
    PolicySet {
        count: usize,
        #[serde(default)]
        items: Vec<ItemResult>,
    },

    /// The answer to a request sent with `dry_run`: what it would do. Nothing was changed.
    DryRun { plan: DryRunPlan },

    /// The answer to an `adapter` request.
    Adapter(Box<crate::adapter::AdapterReply>),

    /// A part of the output of a relayed adapter verb (`more` is set on its envelope). `stream` is
    /// `stdout`, `stderr` or `status`.
    InvokeOutput { stream: String, text: String },

    /// The end of a relayed adapter verb: its exit code and the structured result it returned
    /// (shown by `--json`).
    InvokeDone {
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },

    /// DaemonStatus response. `audit` is `ok` or `degraded`; `audit_lost` counts the audit lines
    /// that could not be written since the daemon started; `agent_socket` says whether
    /// `agent.sock` is bound; `adapters` lists the installed adapters; `warnings` are sentences
    /// for the operator.
    DaemonStatus {
        audit: String,
        #[serde(default)]
        audit_lost: u64,
        agent_socket: bool,
        #[serde(default)]
        adapters: Vec<AdapterStatus>,
        #[serde(default)]
        warnings: Vec<String>,
        /// What the sandbox can enforce on this machine.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<SandboxStatus>,
    },
}

impl Response {
    pub fn ok<S: Into<String>>(message: S) -> Self {
        Response::Ok {
            message: message.into(),
        }
    }

    pub fn error<S: Into<String>>(code: ErrorCode, message: S) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }

    /// The response for a failed request: the code comes from the error.
    pub fn from_error(error: &oxidepm_core::Error) -> Self {
        Response::error(error.code(), error.to_string())
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Response::Error { message, .. } => Some(message),
            _ => None,
        }
    }

    pub fn error_code(&self) -> Option<&ErrorCode> {
        match self {
            Response::Error { code, .. } => Some(code),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Runtime;
    use std::path::PathBuf;

    #[test]
    fn test_request_serialize() {
        let req = Request::Start {
            spec: AppSpec::new(
                "test".to_string(),
                Runtime::from_static("node"),
                "app.js".to_string(),
                PathBuf::from("/app"),
            ),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("start"));
        assert!(json.contains("test"));
    }

    #[test]
    fn start_batch_round_trips_and_names_its_response() {
        let spec = AppSpec::new(
            "a".to_string(),
            Runtime::CMD,
            "true".to_string(),
            PathBuf::from("/"),
        );
        let json = serde_json::to_string(&Request::StartBatch { specs: vec![spec] }).unwrap();
        assert!(json.contains("\"type\":\"start_batch\""), "{json}");
        match serde_json::from_str::<Request>(&json).unwrap() {
            Request::StartBatch { specs } => assert_eq!(specs.len(), 1),
            other => panic!("wrong request {other:?}"),
        }
        let response = Response::StartedBatch {
            items: vec![ItemResult::changed(1, "a")],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"started_batch\""), "{json}");
        assert!(FEATURES.contains(&"start-batch"));
    }

    #[test]
    fn daemon_status_round_trips_and_has_the_wire_names() {
        let json = serde_json::to_string(&Request::DaemonStatus).unwrap();
        assert_eq!(json, "{\"type\":\"daemon_status\"}");
        assert_eq!(Request::DaemonStatus.name(), "daemon_status");
        let response = Response::DaemonStatus {
            audit: "degraded".into(),
            audit_lost: 3,
            agent_socket: true,
            adapters: vec![AdapterStatus {
                name: "check".into(),
                state: "failed".into(),
                note: Some("tampered: x".into()),
            }],
            warnings: vec!["w".into()],
            sandbox: Some(SandboxStatus {
                landlock_abi: Some(10),
                landlock_unix_connect: true,
                seccomp: true,
                no_new_privs: true,
                ptrace_scope: Some(1),
                cgroup_delegated: false,
                launcher: true,
                summary: "landlock abi 10, seccomp".into(),
            }),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"daemon_status\""), "{json}");
        match serde_json::from_str::<Response>(&json).unwrap() {
            Response::DaemonStatus {
                audit,
                audit_lost,
                agent_socket,
                adapters,
                warnings,
                sandbox,
            } => {
                assert_eq!(
                    (audit.as_str(), audit_lost, agent_socket, warnings.len()),
                    ("degraded", 3, true, 1)
                );
                assert_eq!(
                    sandbox.map(|s| (s.landlock_abi, s.launcher)),
                    Some((Some(10), true))
                );
                assert_eq!(adapters.len(), 1);
                assert_eq!(adapters[0].state, "failed");
            }
            other => panic!("{other:?}"),
        }
        // fields a newer daemon adds later are optional for the reader
        let bare: Response = serde_json::from_str(
            "{\"type\":\"daemon_status\",\"audit\":\"ok\",\"agent_socket\":false}",
        )
        .unwrap();
        assert!(
            matches!(&bare, Response::DaemonStatus { audit_lost: 0, adapters, sandbox: None, .. } if adapters.is_empty())
        );
    }

    #[test]
    fn test_response_serialize() {
        let resp = Response::ok("Process started");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("ok"));
        assert!(json.contains("Process started"));
    }

    #[test]
    fn test_selector_in_request() {
        let req = Request::Stop {
            selector: Selector::ByName("myapp".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Stop { selector } => {
                assert_eq!(selector, Selector::ByName("myapp".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_flush_request_serialize() {
        let req = Request::Flush {
            selector: Selector::All,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("flush"));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Flush { selector } => {
                assert_eq!(selector, Selector::All);
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_flushed_response_serialize() {
        let resp = Response::Flushed {
            count: 3,
            items: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("flushed"));
        assert!(json.contains("3"));
    }

    #[test]
    fn test_describe_request_serialize() {
        let req = Request::Describe {
            selector: Selector::ByName("myapp".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("describe"));
        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Describe { selector } => {
                assert_eq!(selector, Selector::ByName("myapp".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_described_response_serialize() {
        use std::collections::HashMap;
        let mut env = HashMap::new();
        env.insert("NODE_ENV".to_string(), "production".to_string());

        let resp = Response::Described {
            name: "myapp".to_string(),
            command: "node".to_string(),
            args: vec!["server.js".to_string()],
            cwd: "/app".to_string(),
            env,
            runtime: "node".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("described"));
        assert!(json.contains("myapp"));
        assert!(json.contains("node"));
        assert!(json.contains("NODE_ENV"));
    }
}
