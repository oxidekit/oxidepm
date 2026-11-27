//! The audit log: a record of what an adapter or an AI did or tried, and of every caution or
//! dangerous request by anyone. Not a metrics stream.
//!
//! Two files in `{home}/audit/`: `audit.jsonl` (everything recorded) and `audit-security.jsonl`
//! (denials, dangerous requests, what non-humans changed), so that a chatty but permitted actor
//! cannot rotate the evidence of earlier denials away. One JSON object per line, at most 4 KiB.
//!
//! A record is a closed struct ([`Record`]): it has no field that could hold an environment
//! value, a hook or probe script, an argument vector, a log line or an app spec, so none of them
//! can get in by accident. Free text (a denial's reason, a note) is cut to a fixed length and
//! goes through `oxidepm_core::scrub_message` where the caller knows values to hide.
//!
//! One writer thread owns each file ([`AuditWriter`]). Senders never block on it: they queue a
//! record with `try_send`, and a request that needs the record on disk before it runs waits for
//! the writer's acknowledgement for a bounded time. The directory is created by the writer when
//! it first writes (not by `Paths::ensure_dirs`), so a home whose audit path is unusable still
//! boots. A write that fails is counted, the writer tries again every few seconds, and the
//! lines it could not write are reported by one `dropped` line when it recovers.

use chrono::{SecondsFormat, Utc};
use oxidepm_core::actor::{ActorClass, ActorId};
use oxidepm_core::{Error, Paths, Tier};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

/// The `v` of every record.
pub const RECORD_VERSION: u32 = 1;

/// Longest line, in bytes, the newline excluded.
pub const MAX_LINE: usize = 4096;

/// Records that may wait for a writer.
pub const QUEUE_CAPACITY: usize = 4096;

/// How long a request that needs its record on disk waits for the writer.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a writer that failed tries to open its file again.
pub const REOPEN_INTERVAL: Duration = Duration::from_secs(5);

/// How often written records are forced to disk when nobody waits for them.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// How long a shutdown waits for the queue to drain.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// Rotation of `audit.jsonl`: at 10 MiB, ten files.
pub const MAIN_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotation of `audit-security.jsonl`: at 5 MiB, ten files.
pub const SECURITY_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Files kept per log, the live one included.
pub const MAX_FILES: usize = 10;

/// Security records for non-human refusals share a burst and a refill, independent
/// of the request limiter and of the records for allowed actions.
const REFUSAL_BURST: f64 = 10.0;
const REFUSALS_PER_SECOND: f64 = 1.0 / 60.0;

/// Targets named in one record; beyond that only the count.
pub const MAX_TARGETS: usize = 50;
/// Characters of a denial's reason.
pub const MAX_WHY_CHARS: usize = 200;
/// Characters of the client's own request id.
pub const MAX_CLIENT_REQ_CHARS: usize = 64;
/// Characters of a note an adapter wrote.
pub const MAX_NOTE_CHARS: usize = 256;
/// Characters of an app name, a selector, a program path or an operation name in a record.
pub const MAX_NAME_CHARS: usize = 100;
/// Longest `end_user` label of an adapter's request.
pub const MAX_END_USER_CHARS: usize = oxidepm_adapter_proto::messages::MAX_END_USER_CHARS;

/// Which kind of line a record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The policy's answer to a request, written before the request runs.
    Decision,
    /// What the request did.
    Result,
    /// A rehearsal's decision (`--dry-run`).
    DryRun,
    /// A note an adapter wrote, or the daemon's own remark (lines lost).
    Note,
    /// An adapter's life: imported, enabled, started, killed...
    Adapter,
    /// A launch plan a runtime adapter returned.
    Plan,
    /// How many requests of one actor were refused for their rate.
    RateLimited,
}

/// Who asked, as the daemon decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorView {
    pub class: String,
    pub label: String,
    /// `main`, `agent`, `adapter`, `internal`, or `unattributed`.
    pub via: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uid: Option<u32>,
}

impl ActorView {
    /// The actor as the log writes it.
    pub fn of(actor: &ActorId, pid: Option<i32>, uid: Option<u32>) -> Self {
        Self {
            class: actor.class.as_str().to_string(),
            label: actor.label.to_string(),
            via: actor.via.as_str().to_string(),
            pid,
            uid,
        }
    }
}

/// An app a request reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Absent for an app that does not exist yet (a start).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<u32>,
    pub name: String,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deny {
    pub code: String,
    pub why: String,
}

/// An adapter's life event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterLine {
    pub name: String,
    /// `imported`, `enabled`, `disabled`, `updated`, `removed`, `started`, `exited`, `killed`...
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<String>,
    /// The first 12 hex digits of the executable's sha256.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sha12: Option<String>,
}

/// One line of the audit log. Every field but `v`, `seq`, `ts` and `phase` is absent when it does
/// not apply. There is deliberately no field for environment values, scripts, the argument
/// vector, log text, the app spec or an adapter's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    /// Monotonic within one file series (across rotation); the writer assigns it.
    pub seq: u64,
    pub ts: String,
    pub phase: Phase,
    /// The daemon's own number for a request; it links the decision to the result.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub req: Option<String>,
    /// The id the client chose, kept only as a label (a client could reuse another's `req`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_req: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actor: Option<ActorView>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub targets: Option<Vec<Target>>,
    /// How many more targets there were than `targets` names.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub targets_more: Option<u64>,
    /// `allow` or `deny`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub deny: Option<Deny>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dry_run: Option<bool>,
    /// The program a start or a launch plan runs (its path, never its arguments).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub program: Option<String>,
    /// How many arguments the program gets.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub argc: Option<u64>,
    /// sha256 of the argument vector: the arguments themselves are not written.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub args_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ms: Option<u64>,
    /// Requests folded into this line (coalesced polling, rate-limited requests).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last: Option<String>,
    /// Lines that could not be written before this one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub adapter: Option<AdapterLine>,
    /// The label an adapter gave for its own end user (`host.request`): unverified, at most 64
    /// characters, written for the reader of the log and used in no decision.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_user: Option<String>,
    /// The text of a note.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
}

/// The names of every key a record may have, nested ones included.
pub const ALLOWED_KEYS: &[&str] = &[
    "v",
    "seq",
    "ts",
    "phase",
    "req",
    "client_req",
    "actor",
    "class",
    "label",
    "via",
    "pid",
    "uid",
    "op",
    "tier",
    "selector",
    "targets",
    "id",
    "name",
    "targets_more",
    "decision",
    "deny",
    "code",
    "why",
    "dry_run",
    "program",
    "argc",
    "args_sha256",
    "ok",
    "changed",
    "failed",
    "ms",
    "count",
    "first",
    "last",
    "dropped",
    "adapter",
    "event",
    "version",
    "sha12",
    "end_user",
    "text",
];

/// `s` cut to `max` characters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

