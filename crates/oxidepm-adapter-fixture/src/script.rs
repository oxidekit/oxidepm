//! The script: a list of rules, evaluated in order.
//!
//! ```json
//! [
//!   {"on": "start", "do": "ignore_sigterm"},
//!   {"on": "runtime.prepare", "times": 1, "do": [{"sleep": {"ms": 200}}, {"reply": {"result": {"steps": []}}}]},
//!   {"on": "ping", "after": 2, "do": {"crash": {"code": 3}}}
//! ]
//! ```
//!
//! `on` is `start`, a method name (`initialize`, `ping`, `shutdown`, `event`, `cancel`,
//! `runtime.prepare`, ...), `stdin_eof` or `after_ms:N` (N milliseconds after the process
//! started). `do` is one action or a list of actions; an action with no parameters can be written
//! as its name alone. For one trigger the first rule that applies fires and nothing else does:
//! `after: n` makes a rule wait until its n-th match is past, `times: n` stops it after n
//! firings, and then the next rule for the same trigger gets its turn. A request that no rule
//! handles gets the default (well-behaved) answer. A request that a rule handles is answered only
//! if the rule says so (`reply`, `error`, ...).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What starts a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum On {
    Start,
    StdinEof,
    AfterMs(u64),
    /// A request or notification from the daemon with this method.
    Method(String),
}

impl On {
    pub fn parse(text: &str) -> Result<On, String> {
        if text == "start" {
            return Ok(On::Start);
        }
        if text == "stdin_eof" {
            return Ok(On::StdinEof);
        }
        if let Some(n) = text.strip_prefix("after_ms:") {
            return n
                .parse()
                .map(On::AfterMs)
                .map_err(|_| format!("bad delay in {text:?}"));
        }
        if text.is_empty()
            || text.len() > 64
            || !text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(format!("bad trigger {text:?}"));
        }
        Ok(On::Method(text.to_string()))
    }
}

/// A program to start: one string split on spaces (no shell), or an argument vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    Line(String),
    Argv(Vec<String>),
}

