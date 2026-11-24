//! The numbers of protocol 1. Every one is enforced by the daemon, none by trusting
//! the adapter. They are constants of the protocol version, not configuration.

use serde::{Deserialize, Serialize};

/// The protocol versions this crate speaks.
pub const SUPPORTED_PROTOCOLS: &[u32] = &[1];

/// The limits of protocol 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest line (without the newline) from an adapter to the daemon. One limit serves every method.
    pub max_line: usize,
    /// Longest `event` line from the daemon to an adapter; the daemon trims `data` to fit.
    pub event_line: usize,
    /// Longest `invoke.output` frame text.
    pub invoke_output_frame: usize,
    /// Most `invoke.output` bytes relayed for one invocation.
    pub invoke_output_total: usize,
    /// Daemon-originated requests in flight per adapter.
    pub max_in_flight: u32,
    /// Adapter-originated `host.request`s in flight; a grant may raise the default up to `max_in_flight_ceiling`.
    pub host_in_flight: u32,
    pub max_in_flight_ceiling: u32,
    /// Normal events queued per service, and the extra critical ones.
    pub event_queue: u32,
    pub event_queue_critical: u32,
    pub event_queue_bytes: usize,
    /// Inbound frames per second from one adapter: sustained and burst.
    pub frames_per_sec: u32,
    pub frames_burst: u32,
    /// Inbound bytes per second from one adapter.
    pub bytes_per_sec: u64,
    /// `host.request` rate: sustained and burst per second.
    pub host_rate: u32,
    pub host_burst: u32,
    /// Caution operations: per minute and burst.
    pub caution_per_min: u32,
    pub caution_burst: u32,
    /// Strikes that kill an adapter, and the window they count in (seconds).
    pub strikes_to_kill: u32,
    pub strike_window_secs: u64,
    /// Expired or cancelled request ids remembered, and for how long (seconds).
    pub tombstones: usize,
    pub tombstone_secs: u64,
    /// A runtime plan: `args` items and bytes.
    pub plan_args_items: usize,
    pub plan_args_bytes: usize,
    /// A step list, at most this many steps.
    pub max_steps: usize,
    /// `capture_max` of a step, at most this many bytes.
    pub capture_max: usize,
    /// `host.audit_note` text, in characters.
    pub audit_note_chars: usize,
}

impl Limits {
    pub const PROTOCOL_1: Limits = Limits {
        max_line: 256 * 1024,
        event_line: 16 * 1024,
        invoke_output_frame: 64 * 1024,
        invoke_output_total: 8 * 1024 * 1024,
        max_in_flight: 32,
        host_in_flight: 32,
        max_in_flight_ceiling: 128,
        event_queue: 1024,
        event_queue_critical: 256,
        event_queue_bytes: 4 * 1024 * 1024,
        frames_per_sec: 500,
        frames_burst: 1000,
        bytes_per_sec: 8 * 1024 * 1024,
        host_rate: 50,
        host_burst: 100,
        caution_per_min: 5,
        caution_burst: 10,
        strikes_to_kill: 3,
        strike_window_secs: 60,
        tombstones: 1024,
        tombstone_secs: 300,
        plan_args_items: 4096,
        plan_args_bytes: 128 * 1024,
        max_steps: 16,
        capture_max: 256 * 1024,
        audit_note_chars: 256,
    };

    /// The part of the limits that `initialize` tells the adapter about.
    pub fn for_initialize(&self) -> InitializeLimits {
        InitializeLimits {
            max_line: self.max_line as u64,
            max_in_flight: self.max_in_flight,
            event_queue: self.event_queue,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::PROTOCOL_1
    }
}

/// The `limits` object of the `initialize` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeLimits {
    pub max_line: u64,
    pub max_in_flight: u32,
    pub event_queue: u32,
}

/// Manifest and index size caps.
pub const MANIFEST_MAX_BYTES: usize = 16 * 1024;
pub const INDEX_MAX_BYTES: usize = 2 * 1024 * 1024;