/// The current time as the log writes it: `2026-11-02T09:14:07.512Z`.
pub fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Record {
    /// An empty record of kind `phase`, stamped now.
    pub fn new(phase: Phase) -> Self {
        Self {
            v: RECORD_VERSION,
            seq: 0,
            ts: now_ts(),
            phase,
            req: None,
            client_req: None,
            actor: None,
            op: None,
            tier: None,
            selector: None,
            targets: None,
            targets_more: None,
            decision: None,
            deny: None,
            dry_run: None,
            program: None,
            argc: None,
            args_sha256: None,
            ok: None,
            code: None,
            changed: None,
            failed: None,
            ms: None,
            count: None,
            first: None,
            last: None,
            dropped: None,
            adapter: None,
            end_user: None,
            text: None,
        }
    }

    /// The record as one line (no newline), at most [`MAX_LINE`] bytes. Free text is cut at its
    /// own limit; if the line is still too long the targets are dropped from the end and counted
    /// in `targets_more`, then the longer strings are cut down.
    pub fn to_line(&self) -> String {
        let mut record = self.clone();
        record.clip_text();
        let mut line = serde_json::to_string(&record).unwrap_or_default();
        while line.len() > MAX_LINE {
            let Some(targets) = record.targets.as_mut().filter(|t| !t.is_empty()) else {
                break;
            };
            targets.pop();
            record.targets_more = Some(record.targets_more.unwrap_or(0) + 1);
            line = serde_json::to_string(&record).unwrap_or_default();
        }
        if line.len() > MAX_LINE {
            // Nothing left to drop: the strings are the problem. Cut them to a fraction.
            for field in [
                &mut record.text,
                &mut record.selector,
                &mut record.program,
                &mut record.client_req,
            ] {
                if let Some(s) = field.as_mut() {
                    *s = truncate_chars(s, 40);
                }
            }
            if let Some(deny) = record.deny.as_mut() {
                deny.why = truncate_chars(&deny.why, 80);
            }
            line = serde_json::to_string(&record).unwrap_or_default();
        }
        line
    }

    /// Cut every free-text field to its limit.
    fn clip_text(&mut self) {
        if let Some(s) = self.client_req.as_mut() {
            *s = truncate_chars(s, MAX_CLIENT_REQ_CHARS);
        }
        if let Some(s) = self.text.as_mut() {
            *s = truncate_chars(s, MAX_NOTE_CHARS);
        }
        if let Some(s) = self.end_user.as_mut() {
            *s = truncate_chars(s, MAX_END_USER_CHARS);
        }
        for s in [&mut self.selector, &mut self.program, &mut self.op]
            .into_iter()
            .flatten()
        {
            *s = truncate_chars(s, MAX_NAME_CHARS);
        }
        if let Some(deny) = self.deny.as_mut() {
            deny.why = truncate_chars(&deny.why, MAX_WHY_CHARS);
        }
        if let Some(targets) = self.targets.as_mut() {
            if targets.len() > MAX_TARGETS {
                let extra = (targets.len() - MAX_TARGETS) as u64;
                targets.truncate(MAX_TARGETS);
                self.targets_more = Some(self.targets_more.unwrap_or(0) + extra);
            }
            for target in targets.iter_mut() {
                target.name = truncate_chars(&target.name, MAX_NAME_CHARS);
            }
        }
    }
}

// ---- the files -----------------------------------------------------------------------------

/// `audit.jsonl` and `3` give `audit.3.jsonl`.
fn rotated_name(file: &str, index: usize) -> String {
    match file.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.{index}.{ext}"),
        None => format!("{file}.{index}"),
    }
}

/// The last `seq` in the newest file of the series that has a whole line; torn last lines (the
/// daemon died mid-write) are skipped. 0 when there is none.
pub fn recover_seq(dir: &Path, file: &str, max_files: usize) -> u64 {
    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u64,
    }
    for index in 0..max_files {
        let name = if index == 0 {
            file.to_string()
        } else {
            rotated_name(file, index)
        };
        let path = dir.join(name);
        // only a regular file: opening a named pipe nobody writes to would block the boot
        if !std::fs::metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(mut f) = File::open(path) else {
            continue;
        };
        let Ok(len) = f.metadata().map(|m| m.len()) else {
            continue;
        };
        let take = len.min(4 * MAX_LINE as u64 * 4);
        if f.seek(SeekFrom::Start(len - take)).is_err() {
            continue;
        }
        let mut tail = Vec::with_capacity(take as usize);
        if f.take(take).read_to_end(&mut tail).is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&tail);
        if let Some(seq) = text
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<SeqOnly>(l).ok().map(|r| r.seq))
        {
            return seq;
        }
    }
    0
}

/// How a writer is set up. The defaults are the constants above; tests shrink them.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    pub dir: PathBuf,
    /// The live file's name inside `dir`.
    pub file: String,
    pub max_bytes: u64,
    pub max_files: usize,
    pub queue: usize,
    /// How soon after a failure the writer tries to open its file again.
    pub retry: Duration,
    pub sync_interval: Duration,
}

impl WriterConfig {
    /// `audit.jsonl` under `dir`.
    pub fn main(dir: impl Into<PathBuf>) -> Self {
        Self::with(dir, oxidepm_core::constants::AUDIT_LOG_FILE, MAIN_MAX_BYTES)
    }

    /// `audit-security.jsonl` under `dir`.
    pub fn security(dir: impl Into<PathBuf>) -> Self {
        Self::with(
            dir,
            oxidepm_core::constants::AUDIT_SECURITY_FILE,
            SECURITY_MAX_BYTES,
        )
    }

    fn with(dir: impl Into<PathBuf>, file: &str, max_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            file: file.to_string(),
            max_bytes,
            max_files: MAX_FILES,
            queue: QUEUE_CAPACITY,
            retry: REOPEN_INTERVAL,
            sync_interval: SYNC_INTERVAL,
        }
    }
}

/// What the senders and the writer share.
#[derive(Default)]
struct Health {
    /// The last write failed (or a record was lost) and none has succeeded since.
    failing: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// Lines lost and not yet reported by a `dropped` line.
    unreported: AtomicU64,
    /// Lines lost since the daemon started.
    lost: AtomicU64,
}

/// A way for a test to hold the writer between two records.
#[derive(Default)]
struct Pause {
    paused: Mutex<bool>,
    changed: Condvar,
}

impl Pause {
    fn wait(&self) {
        let mut paused = self.paused.lock();
        while *paused {
            self.changed.wait(&mut paused);
        }
    }
}

/// While it lives the writer does not take a new record. Dropping it lets the writer go.
#[cfg(any(test, feature = "test-hooks"))]
pub struct PauseGuard(Arc<Pause>);

#[cfg(any(test, feature = "test-hooks"))]
impl Drop for PauseGuard {
    fn drop(&mut self) {
        *self.0.paused.lock() = false;
        self.0.changed.notify_all();
    }
}

enum Msg {
    Record(Box<Record>, Option<oneshot::Sender<bool>>),
    Close(oneshot::Sender<()>),
}

/// Called by the writer thread when it is idle (and with `true` when it is closing): the records
/// it returns are written. It is how folded lines (coalesced polling, rate limits) leave memory.
pub type Ticker = Box<dyn FnMut(bool) -> Vec<Record> + Send>;

/// Why a record that was waited for did not get to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The queue was full or the writer is gone.
    Queue,
    /// The writer did not answer in time.
    Timeout,
    /// The writer tried and could not write.
    Failed,
}

/// The handle to one file's writer thread.
pub struct AuditWriter {
    tx: SyncSender<Msg>,
    health: Arc<Health>,
    #[cfg(any(test, feature = "test-hooks"))]
    pause: Arc<Pause>,
    file: String,
}

impl AuditWriter {
    /// Start the writer thread for `config`. The directory is not touched until the first record.
    pub fn start(config: WriterConfig) -> Self {
        Self::start_with(config, None)
    }

    /// Like [`start`](Self::start), with a [`Ticker`] the writer thread calls when idle.
    pub fn start_with(config: WriterConfig, ticker: Option<Ticker>) -> Self {
        let (tx, rx) = sync_channel(config.queue.max(1));
        let health = Arc::new(Health::default());
        let pause = Arc::new(Pause::default());
        let seq = recover_seq(&config.dir, &config.file, config.max_files);
        let file = config.file.clone();
        let mut state = Writer {
            config,
            file: None,
            size: 0,
            seq,
            last_failure: None,
            last_sync: Instant::now(),
            dirty: false,
            ticker,
            health: Arc::clone(&health),
            pause: Arc::clone(&pause),
        };
        let name = format!("audit-{}", file);
        let spawned = std::thread::Builder::new()
            .name(name)
            .spawn(move || state.run(rx));
        if let Err(e) = spawned {
            error!("Could not start the audit writer for {}: {}", file, e);
            health.failing.store(true, Ordering::SeqCst);
            *health.last_error.lock() = Some(format!("the writer thread did not start: {e}"));
        }
        Self {
            tx,
            health,
            #[cfg(any(test, feature = "test-hooks"))]
            pause,
            file,
        }
    }