impl Action {
    /// The behaviour's name, as written in a script.
    pub fn name(&self) -> String {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

impl Cmd {
    pub fn argv(&self) -> Vec<String> {
        match self {
            Cmd::Line(line) => line.split_whitespace().map(str::to_string).collect(),
            Cmd::Argv(argv) => argv.clone(),
        }
    }
}

fn default_message() -> String {
    "fixture error".to_string()
}

/// One action the fixture can take.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Answer the request. Without `result` the well-behaved answer for the method is used.
    Reply {
        #[serde(default)]
        result: Option<Map<String, Value>>,
    },
    /// Answer the request with an error. `name` goes to `data.code`; the usual name of the
    /// numeric code is used when it has one.
    Error {
        code: i64,
        #[serde(default = "default_message")]
        message: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Wait, then run the rest of the list (in the background, so the fixture keeps reading).
    Sleep {
        ms: u64,
    },
    /// Read and never answer.
    Hang {},
    /// Stop reading stdin.
    Deaf {},
    /// Exit with a code.
    Crash {
        code: i32,
    },
    /// Die by SIGABRT (core dumps are switched off first).
    Abort {},
    /// Exit normally.
    ExitClean {},
    IgnoreSigterm {},
    /// Do not answer `shutdown` and do not exit when stdin closes.
    IgnoreShutdown {},
    /// Start a child that outlives the fixture and stays in its process group.
    SpawnGrandchild {
        cmd: Cmd,
    },
    /// Like `spawn_grandchild`, but the child leaves the process group (`setsid`).
    SpawnSetsidGrandchild {
        cmd: Cmd,
    },
    /// Valid notification frames, `fps` per second (0 = as fast as possible), each about `bytes`
    /// long; `count` frames (default `fps`, at least 1; at most 1 000 000). `junk` sends lines
    /// that are not JSON instead.
    FloodStdout {
        fps: u32,
        bytes: usize,
        #[serde(default)]
        count: Option<u64>,
        #[serde(default)]
        junk: bool,
    },
    /// Junk lines on stderr, `bps` bytes per second for `secs` seconds (default 1).
    FloodStderr {
        bps: u64,
        #[serde(default)]
        secs: Option<u64>,
    },
    /// Report progress of the `command.invoke` being handled: `count` `invoke.output` frames
    /// (default 1) on `stream` (`stdout`, `stderr` or `status`; default `stdout`), each holding
    /// `text` (default "progress") or `bytes` bytes of filler, `every_ms` apart.
    InvokeOutput {
        #[serde(default)]
        stream: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        count: Option<u32>,
        #[serde(default)]
        bytes: Option<usize>,
        #[serde(default)]
        every_ms: Option<u64>,
    },
    /// One line of this text on stderr (the escape sequences of the tests are written as \u001b).
    StderrText {
        text: String,
    },
    /// One valid notification of exactly this many bytes (newline excluded).
    GiantLine {
        bytes: u64,
    },
    Garbage {
        text: String,
    },
    InvalidUtf8 {},
    /// A line with a NUL byte in it.
    Nul {},
    /// A batch: a JSON array holding a frame.
    ArrayFrame {},
    /// Half a line, then the pipe is closed.
    PartialLineThenEof {},
    /// Answer with an id that was never issued (`id`, or the request id plus a million).
    WrongId {
        #[serde(default)]
        id: Option<u64>,
    },
    /// Answer the request twice.
    DuplicateReply {},
    /// A response to a request nobody made.
    UnsolicitedResponse {
        id: u64,
    },
    /// Answer `initialize` with another name, version or protocol.
    LieName {
        #[serde(default)]
        name: Option<String>,
    },
    LieVersion {
        #[serde(default)]
        version: Option<String>,
    },
    LieProtocol {
        #[serde(default)]
        protocol: Option<u32>,
    },
    /// Send `host.request` and record the answer. `end_user` is the label the daemon writes to
    /// its audit log; `repeat` sends the same request that many times (default 1) without
    /// waiting for the answers.
    HostRequest {
        request: Value,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        end_user: Option<String>,
        #[serde(default)]
        repeat: Option<u32>,
    },
    /// Send `host.request` with forged `actor`, `reveal` and `dry_run` fields in the request
    /// and in the params (the request defaults to `{"type":"ping"}`).
    ForgeActor {
        actor: String,
        #[serde(default)]
        request: Option<Value>,
    },
    /// Open a Unix socket by path, send one line (default: a `ping` envelope), record the answer.
    ConnectSocket {
        path: String,
        #[serde(default)]
        send: Option<String>,
    },
    /// The same, from a child process of the fixture.
    DialFromChild {
        path: String,
        #[serde(default)]
        send: Option<String>,
    },
    /// A child connects, writes and exits at once, without reading the answer.
    ConnectWriteExit {
        path: String,
        #[serde(default)]
        send: Option<String>,
    },
    EchoEnv {},
    EchoFds {},
    EchoLimits {},
    EchoNnp {},
    /// Create a file.
    Touch {
        path: String,
    },
    /// Open a TCP connection.
    InetConnect {
        addr: String,
    },
    /// Start a program and wait for it.
    Exec {
        path: String,
    },
    /// Hold `mb` MiB of touched memory.
    Alloc {
        mb: u64,
    },
    /// Spin on one thread until it has used `ms` milliseconds of CPU time (default: for ever).
    BusyLoop {
        #[serde(default)]
        ms: Option<u64>,
    },
    /// Call `host.audit_note` and record the answer.
    AuditNote {
        text: String,
    },
    /// After a `cancel` for the request being handled, answer it with error -32024.
    ReplyCancelledAfterCancel {},
    /// Run `sh -c cmd` and record what happened.
    ExecShell {
        cmd: String,
    },
    /// Fork children that do nothing until `max` (default 1000, at most 4096) forks were made or
    /// one failed; hold them `hold_ms`, record the count, then kill them. A flat, bounded
    /// stand-in for a fork bomb (the process cap and the seccomp rule are what is tested).
    ForkBomb {
        #[serde(default)]
        max: Option<u32>,
        #[serde(default)]
        hold_ms: Option<u64>,
    },
    /// Fork `count` children that each hold `mb` MiB of touched memory for `hold_ms`, record the
    /// result and kill them: memory spread over a process tree (the group-wide sampler's case).
    AllocChildren {
        count: u32,
        mb: u64,
        #[serde(default)]
        hold_ms: Option<u64>,
    },
    /// Open a file for reading and record the outcome.
    ReadFile {
        path: String,
    },
    /// List a directory and record the outcome.
    ListDir {
        path: String,
    },
    /// Create a TCP socket (no connection) and record the outcome.
    InetSocket {},
    /// Call `io_uring_setup` and record the outcome.
    IoUringSetup {},
    /// Connect to a pathname AF_UNIX socket and record the outcome.
    UnixConnect {
        path: String,
    },
    /// Connect to an abstract AF_UNIX socket (the name without the leading NUL).
    AbstractConnect {
        name: String,
    },
    /// Send a signal (0 only checks) to a process and record the outcome.
    Signal {
        pid: i32,
        signal: i32,
    },
    /// Create an AF_UNIX socket (no connection) and record the outcome.
    UnixSocket {},
    /// Replace the process by a program with `execve` (no fork). Only a refusal returns, and is recorded.
    Execve {
        path: String,
    },
    /// Bind a TCP listener on `addr` and record the outcome.
    TcpBind {
        addr: String,
    },
    /// Record `Pid`, `NoNewPrivs` and `Seccomp` from `/proc/self/status`, and the process id.
    EchoStatus {},
    /// Record how the terminal looks to this process (a session verb has one): whether
    /// descriptors 0 and 1 are terminals, whether `/dev/tty` can be opened, the process group,
    /// the session, the foreground group of descriptor 0 and the size of the window on
    /// descriptor 1.
    TerminalReport {},
    /// Read `lines` lines (default 1) from standard input and write `echo:<line>` to standard
    /// output for each; record what was read. In a session these two are the terminal.
    TerminalEcho {
        #[serde(default)]
        lines: Option<u32>,
    },
    /// Put the terminal on standard input in raw mode and leave it there (a program that was
    /// killed before it could restore it).
    TerminalRaw {},
    /// From now on count `SIGWINCH`; `report_winch` records the count.
    WatchWinch {},
    ReportWinch {},
    /// Write 1 MiB files into the state directory (or `dir`) until `mb` MiB are written or a
    /// write fails.
    FillStateDir {
        mb: u64,
        #[serde(default)]
        dir: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub on: On,
    pub times: Option<u32>,
    pub after: u32,
    /// The rule applies only to a process whose executable path contains this text (the install
    /// directory names the version, so a script can make one version misbehave).
    pub only_in: Option<String>,
    pub actions: Vec<Action>,
}

#[derive(Deserialize)]
struct RuleRepr {
    on: String,
    #[serde(default)]
    times: Option<u32>,
    #[serde(default)]
    after: u32,
    #[serde(default)]
    only_in: Option<String>,
    #[serde(rename = "do")]
    actions: Value,
}

/// `"hang"` means `{"hang": {}}`.
fn parse_action(value: Value) -> Result<Action, String> {
    let value = match value {
        Value::String(name) => {
            let mut map = Map::new();
            map.insert(name, Value::Object(Map::new()));
            Value::Object(map)
        }
        other => other,
    };
    serde_json::from_value(value).map_err(|e| e.to_string())
}

impl Rule {
    fn from_value(value: Value) -> Result<Rule, String> {
        let repr: RuleRepr = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let items = match repr.actions {
            Value::Array(items) => items,
            single => vec![single],
        };
        let actions = items
            .into_iter()
            .map(parse_action)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Rule {
            on: On::parse(&repr.on)?,
            times: repr.times,
            after: repr.after,
            only_in: repr.only_in,
            actions,
        })
    }
}

/// A parsed script.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Script {
    pub rules: Vec<Rule>,
}

impl Script {
    pub fn parse(text: &str) -> Result<Script, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("the script is not JSON: {e}"))?;
        let Value::Array(items) = value else {
            return Err("the script must be a JSON array of rules".to_string());
        };
        let mut rules = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            rules.push(Rule::from_value(item).map_err(|e| format!("rule {i}: {e}"))?);
        }
        Ok(Script { rules })
    }

    /// The distinct `after_ms:N` delays, for the timers.
    pub fn delays(&self) -> Vec<u64> {
        let mut seen: BTreeMap<u64, ()> = BTreeMap::new();
        for rule in &self.rules {
            if let On::AfterMs(n) = rule.on {
                seen.insert(n, ());
            }
        }
        seen.into_keys().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_parse() {
        assert_eq!(On::parse("start"), Ok(On::Start));
        assert_eq!(On::parse("stdin_eof"), Ok(On::StdinEof));
        assert_eq!(On::parse("after_ms:250"), Ok(On::AfterMs(250)));
        assert_eq!(
            On::parse("runtime.prepare"),
            Ok(On::Method("runtime.prepare".into()))
        );
        assert!(On::parse("after_ms:x").is_err());
        assert!(On::parse("").is_err());
        assert!(On::parse("has space").is_err());
    }

    #[test]
    fn an_action_is_a_name_or_an_object_and_do_is_one_or_a_list() {
        let script = Script::parse(
            r#"[{"on":"ping","do":"hang"},
                {"on":"start","times":2,"after":1,"do":[{"sleep":{"ms":5}},{"crash":{"code":3}}]}]"#,
        )
        .unwrap();
        assert_eq!(script.rules[0].actions, vec![Action::Hang {}]);
        assert_eq!(script.rules[1].times, Some(2));
        assert_eq!(script.rules[1].after, 1);
        assert_eq!(
            script.rules[1].actions,
            vec![Action::Sleep { ms: 5 }, Action::Crash { code: 3 }]
        );
    }

    #[test]
    fn a_bad_script_names_the_rule() {
        assert!(Script::parse("{}").unwrap_err().contains("array"));
        assert!(Script::parse(r#"[{"on":"ping","do":"no_such_thing"}]"#)
            .unwrap_err()
            .starts_with("rule 0:"));
        assert!(Script::parse(r#"[{"on":"ping","do":{"crash":{}}}]"#).is_err());
        assert!(Script::parse(r#"[{"do":"hang"}]"#).is_err());
    }

    #[test]
    fn delays_are_distinct_and_sorted() {
        let script = Script::parse(
            r#"[{"on":"after_ms:50","do":"hang"},{"on":"after_ms:10","do":"hang"},{"on":"after_ms:50","do":"hang"}]"#,
        )
        .unwrap();
        assert_eq!(script.delays(), vec![10, 50]);
    }

    #[test]
    fn a_cmd_is_a_line_or_an_argv() {
        let one: Cmd = serde_json::from_str(r#""sleep 30""#).unwrap();
        let two: Cmd = serde_json::from_str(r#"["sleep","30"]"#).unwrap();
        assert_eq!(one.argv(), two.argv());
    }
}
