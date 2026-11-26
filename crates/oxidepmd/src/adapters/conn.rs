//! The connection to one adapter process.
//!
//! An [`AdapterConn`] owns the two pipes of one adapter and nothing else: it knows nothing about
//! processes, grants or the database. It is built over any `AsyncRead` and `AsyncWrite`, so it is
//! tested with the fixture adapter over a pipe and without a daemon.
//!
//! What it guarantees, each as a test:
//! - A caller never waits longer than its deadline, whatever the adapter does. The deadline is a
//!   timer here, around the reply; the adapter's cooperation is not needed. On expiry (or when
//!   the calling future is dropped) the pending entry is removed at once, a tombstone remembers
//!   the id, and `cancel {id}` is sent, so a late or "cancelled" reply is dropped and counted
//!   without a strike and the pending table cannot grow.
//! - Writing never waits for the pipe in the caller: a bounded queue feeds a writer task, and a
//!   write that makes no progress for the write timeout closes the connection.
//! - Reading is one task. Every line passes the bounded reader (an oversize line is skipped
//!   without being stored), a rate limiter (the reader stops reading, so the adapter blocks on
//!   its own pipe) and the strike rules; three strikes inside the window close the connection.
//! - Everything in flight when the connection ends fails with `E_ADAPTER_CRASHED`.
//!
//! Time is `tokio::time::Instant`, and the pure parts ([`TokenBucket`], [`StrikeWindow`],
//! [`Tombstones`]) take `now` as an argument, so they are tested without a clock.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::codes::{self, rpc};
use oxidepm_adapter_proto::messages::{method, CancelParams, InvokeOutputParams, OutputStream};
use oxidepm_adapter_proto::{
    AsyncFrameReader, ErrorObject, Frame, FrameError, Limits, Params, Read,
};
use oxidepm_core::Error;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------------------------
// Pure parts
// ---------------------------------------------------------------------------------------------

/// A token bucket that always debits: [`TokenBucket::reserve`] returns how long the caller must
/// wait before the debit is covered. Refills at `rate` per second up to `burst`.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate_per_sec: f64, burst: f64, now: Instant) -> Self {
        Self {
            rate: rate_per_sec.max(f64::MIN_POSITIVE),
            burst,
            tokens: burst,
            last: now,
        }
    }

    /// Take `n` tokens only if the bucket covers them at `now`; otherwise take nothing.
    pub fn try_take(&mut self, n: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        let tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        if tokens >= n {
            self.last = now;
            self.tokens = tokens - n;
            true
        } else {
            false
        }
    }

    /// Take `n` tokens at `now`; the wait is zero while the bucket covers them.
    pub fn reserve(&mut self, n: f64, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.tokens -= n;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((-self.tokens / self.rate).min(3600.0))
        }
    }
}

/// Protocol violations counted over a sliding window.
#[derive(Debug, Clone)]
pub struct StrikeWindow {
    window: Duration,
    limit: u32,
    times: VecDeque<Instant>,
}

impl StrikeWindow {
    pub fn new(window: Duration, limit: u32) -> Self {
        Self {
            window,
            limit: limit.max(1),
            times: VecDeque::new(),
        }
    }

    /// Record a strike at `now`. Returns the strikes inside the window and whether that is enough
    /// to kill the adapter.
    pub fn record(&mut self, now: Instant) -> (u32, bool) {
        while let Some(first) = self.times.front() {
            if now.saturating_duration_since(*first) > self.window {
                self.times.pop_front();
            } else {
                break;
            }
        }
        self.times.push_back(now);
        let count = u32::try_from(self.times.len()).unwrap_or(u32::MAX);
        (count, count >= self.limit)
    }
}

/// Recently expired or cancelled request ids: a reply that carries one is dropped, not punished.
#[derive(Debug, Clone)]
pub struct Tombstones {
    cap: usize,
    ttl: Duration,
    order: VecDeque<(u64, Instant)>,
    set: HashSet<u64>,
}