    /// Queue `record` and go on. False when the queue is full or the writer is gone; the loss is
    /// counted and reported when the writer is back.
    pub fn enqueue(&self, record: Record) -> bool {
        match self.tx.try_send(Msg::Record(Box::new(record), None)) {
            Ok(()) => true,
            Err(e) => {
                self.lose(&e);
                false
            }
        }
    }

    /// Queue `record` and wait up to `timeout` until it is on disk. The record may still be
    /// written after a timeout.
    pub async fn write_acked(&self, record: Record, timeout: Duration) -> Result<(), WriteError> {
        let (ack, answer) = oneshot::channel();
        if let Err(e) = self.tx.try_send(Msg::Record(Box::new(record), Some(ack))) {
            self.lose(&e);
            return Err(WriteError::Queue);
        }
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) | Ok(Err(_)) => Err(WriteError::Failed),
            Err(_) => Err(WriteError::Timeout),
        }
    }

    fn lose(&self, why: &TrySendError<Msg>) {
        self.health.unreported.fetch_add(1, Ordering::SeqCst);
        self.health.lost.fetch_add(1, Ordering::SeqCst);
        self.health.failing.store(true, Ordering::SeqCst);
        let reason = match why {
            TrySendError::Full(_) => "the audit queue is full",
            TrySendError::Disconnected(_) => "the audit writer is not running",
        };
        let mut last = self.health.last_error.lock();
        if last.is_none() {
            warn!("Audit {}: {}", self.file, reason);
        }
        *last = Some(reason.to_string());
    }

    /// Drain the queue (records queued before this call are written) and stop the writer, waiting
    /// at most `budget`. A writer blocked in the file system cannot hold the caller longer.
    pub async fn close(&self, budget: Duration) -> bool {
        let (done, wait) = oneshot::channel();
        if self.tx.try_send(Msg::Close(done)).is_err() {
            return false;
        }
        matches!(tokio::time::timeout(budget, wait).await, Ok(Ok(())))
    }

    /// The last write failed or a record was lost, and nothing has been written since.
    pub fn is_failing(&self) -> bool {
        self.health.failing.load(Ordering::SeqCst)
    }

    /// What went wrong last, while [`is_failing`](Self::is_failing).
    pub fn last_error(&self) -> Option<String> {
        self.health.last_error.lock().clone()
    }

    /// Lines lost since the daemon started.
    pub fn lost(&self) -> u64 {
        self.health.lost.load(Ordering::SeqCst)
    }

    /// Hold the writer until the guard is dropped.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn pause(&self) -> PauseGuard {
        *self.pause.paused.lock() = true;
        PauseGuard(Arc::clone(&self.pause))
    }
}

/// The writer thread's own state.
struct Writer {
    config: WriterConfig,
    file: Option<File>,
    size: u64,
    seq: u64,
    /// When the file last failed to open or write; `None` while all is well.
    last_failure: Option<Instant>,
    last_sync: Instant,
    /// Bytes were written since the last `fdatasync`.
    dirty: bool,
    ticker: Option<Ticker>,
    health: Arc<Health>,
    pause: Arc<Pause>,
}

impl Writer {
    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(self.config.sync_interval) {
                Ok(msg) => {
                    self.pause.wait();
                    match msg {
                        Msg::Record(record, ack) => {
                            let waited = ack.is_some();
                            let ok = self.handle(*record, waited);
                            if let Some(ack) = ack {
                                let _ = ack.send(ok);
                            }
                        }
                        Msg::Close(done) => {
                            self.tick(true);
                            self.sync_if_dirty(true);
                            let _ = done.send(());
                            return;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.tick(false);
                    self.sync_if_dirty(false);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.tick(true);
                    self.sync_if_dirty(true);
                    return;
                }
            }
        }
    }

    /// Write the folded lines that are due (all of them when `all`).
    fn tick(&mut self, all: bool) {
        let Some(mut ticker) = self.ticker.take() else {
            return;
        };
        for record in ticker(all) {
            self.handle(record, false);
        }
        self.ticker = Some(ticker);
    }

    /// Write one record. False when it could not be written (it is counted as lost).
    fn handle(&mut self, record: Record, waited: bool) -> bool {
        match self.write(record, waited) {
            Ok(()) => {
                if self.health.failing.swap(false, Ordering::SeqCst) {
                    info!("Audit {} is writable again", self.config.file);
                }
                *self.health.last_error.lock() = None;
                true
            }
            Err(e) => {
                self.file = None;
                self.last_failure = Some(Instant::now());
                self.health.unreported.fetch_add(1, Ordering::SeqCst);
                self.health.lost.fetch_add(1, Ordering::SeqCst);
                if !self.health.failing.swap(true, Ordering::SeqCst) {
                    warn!("Audit {} cannot be written: {}", self.config.file, e);
                }
                *self.health.last_error.lock() = Some(e);
                false
            }
        }
    }

    fn write(&mut self, mut record: Record, waited: bool) -> Result<(), String> {
        self.ensure_open()?;
        let lost = self.health.unreported.swap(0, Ordering::SeqCst);
        if lost > 0 {
            let mut note = Record::new(Phase::Note);
            note.dropped = Some(lost);
            if let Err(e) = self.append(note) {
                self.health.unreported.fetch_add(lost, Ordering::SeqCst);
                return Err(e);
            }
        }
        record.seq = 0;
        self.append(record)?;
        if waited || self.last_sync.elapsed() >= self.config.sync_interval {
            self.sync()?;
        }
        Ok(())
    }

    /// Give `record` the next `seq` and write it as one line.
    fn append(&mut self, mut record: Record) -> Result<(), String> {
        let seq = self.seq + 1;
        record.seq = seq;
        let mut line = record.to_line();
        line.push('\n');
        if self.size > 0 && self.size + line.len() as u64 > self.config.max_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or("the audit file is not open")?;
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        self.seq = seq;
        self.size += line.len() as u64;
        self.dirty = true;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), String> {
        if let Some(file) = self.file.as_ref() {
            file.sync_data().map_err(|e| e.to_string())?;
        }
        self.last_sync = Instant::now();
        self.dirty = false;
        Ok(())
    }

    fn sync_if_dirty(&mut self, force: bool) {
        if self.dirty && (force || self.last_sync.elapsed() >= self.config.sync_interval) {
            if let Err(e) = self.sync() {
                warn!("Audit {} could not be synced: {}", self.config.file, e);
                self.file = None;
                self.last_failure = Some(Instant::now());
            }
        }
    }

    /// Open the live file, creating the directory (0700) and the file (0600) if need be. After a
    /// failure this tries again only when `retry` has passed.
    fn ensure_open(&mut self) -> Result<(), String> {
        if self.file.is_some() {
            return Ok(());
        }
        if let Some(failed) = self.last_failure {
            if failed.elapsed() < self.config.retry {
                return Err(self
                    .health
                    .last_error
                    .lock()
                    .clone()
                    .unwrap_or_else(|| "the audit file is not writable".to_string()));
            }
        }
        let dir = &self.config.dir;
        match std::fs::metadata(dir) {
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("{} exists and is not a directory", dir.display()));
            }
            Ok(_) => {}
            Err(_) => {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
        }
        let path = dir.join(&self.config.file);
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        // a torn last line (the daemon died mid-write) must not swallow the next record
        if size > 0 && !ends_with_newline(&path) {
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        self.size = file.metadata().map(|m| m.len()).unwrap_or(size);
        self.file = Some(file);
        self.last_failure = None;
        Ok(())
    }

