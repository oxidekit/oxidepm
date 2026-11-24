//! Lifecycle events: one function emits them, sinks react.
//!
//! An event is core-owned data. The sinks are the hook sink (the app's `on_*` script), the adapter
//! sink (services that listed the `lifecycle` family) and, in tests, a recorder. `emit` never
//! awaits and a sink must not block: it only spawns or queues, so a stuck hook or adapter can
//! never hold up an actor.

use super::handle::{AppId, Cause};
use super::hooks::{HookCtx, HookRunner};
use chrono::{DateTime, Utc};
use oxidepm_core::{AppSpec, HookEvent};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    Started,
    Restarted,
    Stopped,
    Crashed,
    Errored,
    Unhealthy,
    Recovered,
    BackoffScheduled,
    CrashLoop,
    CronFired,
    MemoryLimit,
    UptimeLimit,
}

/// What the event is about.
#[derive(Debug, Clone)]
pub struct EventDetail {
    pub spec: Arc<AppSpec>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub restarts: u32,
    pub reason: Option<String>,
    /// `Limit`: observed and limit, in MB.
    pub memory_mb: Option<(u64, u64)>,
}

impl EventDetail {
    pub fn new(spec: &Arc<AppSpec>) -> Self {
        Self {
            spec: Arc::clone(spec),
            pid: None,
            exit_code: None,
            exit_signal: None,
            restarts: 0,
            reason: None,
            memory_mb: None,
        }
    }
}

/// An event before it has a sequence number and a time.
#[derive(Debug, Clone)]
pub struct EventDraft {
    pub app_id: AppId,
    pub kind: LifecycleKind,
    pub cause: Cause,
    pub actor: Option<Arc<str>>,
    pub detail: EventDetail,
}

#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    /// Monotonic across the daemon.
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub app_id: AppId,
    pub app: Arc<str>,
    pub kind: LifecycleKind,
    pub cause: Cause,
    pub actor: Option<Arc<str>>,
    pub detail: EventDetail,
}

/// Reacts to events. MUST NOT block or await.
pub trait EventSink: Send + Sync {
    fn on_event(&self, ev: &LifecycleEvent);
}

pub struct Lifecycle {
    seq: AtomicU64,
    sinks: Vec<Arc<dyn EventSink>>,
}

impl Lifecycle {
    pub fn new(sinks: Vec<Arc<dyn EventSink>>) -> Self {
        Self {
            seq: AtomicU64::new(0),
            sinks,
        }
    }

    /// Build the event and hand it to every sink, synchronously.
    pub fn emit(&self, draft: EventDraft) {
        let event = LifecycleEvent {
            seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
            at: Utc::now(),
            app_id: draft.app_id,
            app: Arc::from(draft.detail.spec.name.as_str()),
            kind: draft.kind,
            cause: draft.cause,
            actor: draft.actor,
            detail: draft.detail,
        };
        debug!(
            "event #{} {:?} for {} ({:?}, by {:?}) at {}",
            event.seq, event.kind, event.app, event.cause, event.actor, event.at
        );
        for sink in &self.sinks {
            sink.on_event(&event);
        }
    }
}

/// Runs the app's hook for the events that have one.
pub struct HookSink {
    hooks: Arc<HookRunner>,
}

impl HookSink {
    pub fn new(hooks: Arc<HookRunner>) -> Self {
        Self { hooks }
    }

    /// Which hook an event fires.
    pub fn hook_for(kind: LifecycleKind) -> Option<HookEvent> {
        match kind {
            LifecycleKind::Started => Some(HookEvent::Start),
            LifecycleKind::Restarted => Some(HookEvent::Restart),
            LifecycleKind::Stopped => Some(HookEvent::Stop),
            LifecycleKind::Crashed => Some(HookEvent::Crash),
            LifecycleKind::Unhealthy | LifecycleKind::Errored => Some(HookEvent::Error),
            LifecycleKind::Recovered
            | LifecycleKind::BackoffScheduled
            | LifecycleKind::CrashLoop
            | LifecycleKind::CronFired
            | LifecycleKind::MemoryLimit
            | LifecycleKind::UptimeLimit => None,
        }
    }
}

impl EventSink for HookSink {
    fn on_event(&self, ev: &LifecycleEvent) {
        let Some(hook) = Self::hook_for(ev.kind) else {
            return;
        };
        self.hooks.fire(
            &ev.detail.spec,
            hook,
            HookCtx {
                app_id: ev.app_id,
                pid: ev.detail.pid,
                exit_code: ev.detail.exit_code,
                reason: ev.detail.reason.clone(),
                restarts: ev.detail.restarts,
            },
        );
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use oxidepm_core::Runtime;
    use parking_lot::Mutex;

    /// Collects every event (for tests).
    #[derive(Default)]
    pub struct Recorder {
        pub events: Mutex<Vec<LifecycleEvent>>,
    }

    impl EventSink for Recorder {
        fn on_event(&self, ev: &LifecycleEvent) {
            self.events.lock().push(ev.clone());
        }
    }

    fn draft(kind: LifecycleKind) -> EventDraft {
        let spec = Arc::new(AppSpec::new(
            "e".into(),
            Runtime::CMD,
            "true".into(),
            "/".into(),
        ));
        EventDraft {
            app_id: 4,
            kind,
            cause: Cause::User,
            actor: None,
            detail: EventDetail::new(&spec),
        }
    }

    #[test]
    fn emit_numbers_events_monotonically() {
        let recorder = Arc::new(Recorder::default());
        let lifecycle = Lifecycle::new(vec![recorder.clone() as Arc<dyn EventSink>]);
        lifecycle.emit(draft(LifecycleKind::Started));
        lifecycle.emit(draft(LifecycleKind::Stopped));
        let events = recorder.events.lock();
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(&*events[0].app, "e");
    }

    #[test]
    fn hooks_follow_the_event_table() {
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Started),
            Some(HookEvent::Start)
        );
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Restarted),
            Some(HookEvent::Restart)
        );
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Stopped),
            Some(HookEvent::Stop)
        );
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Crashed),
            Some(HookEvent::Crash)
        );
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Errored),
            Some(HookEvent::Error)
        );
        assert_eq!(
            HookSink::hook_for(LifecycleKind::Unhealthy),
            Some(HookEvent::Error)
        );
        assert_eq!(HookSink::hook_for(LifecycleKind::MemoryLimit), None);
        assert_eq!(HookSink::hook_for(LifecycleKind::BackoffScheduled), None);
        assert_eq!(HookSink::hook_for(LifecycleKind::CronFired), None);
        assert_eq!(HookSink::hook_for(LifecycleKind::Recovered), None);
    }
}
