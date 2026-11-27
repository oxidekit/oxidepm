//! Counters of the tasks the supervisor owns, for tests (`Supervisor::stats`).
//!
//! Compiled only with the `test-stats` feature: without it a [`Counted`] guard is a zero-sized
//! type and costs nothing, so production builds carry no counters.

/// The kinds of task that are counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Actor,
    Health,
    Cron,
    Capture,
    Pipeline,
}

/// What `Supervisor::stats` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupervisorStats {
    pub actors: usize,
    pub health_tasks: usize,
    pub cron_tasks: usize,
    pub pipelines: usize,
    pub capture_tasks: usize,
}

#[cfg(feature = "test-stats")]
mod imp {
    use super::{Kind, SupervisorStats};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    pub struct Stats {
        actors: AtomicUsize,
        health: AtomicUsize,
        cron: AtomicUsize,
        capture: AtomicUsize,
        pipeline: AtomicUsize,
    }

    impl Stats {
        fn counter(&self, kind: Kind) -> &AtomicUsize {
            match kind {
                Kind::Actor => &self.actors,
                Kind::Health => &self.health,
                Kind::Cron => &self.cron,
                Kind::Capture => &self.capture,
                Kind::Pipeline => &self.pipeline,
            }
        }

        pub fn snapshot(&self) -> SupervisorStats {
            let get = |c: &AtomicUsize| c.load(Ordering::SeqCst);
            SupervisorStats {
                actors: get(&self.actors),
                health_tasks: get(&self.health),
                cron_tasks: get(&self.cron),
                pipelines: get(&self.pipeline),
                capture_tasks: get(&self.capture),
            }
        }
    }

    /// Counts one task of `kind` for as long as it lives.
    pub struct Counted {
        stats: Arc<Stats>,
        kind: Kind,
    }

    impl Counted {
        pub fn new(stats: &Arc<Stats>, kind: Kind) -> Self {
            stats.counter(kind).fetch_add(1, Ordering::SeqCst);
            Self {
                stats: Arc::clone(stats),
                kind,
            }
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.stats.counter(self.kind).fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(not(feature = "test-stats"))]
mod imp {
    use super::Kind;
    use std::sync::Arc;

    #[derive(Default)]
    pub struct Stats;

    impl Stats {}

    pub struct Counted;

    impl Counted {
        pub fn new(_stats: &Arc<Stats>, _kind: Kind) -> Self {
            Counted
        }
    }
}

pub use imp::{Counted, Stats};

/// A single sample, including the leader figures used by the existing memory limit.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Reading {
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    pub tree: oxidepm_core::ProcessTreeStats,
}

impl super::Supervisor {
    /// Live measurements for a registered application.
    pub fn app_stats(&self, id: super::AppId) -> Option<oxidepm_core::AppStats> {
        self.registry.get(id).map(|handle| oxidepm_core::AppStats {
            tree: handle.live().metrics().tree,
        })
    }
}