    /// `audit.jsonl` becomes `audit.1.jsonl`, the older ones move up and the oldest is deleted.
    fn rotate(&mut self) -> Result<(), String> {
        self.file = None;
        let dir = &self.config.dir;
        let name = &self.config.file;
        for index in (1..self.config.max_files.saturating_sub(1).max(1)).rev() {
            let from = dir.join(rotated_name(name, index));
            if from.exists() {
                std::fs::rename(&from, dir.join(rotated_name(name, index + 1)))
                    .map_err(|e| format!("cannot rotate {}: {e}", from.display()))?;
            }
        }
        let live = dir.join(name);
        if self.config.max_files > 1 {
            std::fs::rename(&live, dir.join(rotated_name(name, 1)))
                .map_err(|e| format!("cannot rotate {}: {e}", live.display()))?;
        } else {
            std::fs::remove_file(&live)
                .map_err(|e| format!("cannot remove {}: {e}", live.display()))?;
        }
        self.size = 0;
        self.ensure_open()
    }
}

fn ends_with_newline(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return true;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return true;
    };
    if len == 0 || f.seek(SeekFrom::Start(len - 1)).is_err() {
        return true;
    }
    let mut last = [0u8; 1];
    f.read_exact(&mut last)
        .map(|_| last[0] == b'\n')
        .unwrap_or(true)
}

// ---- who is recorded, and where ------------------------------------------------------------

/// The files a record goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lanes {
    /// `audit.jsonl`.
    pub main: bool,
    /// `audit-security.jsonl`.
    pub security: bool,
}

/// Where the decision on a request is recorded, or `None` when it is not.
///
/// - A denial is always recorded, for every actor, in both files.
/// - A person's safe requests are not recorded; caution requests go to the main file, dangerous
///   ones to both.
/// - Everything a non-human actor asks is recorded: safe requests in the main file, the rest in
///   both.
pub fn lanes_for(class: ActorClass, tier: Tier, denied: bool) -> Option<Lanes> {
    let both = Lanes {
        main: true,
        security: true,
    };
    let main_only = Lanes {
        main: true,
        security: false,
    };
    if denied {
        return Some(both);
    }
    match (class, tier) {
        (ActorClass::Human | ActorClass::System, Tier::Safe) => None,
        (ActorClass::Human | ActorClass::System, Tier::Caution) => Some(main_only),
        (ActorClass::Human | ActorClass::System, Tier::Dangerous) => Some(both),
        (_, Tier::Safe) => Some(main_only),
        (_, _) => Some(both),
    }
}

/// Must the record be on disk before the request runs? Only for a non-human actor's caution or
/// dangerous request: a person is never held up by the audit (stopping an app is how a full-disk
/// incident is handled), and a read is not worth refusing.
pub fn must_wait(class: ActorClass, tier: Tier) -> bool {
    !matches!(class, ActorClass::Human | ActorClass::System) && tier != Tier::Safe
}

/// sha256 of an argument vector (each argument followed by a NUL), as lower-case hex. The
/// arguments themselves are never written.
pub fn args_digest(args: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for arg in args {
        hash.update(arg.as_bytes());
        hash.update([0u8]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// How requests are counted for the rate limit and the folding: an adapter by its name (the
/// daemon gave it that), everyone else by the way in. A caller cannot get a new allowance by
/// changing the label it claims.
pub fn actor_key(actor: &ActorId) -> String {
    match actor.class {
        ActorClass::Adapter => format!("adapter:{}", actor.label),
        class => format!("{}:{}", class.as_str(), actor.via.as_str()),
    }
}

/// Most distinct actors the folding and the limiter keep in memory.
const MAX_TRACKED: usize = 512;

/// How long lines of the same kind are folded into one.
pub const FOLD_WINDOW: Duration = Duration::from_secs(60);

struct FoldEntry {
    phase: Phase,
    started: Instant,
    count: u64,
    first: String,
    last: String,
    actor: ActorView,
    op: Option<String>,
}

/// Lines folded into one per actor and operation per window: repeated `ping`, `hello` and
/// `status` calls, and the requests refused for their rate. The line carries `count`, `first` and
/// `last`.
pub struct Folded {
    window: Duration,
    entries: HashMap<String, FoldEntry>,
}

impl Folded {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            entries: HashMap::new(),
        }
    }

    /// One more occurrence of (`phase`, `key`, `op`) at `now`.
    pub fn hit(
        &mut self,
        phase: Phase,
        key: &str,
        actor: &ActorView,
        op: Option<&str>,
        now: Instant,
    ) {
        let id = format!("{phase:?}|{key}|{}", op.unwrap_or(""));
        let stamp = now_ts();
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_TRACKED {
            return;
        }
        let entry = self.entries.entry(id).or_insert_with(|| FoldEntry {
            phase,
            started: now,
            count: 0,
            first: stamp.clone(),
            last: stamp.clone(),
            actor: actor.clone(),
            op: op.map(str::to_string),
        });
        entry.count += 1;
        entry.last = stamp;
    }

    /// The lines whose window is over (all of them when `all`); they leave the table.
    pub fn take_due(&mut self, now: Instant, all: bool) -> Vec<Record> {
        let window = self.window;
        let due: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| all || now.saturating_duration_since(e.started) >= window)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(due.len());
        for id in due {
            let Some(entry) = self.entries.remove(&id) else {
                continue;
            };
            let mut record = Record::new(entry.phase);
            record.actor = Some(entry.actor);
            record.op = entry.op;
            if entry.phase == Phase::Decision {
                record.tier = Some(Tier::Safe.as_str().to_string());
                record.decision = Some("allow".to_string());
            }
            record.count = Some(entry.count);
            record.first = Some(entry.first);
            record.last = Some(entry.last);
            out.push(record);
        }
        out.sort_by(|a, b| a.first.cmp(&b.first));
        out
    }
}

/// The rates of a non-human actor.
#[derive(Debug, Clone, Copy)]
pub struct Rates {
    /// Requests per second, sustained.
    pub per_sec: f64,
    pub burst: f64,
    /// Caution and dangerous requests (the ones that reach the security file) per minute.
    pub caution_per_min: f64,
    pub caution_burst: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            per_sec: 50.0,
            burst: 100.0,
            caution_per_min: 5.0,
            caution_burst: 10.0,
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    fn refill(&mut self, per_sec: f64, burst: f64, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_sec).min(burst);
        self.last = now;
    }
}

/// Token buckets per actor: the general rate, and a slower one for the requests that change
/// something. A request over either is refused with `E_RATE_LIMITED` and is not written one by
/// one (the refusals are counted into one line per actor per minute).
pub struct RateLimiter {
    rates: Rates,
    general: HashMap<String, Bucket>,
    caution: HashMap<String, Bucket>,
}

impl RateLimiter {
    pub fn new(rates: Rates) -> Self {
        Self {
            rates,
            general: HashMap::new(),
            caution: HashMap::new(),
        }
    }

    /// Take the tokens one request of `tier` needs, or `Err` with what ran out.
    pub fn admit(&mut self, key: &str, tier: Tier, now: Instant) -> Result<(), &'static str> {
        let r = self.rates;
        if self.general.len() >= MAX_TRACKED || self.caution.len() >= MAX_TRACKED {
            // forget the actors that have been quiet long enough to be back at a full bucket
            self.general
                .retain(|_, b| now.saturating_duration_since(b.last) < Duration::from_secs(60));
            self.caution
                .retain(|_, b| now.saturating_duration_since(b.last) < Duration::from_secs(600));
        }
        let general = self
            .general
            .entry(key.to_string())
            .or_insert_with(|| Bucket::full(r.burst, now));
        general.refill(r.per_sec, r.burst, now);
        let needs_caution = tier != Tier::Safe;
        let caution = self
            .caution
            .entry(key.to_string())
            .or_insert_with(|| Bucket::full(r.caution_burst, now));
        caution.refill(r.caution_per_min / 60.0, r.caution_burst, now);
        let general = self.general.get_mut(key).expect("just inserted");
        if general.tokens < 1.0 {
            return Err("requests per second");
        }
        if needs_caution {
            let caution = self.caution.get_mut(key).expect("just inserted");
            if caution.tokens < 1.0 {
                return Err("requests that change something per minute");
            }
            caution.tokens -= 1.0;
        }
        self.general.get_mut(key).expect("just inserted").tokens -= 1.0;
        Ok(())
    }
}