impl Tombstones {
    pub fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            cap: cap.max(1),
            ttl,
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some((id, at)) = self.order.front().copied() {
            if now.saturating_duration_since(at) > self.ttl || self.order.len() > self.cap {
                self.order.pop_front();
                self.set.remove(&id);
            } else {
                break;
            }
        }
    }

    pub fn insert(&mut self, id: u64, now: Instant) {
        if self.set.insert(id) {
            self.order.push_back((id, now));
        }
        self.prune(now);
    }

    pub fn contains(&mut self, id: u64, now: Instant) -> bool {
        self.prune(now);
        self.set.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

/// "Throttled continuously for the strike period", as a pure clock. The reader is throttled
/// whenever the rate limiter makes it wait. A timer that overshoots by a millisecond leaves the
/// bucket briefly in credit, so a single unthrottled frame does not end the period: it ends after
/// a quiet gap with no throttling at all.
#[derive(Debug, Clone)]
pub struct Throttle {
    strike_after: Duration,
    quiet_gap: Duration,
    since: Option<Instant>,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(strike_after: Duration) -> Self {
        Self {
            strike_after,
            quiet_gap: Duration::from_millis(500).min(strike_after / 2),
            since: None,
            last: None,
        }
    }

    /// The reader looked at the limiter at `now`; `throttled` says whether it had to wait.
    /// Returns true when the period reached the strike length (and starts the next period).
    pub fn observe(&mut self, now: Instant, throttled: bool) -> bool {
        if throttled {
            self.last = Some(now);
            self.since.get_or_insert(now);
        } else if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) >= self.quiet_gap)
        {
            self.since = None;
            self.last = None;
        }
        match self.since {
            Some(since) if now.saturating_duration_since(since) >= self.strike_after => {
                self.since = Some(now);
                true
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration, errors, counters
// ---------------------------------------------------------------------------------------------

/// Connection limits and timeouts. [`ConnConfig::protocol1`] holds the production values. Tests inject
/// shorter ones so that none waits real minutes.
#[derive(Debug, Clone, Copy)]
pub struct ConnConfig {
    pub limits: Limits,
    /// A write that makes no progress for this long closes the connection.
    pub write_timeout: Duration,
    /// How long a request waits for room in the outgoing queue before it fails busy.
    pub send_wait: Duration,
    /// Throttled continuously for this long is a strike.
    pub throttle_strike: Duration,
    /// Capacity of the outgoing queue, in frames.
    pub outbound_queue: usize,
}

impl ConnConfig {
    pub fn protocol1() -> Self {
        Self {
            limits: Limits::PROTOCOL_1,
            write_timeout: Duration::from_secs(5),
            send_wait: Duration::from_secs(1),
            throttle_strike: Duration::from_secs(10),
            outbound_queue: 256,
        }
    }
}

impl Default for ConnConfig {
    fn default() -> Self {
        Self::protocol1()
    }
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The adapter closed its stdout (it exited, or closed it).
    Eof,
    /// The stream ended in the middle of a line.
    Truncated,
    /// A line remained oversized beyond the resynchronization budget.
    Desynchronized,
    ReadError(String),
    /// Three strikes inside the window.
    Strikes,
    /// A write made no progress for the write timeout: the adapter does not read its stdin.
    WriteStalled,
    WriteError(String),
    /// The owner closed it.
    Local,
    /// A task of the connection panicked.
    Panicked,
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseReason::Eof => write!(f, "the adapter closed its output"),
            CloseReason::Truncated => {
                write!(f, "the adapter's output ended in the middle of a line")
            }
            CloseReason::Desynchronized => write!(
                f,
                "an oversized adapter frame did not end within the discard limit"
            ),
            CloseReason::ReadError(e) => write!(f, "reading from the adapter failed: {e}"),
            CloseReason::Strikes => write!(f, "the adapter was cut off for protocol violations"),
            CloseReason::WriteStalled => write!(f, "the adapter does not read its input"),
            CloseReason::WriteError(e) => write!(f, "writing to the adapter failed: {e}"),
            CloseReason::Local => write!(f, "the connection was closed"),
            CloseReason::Panicked => write!(f, "a task of the connection panicked"),
        }
    }
}

/// Why a call to an adapter did not give a result.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// Too many requests in flight, or no room in the outgoing queue.
    Busy,
    /// The deadline passed. The adapter is not blamed; a late reply is dropped.
    Timeout,
    /// The connection ended before the answer came (`E_ADAPTER_CRASHED`).
    Crashed(String),
    /// The adapter answered with an error object.
    Remote(ErrorObject),
    /// The reply did not have the shape of the method (a strike was recorded).
    BadReply(String),
    /// The request would not fit in a frame (`E_REQUEST_TOO_LARGE`).
    TooLarge,
    /// The invocation produced more queued output than its client could receive.
    OutputBackpressure,
}

impl CallError {
    /// The core error for this failure, naming the adapter.
    pub fn into_error(self, adapter: &str) -> Error {
        match self {
            CallError::Busy => Error::AdapterBusy(format!(
                "adapter '{adapter}' has too many requests in flight"
            )),
            CallError::Timeout => {
                Error::AdapterTimeout(format!("adapter '{adapter}' did not answer in time"))
            }
            CallError::Crashed(why) => Error::AdapterCrashed(format!("adapter '{adapter}': {why}")),
            CallError::Remote(e) => Error::AdapterProtocol(format!(
                "adapter '{adapter}' answered with an error: {}",
                e.message
            )),
            CallError::BadReply(why) => {
                Error::AdapterProtocol(format!("adapter '{adapter}': {why}"))
            }
            CallError::TooLarge => Error::AdapterInvalid(format!(
                "a request to adapter '{adapter}' does not fit in a frame"
            )),
            CallError::OutputBackpressure => Error::AdapterBusy(format!(
                "the caller could not keep up with output from adapter '{adapter}'"
            )),
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Busy => write!(f, "busy"),
            CallError::Timeout => write!(f, "timeout"),
            CallError::Crashed(why) => write!(f, "crashed: {why}"),
            CallError::Remote(e) => write!(f, "adapter error {}: {}", e.code, e.message),
            CallError::BadReply(why) => write!(f, "bad reply: {why}"),
            CallError::TooLarge => write!(f, "request too large"),
            CallError::OutputBackpressure => write!(f, "the invocation output queue is full"),
        }
    }
}