/// How the audit log is set up.
#[derive(Clone)]
pub struct AuditConfig {
    pub main: WriterConfig,
    pub security: WriterConfig,
    /// How long a request waits for its decision to reach the disk.
    pub ack_timeout: Duration,
    pub rates: Rates,
    pub fold_window: Duration,
    /// The time the limiter and the folding use (a test moves it).
    pub clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl AuditConfig {
    /// The audit log of the home `paths`.
    pub fn new(paths: &Paths) -> Self {
        Self {
            main: WriterConfig::main(paths.audit_dir()),
            security: WriterConfig::security(paths.audit_dir()),
            ack_timeout: ACK_TIMEOUT,
            rates: Rates::default(),
            fold_window: FOLD_WINDOW,
            clock: Arc::new(Instant::now),
        }
    }
}

/// What `DaemonStatus` says about the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditState {
    /// `ok` or `degraded`.
    pub state: &'static str,
    /// Lines lost since the daemon started.
    pub lost: u64,
    pub warnings: Vec<String>,
}

/// The audit log of one daemon: its two files, the limiter and the folding.
pub struct Audit {
    main: AuditWriter,
    security: AuditWriter,
    dir: PathBuf,
    ack_timeout: Duration,
    next_req: AtomicU64,
    limiter: Mutex<RateLimiter>,
    refusal_budget: Mutex<Bucket>,
    folded: Arc<Mutex<Folded>>,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl Audit {
    /// The audit log of the home `paths`. Nothing is created until the first record.
    pub fn new(paths: &Paths) -> Self {
        Self::with_config(AuditConfig::new(paths))
    }

    pub fn with_config(config: AuditConfig) -> Self {
        let folded = Arc::new(Mutex::new(Folded::new(config.fold_window)));
        let ticker: Ticker = {
            let folded = Arc::clone(&folded);
            let clock = Arc::clone(&config.clock);
            Box::new(move |all| folded.lock().take_due(clock(), all))
        };
        let dir = config.main.dir.clone();
        let main_seq = recover_seq(&config.main.dir, &config.main.file, config.main.max_files);
        Self {
            main: AuditWriter::start_with(config.main, Some(ticker)),
            security: AuditWriter::start(config.security),
            dir,
            ack_timeout: config.ack_timeout,
            // request numbers continue after a restart, so one never repeats an older one
            next_req: AtomicU64::new(main_seq + 1),
            limiter: Mutex::new(RateLimiter::new(config.rates)),
            refusal_budget: Mutex::new(Bucket::full(REFUSAL_BURST, (config.clock)())),
            folded,
            clock: config.clock,
        }
    }

    /// The daemon's number for the next request (`r4121`).
    pub fn next_req(&self) -> String {
        format!("r{}", self.next_req.fetch_add(1, Ordering::SeqCst))
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    /// The rate limit of a non-human actor: `Err(RateLimited)` when it asked too often. The
    /// refusal is counted into one line per actor per window.
    pub fn check_rate(&self, actor: &ActorId, view: &ActorView, tier: Tier) -> Result<(), Error> {
        if matches!(actor.class, ActorClass::Human | ActorClass::System) {
            return Ok(());
        }
        let key = actor_key(actor);
        let now = self.now();
        match self.limiter.lock().admit(&key, tier, now) {
            Ok(()) => Ok(()),
            Err(what) => {
                self.folded
                    .lock()
                    .hit(Phase::RateLimited, &key, view, None, now);
                Err(Error::RateLimited(format!(
                    "{} asked too often ({what}); slow down",
                    actor.wire()
                )))
            }
        }
    }

    /// A `ping`, `hello` or `status` of a non-human actor: counted, and written as one line per
    /// actor and operation per window.
    pub fn poll(&self, actor: &ActorId, view: &ActorView, op: &str) {
        self.folded.lock().hit(
            Phase::Decision,
            &actor_key(actor),
            view,
            Some(op),
            self.now(),
        );
    }

    /// Record the decision on a request. With `wait` the record is on disk (in every file it
    /// goes to) before this returns, or the call fails with `E_AUDIT_UNAVAILABLE` and the
    /// request must not run. Without it the record is queued and the call never blocks.
    pub async fn decision(&self, record: Record, lanes: Lanes, wait: bool) -> Result<(), Error> {
        if !wait {
            self.queue(record, lanes);
            return Ok(());
        }
        let timeout = self.ack_timeout;
        let on_main = async {
            if lanes.main {
                self.main.write_acked(record.clone(), timeout).await
            } else {
                Ok(())
            }
        };
        let on_security = async {
            if lanes.security {
                self.security.write_acked(record.clone(), timeout).await
            } else {
                Ok(())
            }
        };
        let (main, security) = tokio::join!(on_main, on_security);
        match main.and(security) {
            Ok(()) => Ok(()),
            Err(why) => {
                let reason = self
                    .main
                    .last_error()
                    .or_else(|| self.security.last_error())
                    .unwrap_or_else(|| format!("{why:?}").to_lowercase());
                warn!("The audit log could not record a decision: {}", reason);
                Err(Error::AuditUnavailable(format!(
                    "the audit log cannot record this request, so it was not run ({reason})"
                )))
            }
        }
    }

    /// Queue a record for the files in `lanes` and go on.
    pub fn queue(&self, record: Record, lanes: Lanes) {
        if lanes.main {
            self.main.enqueue(record.clone());
        }
        if lanes.security {
            self.security.enqueue(record);
        }
    }

    /// Keep each refusal in the main file. Non-human refusals share one budget for
    /// the security file so changing peers, operations or targets cannot increase it.
    pub(crate) fn refusal(&self, mut record: Record, class: ActorClass, tier: Tier, error: &Error) {
        record.decision = Some("deny".to_string());
        record.deny = Some(Deny {
            code: error.code().as_str().to_string(),
            why: error.to_string(),
        });
        record.clip_text();
        let mut lanes = lanes_for(class, tier, true).expect("refusals always have audit lanes");
        if !matches!(class, ActorClass::Human | ActorClass::System) {
            let mut budget = self.refusal_budget.lock();
            budget.refill(REFUSALS_PER_SECOND, REFUSAL_BURST, self.now());
            lanes.security = budget.tokens >= 1.0;
            if lanes.security {
                budget.tokens -= 1.0;
            }
        }
        self.queue(record, lanes);
    }

    /// A line in an adapter's life: imported, enabled, started, killed...
    pub fn adapter_line(&self, line: AdapterLine) {
        let mut record = Record::new(Phase::Adapter);
        record.adapter = Some(line);
        self.queue(
            record,
            Lanes {
                main: true,
                security: true,
            },
        );
    }

    /// The launch plan a runtime adapter returned: the program, how many arguments, and a
    /// digest of them (never the arguments).
    pub fn plan_line(&self, req: Option<&str>, program: &str, args: &[String]) {
        let mut record = Record::new(Phase::Plan);
        record.req = req.map(str::to_string);
        record.program = Some(program.to_string());
        record.argc = Some(args.len() as u64);
        record.args_sha256 = Some(args_digest(args));
        self.queue(
            record,
            Lanes {
                main: true,
                security: true,
            },
        );
    }

    /// A note an adapter wrote (`host.audit_note`), always tagged with its name.
    pub fn note(&self, adapter: &str, text: &str) {
        let mut record = Record::new(Phase::Note);
        record.adapter = Some(AdapterLine {
            name: adapter.to_string(),
            event: "note".to_string(),
            version: None,
            sha12: None,
        });
        record.text = Some(text.to_string());
        self.queue(
            record,
            Lanes {
                main: true,
                security: false,
            },
        );
    }

    /// Whether the log works, what has been lost and what to tell the operator.
    pub fn state(&self) -> AuditState {
        let mut warnings = Vec::new();
        let in_the_way = std::fs::metadata(&self.dir)
            .map(|m| !m.is_dir())
            .unwrap_or(false);
        if in_the_way {
            warnings.push(format!(
                "the audit directory {} exists but is not a directory: requests of AI and adapters that change something are refused until it is removed (people are not held up)",
                self.dir.display()
            ));
        } else if self.main.is_failing() || self.security.is_failing() {
            let why = self
                .main
                .last_error()
                .or_else(|| self.security.last_error())
                .unwrap_or_default();
            warnings.push(format!(
                "the audit log cannot be written ({why}); requests of AI and adapters that change something are refused until it can"
            ));
        }
        let lost = self.main.lost() + self.security.lost();
        AuditState {
            state: if warnings.is_empty() {
                "ok"
            } else {
                "degraded"
            },
            lost,
            warnings,
        }
    }

    /// Write what is folded and drain both queues, waiting at most `budget` for each; a writer
    /// stuck in the file system cannot hold a shutdown longer.
    pub async fn close(&self, budget: Duration) {
        let (main, security) = tokio::join!(self.main.close(budget), self.security.close(budget));
        if !(main && security) {
            warn!("The audit log was not drained within {:?}", budget);
        }
    }

    /// The two writers (main, security), for a test that holds one.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn writers(&self) -> (&AuditWriter, &AuditWriter) {
        (&self.main, &self.security)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn fast(dir: &Path) -> WriterConfig {
        WriterConfig {
            retry: Duration::from_millis(60),
            sync_interval: Duration::from_millis(20),
            ..WriterConfig::main(dir)
        }
    }

    fn rec(text: &str) -> Record {
        let mut r = Record::new(Phase::Note);
        r.text = Some(text.to_string());
        r
    }

    async fn write_all(writer: &AuditWriter, n: usize) {
        for i in 0..n {
            writer
                .write_acked(rec(&format!("line {i}")), Duration::from_secs(5))
                .await
                .expect("written");
        }
    }

    /// Every line of every file of the series, oldest first.
    fn all_lines(dir: &Path, file: &str) -> Vec<serde_json::Value> {
        let mut names: Vec<String> = (1..MAX_FILES + 2)
            .rev()
            .map(|i| rotated_name(file, i))
            .collect();
        names.push(file.to_string());
        let mut out = Vec::new();
        for name in names {
            if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
                out.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
            }
        }
        out
    }

    fn full_record() -> Record {
        let mut r = Record::new(Phase::Decision);
        r.seq = 99;
        r.req = Some("r1".into());
        r.client_req = Some("c".repeat(500));
        r.actor = Some(ActorView {
            class: "ai".into(),
            label: "example-agent".into(),
            via: "agent".into(),
            pid: Some(1),
            uid: Some(2),
        });
        r.op = Some("restart".into());
        r.tier = Some("caution".into());
        r.selector = Some("s".repeat(900));
        r.targets = Some(
            (0..200)
                .map(|id| Target {
                    id: Some(id),
                    name: "n".repeat(300),
                })
                .collect(),
        );
        r.targets_more = Some(1);
        r.decision = Some("deny".into());
        r.deny = Some(Deny {
            code: "E_POLICY_DENIED".into(),
            why: "w".repeat(2000),
        });
        r.dry_run = Some(false);
        r.program = Some("p".repeat(2000));
        r.argc = Some(3);
        r.args_sha256 = Some("a".repeat(64));
        r.ok = Some(false);
        r.code = Some("E_X".into());
        r.changed = Some(1);
        r.failed = Some(1);
        r.ms = Some(5);
        r.count = Some(7);
        r.first = Some(now_ts());
        r.last = Some(now_ts());
        r.dropped = Some(2);
        r.adapter = Some(AdapterLine {
            name: "a".into(),
            event: "started".into(),
            version: Some("1.0.0".into()),
            sha12: Some("0123456789ab".into()),
        });
        r.end_user = Some("e".repeat(500));
        r.text = Some("t".repeat(2000));
        r
    }

    #[test]
    fn record_serialises_to_one_line_under_4k() {
        let line = full_record().to_line();
        assert!(line.len() <= MAX_LINE, "{} bytes", line.len());
        assert!(!line.contains('\n'));
        let back: Record = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(back.phase, Phase::Decision);
        let targets = back.targets.expect("some targets are kept");
        assert!(targets.len() <= MAX_TARGETS && !targets.is_empty());
        assert!(
            back.targets_more.unwrap() as usize + targets.len() >= 200,
            "the rest is counted"
        );
        assert_eq!(
            back.deny.unwrap().why.chars().count(),
            MAX_WHY_CHARS.min(2000)
        );
        assert_eq!(back.client_req.unwrap().len(), MAX_CLIENT_REQ_CHARS);
        // multi-byte text is cut on a character, not a byte
        let mut r = Record::new(Phase::Note);
        r.text = Some("é".repeat(1000));
        let line = r.to_line();
        assert!(serde_json::from_str::<Record>(&line).is_ok());
        assert!(line.len() <= MAX_LINE);
    }

    #[test]
    fn record_type_has_no_field_for_env_or_args() {
        fn keys(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (k, v) in map {
                        out.push(k.clone());
                        keys(v, out);
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
                _ => {}
            }
        }
        let value = serde_json::to_value(full_record()).unwrap();
        let mut found = Vec::new();
        keys(&value, &mut found);
        found.sort();
        found.dedup();
        for key in &found {
            assert!(
                ALLOWED_KEYS.contains(&key.as_str()),
                "unexpected key '{key}' in a record"
            );
        }
        // and the other way: the list names no key a record cannot have
        for key in ALLOWED_KEYS {
            assert!(
                found.contains(&key.to_string()),
                "'{key}' is allowed but no record has it"
            );
        }
        for banned in [
            "env", "args", "hooks", "script", "spec", "config", "log", "password", "token",
        ] {
            assert!(!ALLOWED_KEYS.contains(&banned), "{banned}");
        }
    }

    #[tokio::test]
    async fn seq_is_monotonic_across_rotation() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(WriterConfig {
            max_bytes: 600,
            ..fast(&dir)
        });
        write_all(&writer, 60).await;
        assert!(dir.join("audit.1.jsonl").exists(), "the file rotated");
        let seqs: Vec<u64> = all_lines(&dir, "audit.jsonl")
            .iter()
            .map(|l| l["seq"].as_u64().unwrap())
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "no gap and no repeat: {seqs:?}"
        );
        assert_eq!(*seqs.last().unwrap(), 60);
    }

    #[tokio::test]
    async fn rotation_keeps_max_files_and_total_size_bounded() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(WriterConfig {
            max_bytes: 500,
            max_files: 3,
            ..fast(&dir)
        });
        write_all(&writer, 120).await;
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["audit.1.jsonl", "audit.2.jsonl", "audit.jsonl"],
            "the live file and two older"
        );
        let total: u64 = names
            .iter()
            .map(|n| std::fs::metadata(dir.join(n)).unwrap().len())
            .sum();
        assert!(total <= 3 * (500 + MAX_LINE as u64), "{total} bytes");
        // the newest lines are the ones kept
        let last = all_lines(&dir, "audit.jsonl").last().unwrap()["seq"]
            .as_u64()
            .unwrap();
        assert_eq!(last, 120);
    }

    #[tokio::test]
    async fn torn_last_line_is_skipped_and_seq_continues() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        std::fs::create_dir_all(&dir).unwrap();
        let mut good = rec("one");
        good.seq = 41;
        let mut torn = good.to_line();
        torn.truncate(torn.len() - 9);
        std::fs::write(
            dir.join("audit.jsonl"),
            format!(
                "{}\n{}\n{}",
                rec("zero").to_line().replace("\"seq\":0", "\"seq\":40"),
                good.to_line(),
                torn
            ),
        )
        .unwrap();
        assert_eq!(recover_seq(&dir, "audit.jsonl", MAX_FILES), 41);
        let writer = AuditWriter::start(fast(&dir));
        write_all(&writer, 1).await;
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        let last = text.lines().last().unwrap();
        let last: serde_json::Value =
            serde_json::from_str(last).expect("the new record is on its own line");
        assert_eq!(last["seq"], 42);
    }

    #[tokio::test]
    async fn seq_is_recovered_after_restart() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        {
            let writer = AuditWriter::start(WriterConfig {
                max_bytes: 400,
                ..fast(&dir)
            });
            write_all(&writer, 30).await;
            assert!(writer.close(Duration::from_secs(2)).await);
        }
        assert_eq!(recover_seq(&dir, "audit.jsonl", MAX_FILES), 30);
        let writer = AuditWriter::start(WriterConfig {
            max_bytes: 400,
            ..fast(&dir)
        });
        write_all(&writer, 2).await;
        let seqs: Vec<u64> = all_lines(&dir, "audit.jsonl")
            .iter()
            .map(|l| l["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(&seqs[seqs.len() - 2..], [31, 32]);
        // a series whose live file was just rotated away still gives its last seq
        std::fs::write(dir.join("audit.jsonl"), b"").unwrap();
        let older = std::fs::read_to_string(dir.join("audit.1.jsonl")).unwrap();
        let last_older: serde_json::Value =
            serde_json::from_str(older.lines().last().unwrap()).unwrap();
        assert_eq!(
            recover_seq(&dir, "audit.jsonl", MAX_FILES),
            last_older["seq"].as_u64().unwrap()
        );
    }

    #[tokio::test]
    async fn file_is_0600_and_dir_0700() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(fast(&dir));
        write_all(&writer, 1).await;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("audit.jsonl")), 0o600);
        // a rotated file keeps the mode
        let writer = AuditWriter::start(WriterConfig {
            max_bytes: 300,
            ..fast(&tmp.path().join("audit2"))
        });
        write_all(&writer, 10).await;
        assert_eq!(mode(&tmp.path().join("audit2/audit.1.jsonl")), 0o600);
    }

    #[tokio::test]
    async fn directory_is_created_lazily_and_failure_is_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(fast(&dir));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!dir.exists(), "nothing is created before the first record");
        write_all(&writer, 1).await;
        assert!(dir.join("audit.jsonl").exists());

        // the audit path is a file: the writer reports it and does not panic or stop
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"in the way").unwrap();
        let writer = AuditWriter::start(fast(&blocked));
        let err = writer
            .write_acked(rec("x"), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err, WriteError::Failed);
        assert!(writer.is_failing());
        assert!(
            writer.last_error().unwrap().contains("not a directory"),
            "{:?}",
            writer.last_error()
        );
        assert_eq!(
            std::fs::read(&blocked).unwrap(),
            b"in the way",
            "the file is left alone"
        );
    }

    #[tokio::test]
    async fn writer_reopens_after_error() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dir = home.join("audit");
        // the parent cannot be written: the directory cannot be made
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writer = AuditWriter::start(fast(&dir));
        let failed = writer.write_acked(rec("a"), Duration::from_secs(2)).await;
        assert_eq!(failed, Err(WriteError::Failed));
        assert!(writer.is_failing());
        // inside the retry interval it does not even try
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            writer.write_acked(rec("b"), Duration::from_secs(2)).await,
            Err(WriteError::Failed)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        writer
            .write_acked(rec("c"), Duration::from_secs(2))
            .await
            .expect("written once writable");
        assert!(!writer.is_failing());
        let lines = all_lines(&dir, "audit.jsonl");
        // the two lost lines are reported before the first one that got through
        assert_eq!(lines[0]["phase"], "note");
        assert_eq!(lines[0]["dropped"], 2);
        assert_eq!(lines[1]["text"], "c");
        assert_eq!(writer.lost(), 2);
    }

    #[tokio::test]
    async fn ack_times_out_when_the_writer_is_blocked() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(fast(&dir));
        let guard = writer.pause();
        let started = Instant::now();
        let err = writer
            .write_acked(rec("held"), Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(err, WriteError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            !dir.join("audit.jsonl").exists(),
            "nothing was written while it was held"
        );
        drop(guard);
        write_all(&writer, 1).await;
        assert_eq!(
            all_lines(&dir, "audit.jsonl").len(),
            2,
            "the held record is written once released"
        );
        // a close that cannot drain gives up after its budget
        let writer = AuditWriter::start(fast(&dir));
        let _held = writer.pause();
        let started = Instant::now();
        assert!(!writer.close(Duration::from_millis(100)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn queue_overflow_never_blocks_and_is_counted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let writer = AuditWriter::start(WriterConfig {
            queue: 4,
            ..fast(&dir)
        });
        let guard = writer.pause();
        let started = Instant::now();
        let queued = (0..50)
            .filter(|i| writer.enqueue(rec(&format!("q{i}"))))
            .count();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "enqueue never waits"
        );
        // the writer holds one record it already took, the queue holds four
        assert!((4..=5).contains(&queued), "{queued}");
        assert!(writer.is_failing() && writer.lost() as usize == 50 - queued);
        drop(guard);
        // the queue drains; the first record the writer takes after that is the new one
        let mut written = Err(WriteError::Queue);
        for _ in 0..100 {
            written = writer
                .write_acked(rec("after"), Duration::from_secs(5))
                .await;
            if written != Err(WriteError::Queue) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        written.expect("written");
        let lines = all_lines(&dir, "audit.jsonl");
        // every refused record is reported by a dropped line (the retries of this test included,
        // which may land in a second line), and the lines add up to the count
        let reported: u64 = lines
            .iter()
            .filter(|l| l["phase"] == "note")
            .filter_map(|l| l["dropped"].as_u64())
            .sum();
        assert!(reported as usize >= 50 - queued);
        assert_eq!(reported, writer.lost());
        assert!(!writer.is_failing());
    }

    #[tokio::test]
    async fn security_file_is_not_rotated_by_read_volume() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let main = AuditWriter::start(WriterConfig {
            max_bytes: 500,
            max_files: 3,
            ..fast(&dir)
        });
        let security = AuditWriter::start(WriterConfig {
            file: oxidepm_core::constants::AUDIT_SECURITY_FILE.to_string(),
            ..fast(&dir)
        });
        security
            .write_acked(rec("the denial"), Duration::from_secs(2))
            .await
            .unwrap();
        write_all(&main, 200).await;
        assert!(
            dir.join("audit.2.jsonl").exists(),
            "the main file rotated many times"
        );
        let sec = all_lines(&dir, "audit-security.jsonl");
        assert_eq!(sec.len(), 1);
        assert_eq!(sec[0]["text"], "the denial");
        assert!(!dir.join("audit-security.1.jsonl").exists());
    }

    #[test]
    fn recording_rules_by_actor_and_tier() {
        use ActorClass::{Adapter, Ai, Human, System};
        let both = Some(Lanes {
            main: true,
            security: true,
        });
        let main = Some(Lanes {
            main: true,
            security: false,
        });
        // a person: only what changes something; dangerous also in the security file
        assert_eq!(lanes_for(Human, Tier::Safe, false), None);
        assert_eq!(lanes_for(Human, Tier::Caution, false), main);
        assert_eq!(lanes_for(Human, Tier::Dangerous, false), both);
        assert_eq!(lanes_for(System, Tier::Safe, false), None);
        // an AI or an adapter: everything; reads in the main file only
        for class in [Ai, Adapter] {
            assert_eq!(lanes_for(class, Tier::Safe, false), main);
            assert_eq!(lanes_for(class, Tier::Caution, false), both);
            assert_eq!(lanes_for(class, Tier::Dangerous, false), both);
        }
        // a denial is always recorded, in both files, whoever asked
        for class in [Human, System, Ai, Adapter] {
            for tier in [Tier::Safe, Tier::Caution, Tier::Dangerous] {
                assert_eq!(lanes_for(class, tier, true), both);
            }
        }
        // only a non-human actor's changes wait for the disk
        assert!(must_wait(Ai, Tier::Caution) && must_wait(Adapter, Tier::Dangerous));
        assert!(
            !must_wait(Ai, Tier::Safe)
                && !must_wait(Human, Tier::Dangerous)
                && !must_wait(System, Tier::Caution)
        );
    }

    #[test]
    fn args_digest_is_sha256_of_the_nul_terminated_arguments() {
        // sha256 of the empty input
        assert_eq!(
            args_digest(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // the split between arguments is part of the digest
        let two = args_digest(&["a".to_string(), "b".to_string()]);
        let one = args_digest(&["ab".to_string()]);
        assert_ne!(two, one);
        assert_eq!(two.len(), 64);
    }

    #[test]
    fn token_buckets_allow_the_burst_then_the_rate() {
        let origin = Instant::now();
        let mut limiter = RateLimiter::new(Rates::default());
        // 100 requests at once, the 101st is over
        for i in 0..100 {
            assert!(limiter.admit("ai:agent", Tier::Safe, origin).is_ok(), "{i}");
        }
        assert_eq!(
            limiter.admit("ai:agent", Tier::Safe, origin),
            Err("requests per second")
        );
        // 50 per second come back
        assert!(limiter
            .admit("ai:agent", Tier::Safe, origin + Duration::from_millis(40))
            .is_ok());
        assert!(limiter
            .admit("ai:agent", Tier::Safe, origin + Duration::from_millis(40))
            .is_ok());
        assert!(limiter
            .admit("ai:agent", Tier::Safe, origin + Duration::from_millis(40))
            .is_err());
        // another actor has its own bucket
        assert!(limiter.admit("ai:main", Tier::Safe, origin).is_ok());
        // requests that change something: a burst of ten, then five a minute
        let mut limiter = RateLimiter::new(Rates::default());
        for i in 0..10 {
            assert!(limiter.admit("a", Tier::Caution, origin).is_ok(), "{i}");
        }
        assert_eq!(
            limiter.admit("a", Tier::Caution, origin),
            Err("requests that change something per minute")
        );
        assert!(
            limiter.admit("a", Tier::Safe, origin).is_ok(),
            "reads have their own allowance"
        );
        assert!(
            limiter
                .admit("a", Tier::Dangerous, origin + Duration::from_secs(11))
                .is_err(),
            "one token per 12 s"
        );
        assert!(limiter
            .admit("a", Tier::Dangerous, origin + Duration::from_secs(13))
            .is_ok());
        // a refused request costs nothing: the caution bucket did not drain the general one
        let rates = Rates {
            per_sec: 1.0,
            burst: 5.0,
            caution_burst: 2.0,
            ..Rates::default()
        };
        let mut limiter = RateLimiter::new(rates);
        for _ in 0..10 {
            let _ = limiter.admit("b", Tier::Caution, origin);
        }
        for i in 0..3 {
            assert!(
                limiter.admit("b", Tier::Safe, origin).is_ok(),
                "{i}: three general tokens were left"
            );
        }
        assert!(limiter.admit("b", Tier::Safe, origin).is_err());
    }

    #[test]
    fn folded_lines_carry_count_first_and_last_once_per_window() {
        let origin = Instant::now();
        let actor = ActorView {
            class: "adapter".into(),
            label: "notifier".into(),
            via: "adapter".into(),
            pid: None,
            uid: None,
        };
        let mut folded = Folded::new(Duration::from_secs(60));
        for i in 0..500 {
            folded.hit(
                Phase::Decision,
                "adapter:notifier",
                &actor,
                Some("status"),
                origin + Duration::from_millis(i),
            );
        }
        folded.hit(
            Phase::Decision,
            "adapter:notifier",
            &actor,
            Some("ping"),
            origin,
        );
        assert!(folded
            .take_due(origin + Duration::from_secs(59), false)
            .is_empty());
        let due = folded.take_due(origin + Duration::from_secs(60), false);
        assert_eq!(due.len(), 2, "one line per operation");
        let status = due
            .iter()
            .find(|r| r.op.as_deref() == Some("status"))
            .unwrap();
        assert_eq!(status.count, Some(500));
        assert!(status.first.is_some() && status.last.is_some());
        assert_eq!(status.decision.as_deref(), Some("allow"));
        // taken lines are gone; a new hit starts a new window
        assert!(folded
            .take_due(origin + Duration::from_secs(120), true)
            .is_empty());
        folded.hit(Phase::RateLimited, "ai:agent", &actor, None, origin);
        let due = folded.take_due(origin, true);
        assert_eq!((due.len(), due[0].phase), (1, Phase::RateLimited));
    }

    #[test]
    fn recover_seq_does_not_block_on_a_named_pipe() {
        use nix::sys::stat::Mode;
        let tmp = TempDir::new().unwrap();
        nix::unistd::mkfifo(
            &tmp.path().join("audit.jsonl"),
            Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        // opening the pipe for reading would wait for a writer that never comes
        assert_eq!(recover_seq(tmp.path(), "audit.jsonl", MAX_FILES), 0);
    }

    #[tokio::test]
    async fn plan_adapter_and_note_lines_have_no_arguments() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        let mut config = AuditConfig::new(&paths);
        config.main = fast(&paths.audit_dir());
        config.security = WriterConfig {
            file: "audit-security.jsonl".into(),
            ..fast(&paths.audit_dir())
        };
        let audit = Audit::with_config(config);
        audit.plan_line(
            Some("r7"),
            "/usr/bin/node",
            &["server.js".to_string(), "--key=PLANSECRET".to_string()],
        );
        audit.adapter_line(AdapterLine {
            name: "runtime-node".into(),
            event: "started".into(),
            version: Some("1.2.3".into()),
            sha12: Some("0123456789ab".into()),
        });
        audit.note("runtime-node", &"n".repeat(1000));
        audit.close(Duration::from_secs(2)).await;
        let main = all_lines(&paths.audit_dir(), "audit.jsonl");
        let security = all_lines(&paths.audit_dir(), "audit-security.jsonl");
        assert_eq!(main.len(), 3);
        assert_eq!(
            security.len(),
            2,
            "plan and adapter lines are in the security file, a note is not"
        );
        let plan = main.iter().find(|l| l["phase"] == "plan").unwrap();
        assert_eq!(
            (
                plan["program"].as_str(),
                plan["argc"].as_u64(),
                plan["req"].as_str()
            ),
            (Some("/usr/bin/node"), Some(2), Some("r7"))
        );
        assert_eq!(plan["args_sha256"].as_str().unwrap().len(), 64);
        let note = main.iter().find(|l| l["phase"] == "note").unwrap();
        assert_eq!(note["text"].as_str().unwrap().len(), MAX_NOTE_CHARS, "cut");
        assert_eq!(note["adapter"]["name"], "runtime-node");
        let text = std::fs::read_to_string(paths.audit_log()).unwrap();
        assert!(!text.contains("PLANSECRET") && !text.contains("server.js"));
    }

    #[tokio::test]
    async fn close_writes_the_folded_lines_and_drains() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        let mut config = AuditConfig::new(&paths);
        config.main = fast(&paths.audit_dir());
        let audit = Audit::with_config(config);
        let actor = ActorId::new(ActorClass::Ai, "agent", oxidepm_core::actor::Via::Agent);
        let view = ActorView::of(&actor, Some(1), Some(2));
        for _ in 0..7 {
            audit.poll(&actor, &view, "status");
        }
        audit.close(Duration::from_secs(2)).await;
        let lines = all_lines(&paths.audit_dir(), "audit.jsonl");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            (lines[0]["count"].as_u64(), lines[0]["op"].as_str()),
            (Some(7), Some("status"))
        );
        assert_eq!(lines[0]["actor"]["pid"], 1);
    }
}