/// What the connection counted. Cheap to read at any time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Strikes recorded over the life of the connection.
    pub strikes: u64,
    /// Replies dropped because their request had expired or been cancelled.
    pub late_dropped: u64,
    /// Lines that were not valid frames.
    pub violations: u64,
    /// Lines over the limit that were skipped.
    pub overflows: u64,
    /// Frames read.
    pub frames_in: u64,
    /// Frames written.
    pub frames_out: u64,
    /// Times the reader was held back by the rate limiter for the strike period.
    pub throttle_strikes: u64,
    /// The largest buffer the reader ever held (bytes of capacity).
    pub reader_peak_capacity: usize,
}

#[derive(Default)]
struct AtomicCounters {
    strikes: AtomicU64,
    late_dropped: AtomicU64,
    violations: AtomicU64,
    overflows: AtomicU64,
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    throttle_strikes: AtomicU64,
    reader_peak: AtomicUsize,
}

/// A way for the code that answers an adapter's requests to count a protocol violation against
/// the connection. Repeated requests over the rate limit earn strikes.
#[derive(Clone)]
pub struct StrikeHandle(std::sync::Weak<Shared>);

impl StrikeHandle {
    /// One strike against the connection, if it is still there.
    pub fn strike(&self, why: &str) {
        if let Some(shared) = self.0.upgrade() {
            shared.strike(why);
        }
    }
}

/// The adapter's requests to the daemon (`host.request`, `host.audit_note`); the host API
/// (`adapters/hostapi.rs`) answers them.
#[async_trait]
pub trait Inbound: Send + Sync {
    /// Optionally create a handler with state and strikes specific to this connection.
    fn for_connection(&self, _strikes: StrikeHandle) -> Option<Arc<dyn Inbound>> {
        None
    }

    /// Answer a request the adapter sent. A refusal is an `Err`.
    async fn request(&self, method: &str, params: Params) -> Result<Params, ErrorObject>;

    /// A notification from the adapter (`cancel`, `invoke.output`). Unknown ones are ignored.
    fn notification(&self, _method: &str, _params: Params) {}

    /// The same answers for an adapter process started for a session, whose requests are
    /// recorded as coming in through the session. `None`: no difference.
    fn for_session(&self) -> Option<Arc<dyn Inbound>> {
        None
    }
}

/// The answer before the host API exists: every method is unknown.
pub struct NoHostApi;

#[async_trait]
impl Inbound for NoHostApi {
    async fn request(&self, method: &str, _params: Params) -> Result<Params, ErrorObject> {
        Err(ErrorObject::new(
            rpc::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------------------------

type Reply = Result<Params, CallError>;

enum WriterMsg {
    Line(Vec<u8>),
    /// Close the adapter's stdin and stop writing (the adapter's cue to exit).
    CloseInput,
}

/// A piece of the output of a `command.invoke`, as the adapter reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stream: OutputStream,
    pub text: String,
}

/// Track `invoke.output` and enforce the 64 KiB frame and 8 MiB invocation limits.
struct Progress {
    tx: mpsc::Sender<Output>,
    bytes: usize,
    truncated: bool,
}

/// What the host says in place of the output past the cap.
pub const TRUNCATED_MARKER: &str =
    "[output truncated: the limit of 8 MiB for one invocation was reached]";
pub const RELAY_QUEUE_FRAMES: usize = 32;

struct State {
    pending: HashMap<u64, oneshot::Sender<Reply>>,
    /// Calls that listen for `invoke.output`, by request id.
    progress: HashMap<u64, Progress>,
    next_id: u64,
    tombstones: Tombstones,
    strikes: StrikeWindow,
}

struct Shared {
    cfg: ConnConfig,
    state: Mutex<State>,
    /// Set when the first frame is queued to the adapter: before that the adapter must be silent.
    spoke: std::sync::atomic::AtomicBool,
    initialized: Option<CancellationToken>,
    initialize_id: AtomicU64,
    out: mpsc::Sender<WriterMsg>,
    closed: watch::Sender<Option<CloseReason>>,
    cancel: CancellationToken,
    counters: AtomicCounters,
    inbound_in_flight: AtomicU32,
}

/// The connection to one adapter. Dropping it stops its tasks and closes the adapter's stdin.
pub struct AdapterConn {
    shared: Arc<Shared>,
}

impl Drop for AdapterConn {
    fn drop(&mut self) {
        self.shared.close(CloseReason::Local);
    }
}

impl Shared {
    fn close(&self, reason: CloseReason) {
        let first = self.closed.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason.clone());
                true
            } else {
                false
            }
        });
        if !first {
            return;
        }
        self.cancel.cancel();
        let text = reason.to_string();
        let drained: Vec<_> = self.state.lock().pending.drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(CallError::Crashed(text.clone())));
        }
    }

    fn close_reason(&self) -> Option<CloseReason> {
        self.closed.borrow().clone()
    }

    /// Record a protocol violation; three inside the window end the connection.
    fn strike(&self, why: &str) {
        self.counters.strikes.fetch_add(1, Ordering::Relaxed);
        let (count, kill) = self.state.lock().strikes.record(Instant::now());
        debug!("adapter protocol strike {count}: {why}");
        if kill {
            warn!("adapter cut off after {count} protocol violations (last: {why})");
            self.close(CloseReason::Strikes);
        }
    }

    /// Queue a line for the writer without waiting. False when the queue is full or closed.
    fn try_queue(&self, line: Vec<u8>) -> bool {
        self.spoke.store(true, Ordering::SeqCst);
        match self.out.try_send(WriterMsg::Line(line)) {
            Ok(()) => {
                self.counters.frames_out.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }

    fn send_cancel(&self, id: u64) {
        if let Ok(frame) = Frame::notification(method::CANCEL, &CancelParams { id }) {
            let _ = self.try_queue(frame.encode());
        }
    }

    /// The deadline passed, or the caller went away: forget the entry, remember the id and tell
    /// the adapter it may stop.
    fn abandon(&self, id: u64) {
        let removed = {
            let mut st = self.state.lock();
            let removed = st.pending.remove(&id).is_some();
            if removed {
                st.tombstones.insert(id, Instant::now());
            }
            removed
        };
        if removed {
            self.send_cancel(id);
        }
    }

    /// An `invoke.output` notification arrived. A frame for a call that does not listen is
    /// ignored (a late one, or an adapter talking to itself); a long text is cut at the frame
    /// limit; past the total the output is dropped and one marker goes in its place.
    fn on_output(&self, params: Params) {
        let Ok(output) = oxidepm_adapter_proto::from_params::<InvokeOutputParams>(&params) else {
            return;
        };
        let limits = &self.cfg.limits;
        let mut st = self.state.lock();
        let Some(progress) = st.progress.get_mut(&output.id) else {
            return;
        };
        if progress.truncated {
            return;
        }
        let mut text = output.text;
        if text.len() > limits.invoke_output_frame {
            let mut cut = limits.invoke_output_frame;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        let frame = if progress.bytes + text.len() > limits.invoke_output_total {
            progress.truncated = true;
            Output {
                stream: OutputStream::Status,
                text: TRUNCATED_MARKER.to_string(),
            }
        } else {
            progress.bytes += text.len();
            Output {
                stream: output.stream,
                text,
            }
        };
        if progress.tx.try_send(frame).is_err() {
            st.progress.remove(&output.id);
            if let Some(reply) = st.pending.remove(&output.id) {
                st.tombstones.insert(output.id, Instant::now());
                drop(st);
                self.send_cancel(output.id);
                let _ = reply.send(Err(CallError::OutputBackpressure));
            }
        }
    }

    /// A response or an error with `id` arrived.
    fn on_reply(&self, id: u64, reply: Reply) {
        let now = Instant::now();
        let outcome = {
            let mut st = self.state.lock();
            st.progress.remove(&id);
            match st.pending.remove(&id) {
                Some(tx) => Ok(tx),
                None if st.tombstones.contains(id, now) => Err(true),
                None => Err(false),
            }
        };
        match outcome {
            Ok(tx) => {
                let _ = tx.send(reply);
            }
            Err(true) => {
                self.counters.late_dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(false) => {
                self.strike("a reply to a request that was never issued, or was already answered")
            }
        }
    }

    /// Answer a request of the adapter. A result that does not fit in a frame becomes an error,
    /// so the daemon never buffers an oversize frame for an adapter.
    fn respond(&self, id: u64, outcome: Result<Params, ErrorObject>) {
        let max = self.cfg.limits.max_line;
        let frame = match outcome {
            Ok(result) => Frame::Response { id, result },
            Err(error) => Frame::Error { id, error },
        };
        let line = match frame.encode_limited(max) {
            Ok(line) => line,
            Err(FrameError::TooLarge { size, max }) => Frame::Error {
                id,
                error: ErrorObject::with_name(
                    rpc::INTERNAL_ERROR,
                    codes::E_REQUEST_TOO_LARGE,
                    format!("the answer is {size} bytes; a frame holds at most {max}"),
                ),
            }
            .encode(),
            Err(_) => return,
        };
        if !self.try_queue(line) {
            debug!("could not queue the answer to the adapter's request {id}");
        }
    }
}

struct Guard<'a> {
    shared: &'a Shared,
    id: u64,
    armed: bool,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.abandon(self.id);
        }
        // the call is over, whichever way: nothing listens for its output any more
        self.shared.state.lock().progress.remove(&self.id);
    }
}

impl AdapterConn {
    /// Start the reader and writer tasks over `reader` (the adapter's stdout) and `writer` (its
    /// stdin).
    pub fn start<R, W>(
        reader: R,
        writer: W,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
    ) -> Arc<AdapterConn>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::start_with(reader, writer, cfg, inbound, None)
    }

    pub(super) fn start_handshaking<R, W>(
        reader: R,
        writer: W,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
    ) -> Arc<AdapterConn>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::start_with(reader, writer, cfg, inbound, Some(CancellationToken::new()))
    }

    fn start_with<R, W>(
        reader: R,
        writer: W,
        cfg: ConnConfig,
        inbound: Arc<dyn Inbound>,
        initialized: Option<CancellationToken>,
    ) -> Arc<AdapterConn>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::channel(cfg.outbound_queue.max(1));
        let limits = cfg.limits;
        let shared = Arc::new(Shared {
            cfg,
            state: Mutex::new(State {
                pending: HashMap::new(),
                progress: HashMap::new(),
                next_id: 1,
                tombstones: Tombstones::new(
                    limits.tombstones,
                    Duration::from_secs(limits.tombstone_secs),
                ),
                strikes: StrikeWindow::new(
                    Duration::from_secs(limits.strike_window_secs),
                    limits.strikes_to_kill,
                ),
            }),
            spoke: std::sync::atomic::AtomicBool::new(false),
            initialized,
            initialize_id: AtomicU64::new(0),
            out: out_tx,
            closed: watch::channel(None).0,
            cancel: CancellationToken::new(),
            counters: AtomicCounters::default(),
            inbound_in_flight: AtomicU32::new(0),
        });

        let inbound = inbound
            .for_connection(StrikeHandle(Arc::downgrade(&shared)))
            .unwrap_or(inbound);

        // Each task has a watcher: a panic ends this connection and nothing else.
        let reader_task = tokio::spawn(reader_loop(Arc::clone(&shared), reader, inbound));
        let writer_task = tokio::spawn(writer_loop(Arc::clone(&shared), writer, out_rx));
        for task in [reader_task, writer_task] {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                if let Err(e) = task.await {
                    if e.is_panic() {
                        warn!("a task of an adapter connection panicked");
                        shared.close(CloseReason::Panicked);
                    }
                }
            });
        }
        Arc::new(AdapterConn { shared })
    }

    pub(super) fn accept_handshake(&self) {
        if let Some(initialized) = &self.shared.initialized {
            initialized.cancel();
        }
    }

    /// Send a request and wait for its answer, at most `deadline`. Never waits for the adapter
    /// beyond it. If the returned future is dropped, the request is abandoned the same way.
    pub async fn call<P: Serialize>(
        &self,
        method: &str,
        params: &P,
        deadline: Duration,
    ) -> Result<Params, CallError> {
        self.call_inner(method, params, deadline, None).await
    }

    /// Like [`call`](Self::call), for a request whose adapter reports progress: every
    /// `invoke.output` frame for this request goes to `progress` until the call ends, at most 64
    /// KiB of text per frame and 8 MiB in all (the rest is dropped and one marker is sent in its
    /// place). The frames reach the channel before the answer reaches the caller. If the queue
    /// fills, the invocation is cancelled with `OutputBackpressure`.
    pub async fn call_with_progress<P: Serialize>(
        &self,
        method: &str,
        params: &P,
        deadline: Duration,
        progress: mpsc::Sender<Output>,
    ) -> Result<Params, CallError> {
        self.call_inner(method, params, deadline, Some(progress))
            .await
    }

    async fn call_inner<P: Serialize>(
        &self,
        method: &str,
        params: &P,
        deadline: Duration,
        progress: Option<mpsc::Sender<Output>>,
    ) -> Result<Params, CallError> {
        let shared = &*self.shared;
        if let Some(reason) = shared.close_reason() {
            return Err(CallError::Crashed(reason.to_string()));
        }
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut st = shared.state.lock();
            if st.pending.len() >= shared.cfg.limits.max_in_flight as usize {
                return Err(CallError::Busy);
            }
            let id = st.next_id;
            st.next_id += 1;
            st.pending.insert(id, tx);
            if let Some(tx) = progress {
                st.progress.insert(
                    id,
                    Progress {
                        tx,
                        bytes: 0,
                        truncated: false,
                    },
                );
            }
            id
        };
        let mut guard = Guard {
            shared,
            id,
            armed: true,
        };

        let frame = match Frame::request(id, method, params) {
            Ok(frame) => frame,
            Err(e) => {
                shared.state.lock().pending.remove(&id);
                guard.armed = false;
                return Err(CallError::BadReply(format!(
                    "cannot encode the request: {e}"
                )));
            }
        };
        let line = match frame.encode_limited(shared.cfg.limits.max_line) {
            Ok(line) => line,
            Err(_) => {
                shared.state.lock().pending.remove(&id);
                guard.armed = false;
                return Err(CallError::TooLarge);
            }
        };
        if method == method::INITIALIZE && shared.initialized.is_some() {
            shared.initialize_id.store(id, Ordering::Release);
        }
        shared.spoke.store(true, Ordering::SeqCst);
        match shared
            .out
            .send_timeout(WriterMsg::Line(line), shared.cfg.send_wait)
            .await
        {
            Ok(()) => {
                shared.counters.frames_out.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::SendTimeoutError::Timeout(_)) => {
                shared.state.lock().pending.remove(&id);
                guard.armed = false;
                return Err(CallError::Busy);
            }
            Err(mpsc::error::SendTimeoutError::Closed(_)) => {
                shared.state.lock().pending.remove(&id);
                guard.armed = false;
                let why = shared
                    .close_reason()
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "the connection is closed".into());
                return Err(CallError::Crashed(why));
            }
        }

        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(reply)) => {
                guard.armed = false;
                reply
            }
            Ok(Err(_)) => {
                // the sender was dropped without an answer: the connection ended
                guard.armed = false;
                let why = shared
                    .close_reason()
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "the connection ended".into());
                Err(CallError::Crashed(why))
            }
            // the guard abandons the entry when it drops here
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Like [`call`](Self::call), reading the result into `R`. A result of the wrong shape is a
    /// strike and a [`CallError::BadReply`].
    pub async fn call_typed<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        deadline: Duration,
    ) -> Result<R, CallError> {
        let result = self.call(method, params, deadline).await?;
        match oxidepm_adapter_proto::from_params::<R>(&result) {
            Ok(value) => Ok(value),
            Err(e) => {
                self.shared.strike("a reply of the wrong shape");
                Err(CallError::BadReply(format!(
                    "the reply to {method} has the wrong shape: {e}"
                )))
            }
        }
    }

    /// Send a notification. It is dropped, not waited for, when the queue is full.
    pub fn notify<P: Serialize>(&self, method: &str, params: &P) -> Result<(), CallError> {
        if let Some(reason) = self.shared.close_reason() {
            return Err(CallError::Crashed(reason.to_string()));
        }
        let line = Frame::notification(method, params)
            .map_err(|e| CallError::BadReply(e.to_string()))?
            .encode_limited(self.shared.cfg.limits.max_line)
            .map_err(|_| CallError::TooLarge)?;
        if self.shared.try_queue(line) {
            Ok(())
        } else {
            Err(CallError::Busy)
        }
    }

    /// Close the adapter's stdin (its cue to exit) and stop writing. Reading goes on.
    pub fn close_input(&self) {
        let _ = self.shared.out.try_send(WriterMsg::CloseInput);
    }

    /// End the connection now: stops both tasks, fails everything in flight and closes stdin.
    pub fn close(&self) {
        self.shared.close(CloseReason::Local);
    }

    /// Wait until the connection has ended and say why.
    pub async fn closed(&self) -> CloseReason {
        let mut rx = self.shared.closed.subscribe();
        let reason = match rx.wait_for(|c| c.is_some()).await {
            Ok(reason) => reason.clone(),
            Err(_) => None,
        };
        reason.unwrap_or(CloseReason::Local)
    }

    pub fn close_reason(&self) -> Option<CloseReason> {
        self.shared.close_reason()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.close_reason().is_some()
    }

    pub fn counters(&self) -> Counters {
        let c = &self.shared.counters;
        Counters {
            strikes: c.strikes.load(Ordering::Relaxed),
            late_dropped: c.late_dropped.load(Ordering::Relaxed),
            violations: c.violations.load(Ordering::Relaxed),
            overflows: c.overflows.load(Ordering::Relaxed),
            frames_in: c.frames_in.load(Ordering::Relaxed),
            frames_out: c.frames_out.load(Ordering::Relaxed),
            throttle_strikes: c.throttle_strikes.load(Ordering::Relaxed),
            reader_peak_capacity: c.reader_peak.load(Ordering::Relaxed),
        }
    }

    /// Requests waiting for an answer right now.
    pub fn in_flight(&self) -> usize {
        self.shared.state.lock().pending.len()
    }

    /// Ids remembered as expired or cancelled.
    pub fn tombstone_count(&self) -> usize {
        self.shared.state.lock().tombstones.len()
    }

    /// Record a strike from outside (the owner found a reply wrong in a way only it can see).
    pub fn strike(&self, why: &str) {
        self.shared.strike(why);
    }
}

// ---------------------------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------------------------

async fn writer_loop<W: AsyncWrite + Unpin>(
    shared: Arc<Shared>,
    mut writer: W,
    mut rx: mpsc::Receiver<WriterMsg>,
) {
    loop {
        let msg = tokio::select! {
            _ = shared.cancel.cancelled() => break,
            msg = rx.recv() => msg,
        };
        let line = match msg {
            Some(WriterMsg::Line(line)) => line,
            // closing stdin ends this task; dropping the writer is what the adapter sees
            Some(WriterMsg::CloseInput) | None => break,
        };
        let write = async {
            writer.write_all(&line).await?;
            writer.flush().await
        };
        let result = tokio::select! {
            _ = shared.cancel.cancelled() => break,
            r = tokio::time::timeout(shared.cfg.write_timeout, write) => r,
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // a broken pipe is the adapter having gone: the reader reports the end
                if e.kind() != io::ErrorKind::BrokenPipe {
                    shared.close(CloseReason::WriteError(e.to_string()));
                }
                break;
            }
            Err(_) => {
                shared.close(CloseReason::WriteStalled);
                break;
            }
        }
    }
    let _ = writer.shutdown().await;
}

async fn reader_loop<R: AsyncRead + Unpin + Send + 'static>(
    shared: Arc<Shared>,
    read: R,
    inbound: Arc<dyn Inbound>,
) {
    let cfg = shared.cfg;
    let mut reader = AsyncFrameReader::new(BufReader::new(read), cfg.limits.max_line);
    let discard_limit = cfg.limits.max_line.saturating_mul(2) as u64;

    let start = Instant::now();
    let mut frames = TokenBucket::new(
        cfg.limits.frames_per_sec as f64,
        cfg.limits.frames_burst as f64,
        start,
    );
    let mut bytes = TokenBucket::new(
        cfg.limits.bytes_per_sec as f64,
        cfg.limits.bytes_per_sec as f64,
        start,
    );
    let mut throttle = Throttle::new(cfg.throttle_strike);

    loop {
        let read = tokio::select! {
            _ = shared.cancel.cancelled() => return,
            r = reader.read_step() => r,
        };
        shared
            .counters
            .reader_peak
            .fetch_max(reader.assembler().peak_capacity(), Ordering::Relaxed);

        let step = match read {
            Err(e) => {
                shared.close(CloseReason::ReadError(e.to_string()));
                return;
            }
            Ok(step) => step,
        };
        if step.overflow_started {
            shared.counters.overflows.fetch_add(1, Ordering::Relaxed);
            shared.strike("a line over the size limit");
        }
        if step.oversized_line_bytes > discard_limit {
            shared.close(CloseReason::Desynchronized);
            return;
        }
        // Charge bytes even while a partial or discarded line is still arriving.
        let now = Instant::now();
        let frame_cost = if matches!(
            step.event,
            Some(Read::Frame(_) | Read::Overflow { .. } | Read::Violation(_))
        ) {
            1.0
        } else {
            0.0
        };
        let wait = frames
            .reserve(frame_cost, now)
            .max(bytes.reserve(step.consumed as f64, now));
        let mut strike = throttle.observe(now, !wait.is_zero());
        if !wait.is_zero() {
            tokio::select! {
                _ = shared.cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            strike |= throttle.observe(Instant::now(), true);
        }
        if strike {
            shared
                .counters
                .throttle_strikes
                .fetch_add(1, Ordering::Relaxed);
            shared.strike("throttled for too long");
        }
        if shared.close_reason().is_some() {
            return;
        }
        match step.event {
            Some(Read::Eof) => {
                shared.close(CloseReason::Eof);
                return;
            }
            Some(Read::Truncated { .. }) => {
                shared.close(CloseReason::Truncated);
                return;
            }
            None | Some(Read::Overflow { .. }) => {}
            Some(Read::Violation(e)) => {
                shared.counters.violations.fetch_add(1, Ordering::Relaxed);
                shared.strike(&format!("a line that is not a valid frame: {e}"));
            }
            Some(Read::Frame(frame)) => {
                shared.counters.frames_in.fetch_add(1, Ordering::Relaxed);
                if !shared.spoke.load(Ordering::SeqCst) {
                    // the adapter stays silent until it hears `initialize`
                    shared.strike("a frame before the daemon spoke");
                } else if let Some(gate) = shared
                    .initialized
                    .as_ref()
                    .filter(|gate| !gate.is_cancelled())
                {
                    let expected = shared.initialize_id.load(Ordering::Acquire);
                    match frame {
                        Frame::Response { id, .. } | Frame::Error { id, .. }
                            if id != 0 && id == expected =>
                        {
                            dispatch(&shared, &inbound, frame);
                            // Preserve ordering for frames immediately following initialize.
                            tokio::select! {
                                _ = shared.cancel.cancelled() => return,
                                _ = gate.cancelled() => {}
                            }
                        }
                        Frame::Request { id, .. } => shared.respond(
                            id,
                            Err(ErrorObject::new(
                                rpc::PERMISSION_DENIED,
                                "the adapter handshake has not completed",
                            )),
                        ),
                        _ => shared.strike("unexpected frame during the adapter handshake"),
                    }
                } else {
                    dispatch(&shared, &inbound, frame);
                }
            }
        }
    }
}

fn dispatch(shared: &Arc<Shared>, inbound: &Arc<dyn Inbound>, frame: Frame) {
    match frame {
        Frame::Response { id, result } => shared.on_reply(id, Ok(result)),
        Frame::Error { id, error } => shared.on_reply(id, Err(CallError::Remote(error))),
        Frame::Notification { method, params } => {
            if method == method::INVOKE_OUTPUT {
                shared.on_output(params);
            } else {
                inbound.notification(&method, params)
            }
        }
        Frame::Request { id, method, params } => {
            // adapter-originated requests are bounded; the answer is built in its own task
            let limit = shared.cfg.limits.host_in_flight;
            if shared.inbound_in_flight.fetch_add(1, Ordering::SeqCst) >= limit {
                shared.inbound_in_flight.fetch_sub(1, Ordering::SeqCst);
                shared.respond(
                    id,
                    Err(ErrorObject::with_name(
                        rpc::BUSY,
                        codes::E_ADAPTER_BUSY,
                        "too many requests in flight",
                    )),
                );
                return;
            }
            let shared = Arc::clone(shared);
            let inbound = Arc::clone(inbound);
            tokio::spawn(async move {
                let outcome = tokio::select! {
                    _ = shared.cancel.cancelled() => None,
                    r = inbound.request(&method, params) => Some(r),
                };
                shared.inbound_in_flight.fetch_sub(1, Ordering::SeqCst);
                if let Some(outcome) = outcome {
                    shared.respond(id, outcome);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn token_bucket_allows_the_burst_then_makes_the_caller_wait() {
        let now = t0();
        let mut b = TokenBucket::new(10.0, 5.0, now);
        for _ in 0..5 {
            assert_eq!(b.reserve(1.0, now), Duration::ZERO);
        }
        let wait = b.reserve(1.0, now);
        assert!(
            wait > Duration::from_millis(90) && wait <= Duration::from_millis(100),
            "{wait:?}"
        );
        // after the wait the debit is covered
        let later = now + Duration::from_millis(100);
        assert_eq!(b.reserve(0.0, later), Duration::ZERO);
        // try_take debits only what is there
        let mut c = TokenBucket::new(10.0, 5.0, now);
        assert!(c.try_take(5.0, now));
        assert!(!c.try_take(1.0, now), "empty");
        assert!(
            c.try_take(1.0, now + Duration::from_millis(100)),
            "one token came back"
        );
        // a long pause refills to the burst only
        let much_later = later + Duration::from_secs(60);
        for _ in 0..5 {
            assert_eq!(b.reserve(1.0, much_later), Duration::ZERO);
        }
        assert!(b.reserve(1.0, much_later) > Duration::ZERO);
    }

    #[test]
    fn strike_window_kills_on_the_third_strike_inside_the_window_only() {
        let now = t0();
        let mut w = StrikeWindow::new(Duration::from_secs(60), 3);
        assert_eq!(w.record(now), (1, false));
        assert_eq!(w.record(now + Duration::from_secs(30)), (2, false));
        // the first has left the window: still two
        assert_eq!(w.record(now + Duration::from_secs(70)), (2, false));
        assert_eq!(w.record(now + Duration::from_secs(80)), (3, true));
    }

    #[test]
    fn throttle_period_survives_a_timer_that_overshoots() {
        let now = t0();
        let ms = Duration::from_millis;
        let mut t = Throttle::new(ms(250));
        // a flood on a loaded machine: throttled, but every third look finds the bucket in credit
        let mut struck_at = None;
        for i in 0..200u64 {
            let at = now + ms(i * 3);
            if t.observe(at, i % 3 != 0) {
                struck_at = Some(i * 3);
                break;
            }
        }
        assert!(
            struck_at.is_some_and(|ms| (250..=258).contains(&ms)),
            "{struck_at:?}"
        );
    }

    #[test]
    fn throttle_period_ends_after_a_quiet_gap() {
        let now = t0();
        let ms = Duration::from_millis;
        let mut t = Throttle::new(ms(250));
        assert!(!t.observe(now, true));
        assert!(!t.observe(now + ms(100), true));
        // the adapter goes quiet for longer than half the period (125 ms): the period is over
        assert!(!t.observe(now + ms(240), false));
        // so being throttled again at 260 ms is the start of a new period, not the end of the old
        assert!(!t.observe(now + ms(260), true));
        assert!(!t.observe(now + ms(480), true));
        assert!(t.observe(now + ms(510), true));
        // and the next period starts at the strike
        assert!(!t.observe(now + ms(600), true));
    }

    #[test]
    fn tombstones_are_bounded_and_expire() {
        let now = t0();
        let mut t = Tombstones::new(3, Duration::from_secs(300));
        for id in 1..=5 {
            t.insert(id, now);
        }
        assert_eq!(t.len(), 3, "oldest are dropped past the cap");
        assert!(!t.contains(1, now) && !t.contains(2, now));
        assert!(t.contains(5, now));
        assert!(!t.contains(5, now + Duration::from_secs(301)), "expired");
        assert!(t.is_empty());
    }
}
