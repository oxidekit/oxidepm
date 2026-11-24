//! The probe loop of one launch.
//!
//! One task per launch, owned by the launch token: it waits out the start period, then probes at
//! a fixed rate (a slow probe delays the next one, so probes never pile up and a short interval
//! cannot hammer the app). It keeps going after a failure, so a recovery is seen. It tells the
//! actor only about transitions: the first success and every recovery (`HealthOk`), and the
//! moment `retries` failures in a row are reached (`HealthFailed`, once per streak).

use super::actor::{probe_context, AbortOnDrop};
use super::handle::{post, Command, Live};
use super::stats::Counted;
use oxidepm_core::AppSpec;
use oxidepm_health::HealthChecker;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::WeakSender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// What a probe result changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The first success, or a success after the app was reported unhealthy.
    Healthy,
    /// `retries` failures in a row.
    Unhealthy { failures: u32 },
}

/// Turns probe results into transitions (the only thing posted to the actor).
#[derive(Debug)]
pub struct ProbeTracker {
    retries: u32,
    failures: u32,
    /// `None` until something was reported; `Some(false)` while a streak is reported.
    healthy: Option<bool>,
}

impl ProbeTracker {
    pub fn new(retries: u32) -> Self {
        Self {
            retries: retries.max(1),
            failures: 0,
            healthy: None,
        }
    }

    /// Consecutive failures so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn observe(&mut self, ok: bool) -> Option<Transition> {
        if ok {
            self.failures = 0;
            let changed = self.healthy != Some(true);
            self.healthy = Some(true);
            changed.then_some(Transition::Healthy)
        } else {
            self.failures = self.failures.saturating_add(1);
            if self.failures == self.retries {
                self.healthy = Some(false);
                Some(Transition::Unhealthy {
                    failures: self.failures,
                })
            } else {
                None
            }
        }
    }

    /// The actor could not be told (its mailbox was full): report the same transition again
    /// with the next probe.
    pub fn unreport(&mut self, transition: Transition) {
        match transition {
            Transition::Healthy => self.healthy = None,
            Transition::Unhealthy { .. } => {
                self.failures = self.retries.saturating_sub(1);
                self.healthy = None;
            }
        }
    }
}

/// What a probe loop needs.
pub struct HealthTask {
    pub spec: Arc<AppSpec>,
    pub live: Arc<Live>,
    pub tx: WeakSender<Command>,
    /// The generation of the launch the probes are for; the actor drops stale reports.
    pub gen: u64,
    pub token: CancellationToken,
    pub counted: Counted,
}

/// Start the loop; dropping the returned value ends it.
pub fn spawn(task: HealthTask) -> AbortOnDrop {
    let HealthTask {
        spec,
        live,
        tx,
        gen,
        token,
        counted,
    } = task;
    AbortOnDrop::counted(
        tokio::spawn(async move {
            let Some(config) = spec.health_check.clone() else {
                return;
            };
            let checker = HealthChecker::new().with_script_context(probe_context(&spec));
            let mut tracker = ProbeTracker::new(config.retries);

            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(config.start_period_secs)) => {}
            }
            let mut ticks = tokio::time::interval(Duration::from_secs(config.interval_secs.max(1)));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = ticks.tick() => {}
                }
                let result = tokio::select! {
                    _ = token.cancelled() => return,
                    result = checker.check(&config) => result,
                };

                let transition = tracker.observe(result.healthy);
                live.record_health(tracker.failures());
                if result.healthy {
                    debug!("Health check passed for app {}", spec.name);
                } else {
                    warn!(
                        "Health check failed for app {} ({} consecutive failures): {:?}",
                        spec.name,
                        tracker.failures(),
                        result.message
                    );
                }
                let Some(transition) = transition else {
                    continue;
                };
                let command = match transition {
                    Transition::Healthy => Command::HealthOk { gen },
                    Transition::Unhealthy { failures } => Command::HealthFailed {
                        gen,
                        failures,
                        message: result.message.unwrap_or_default(),
                    },
                };
                if !post(&tx, command) {
                    if tx.upgrade().is_none() {
                        return; // the app is gone
                    }
                    tracker.unreport(transition);
                }
            }
        }),
        counted,
    )
}

#[cfg(test)]
mod tests {
    use super::super::handle::{HandleKind, Snapshot};
    use super::super::stats::{Kind, Stats};
    use super::*;
    use oxidepm_core::{Desired, HealthCheck, Runtime};

    #[test]
    fn health_task_posts_only_on_transitions() {
        let mut t = ProbeTracker::new(3);
        // the first success is reported, the next ones are not
        assert_eq!(t.observe(true), Some(Transition::Healthy));
        assert_eq!(t.observe(true), None);
        assert_eq!(t.observe(true), None);
        // failures below `retries` change nothing, and a success in between starts over
        assert_eq!(t.observe(false), None);
        assert_eq!(t.observe(false), None);
        assert_eq!(
            t.observe(true),
            None,
            "never reported unhealthy, so nothing to recover from"
        );
        assert_eq!(t.observe(false), None);
        assert_eq!(t.observe(false), None);
        // the streak reaches `retries`: reported once
        assert_eq!(
            t.observe(false),
            Some(Transition::Unhealthy { failures: 3 })
        );
        assert_eq!(t.observe(false), None);
        assert_eq!(t.observe(false), None);
        // the recovery is reported once
        assert_eq!(t.observe(true), Some(Transition::Healthy));
        assert_eq!(t.observe(true), None);
        // and the next streak is reported again
        assert_eq!(t.observe(false), None);
        assert_eq!(t.observe(false), None);
        assert_eq!(
            t.observe(false),
            Some(Transition::Unhealthy { failures: 3 })
        );
    }

    #[test]
    fn a_streak_during_start_up_is_reported_and_its_recovery_too() {
        let mut t = ProbeTracker::new(1);
        assert_eq!(
            t.observe(false),
            Some(Transition::Unhealthy { failures: 1 })
        );
        assert_eq!(t.observe(false), None);
        assert_eq!(t.observe(true), Some(Transition::Healthy));
    }

    #[test]
    fn an_unposted_transition_is_reported_again() {
        let mut t = ProbeTracker::new(2);
        let first = t.observe(true).expect("reported");
        t.unreport(first);
        assert_eq!(t.observe(true), Some(Transition::Healthy));
        assert_eq!(t.observe(false), None);
        let streak = t.observe(false).expect("reported");
        t.unreport(streak);
        assert_eq!(
            t.observe(false),
            Some(Transition::Unhealthy { failures: 2 })
        );
    }

    /// The real loop, against a mailbox: it posts one `HealthOk` when the probe starts passing,
    /// nothing while it keeps passing, and no probe after the token is cancelled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_loop_posts_the_transition_and_stops_with_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("ok");
        let mut spec = AppSpec::new(
            "probe".into(),
            Runtime::CMD,
            "true".into(),
            dir.path().into(),
        );
        spec.health_check = Some(HealthCheck {
            interval_secs: 1,
            timeout_secs: 1,
            retries: 1,
            start_period_secs: 0,
            ..HealthCheck::script(format!("test -f '{}'", flag.display()))
        });
        let spec = Arc::new(spec);
        let snap = Snapshot::new(Arc::clone(&spec), Desired::Running, None);
        let (handle, mut parts) =
            super::super::handle::ProcessHandle::channel(1, HandleKind::Process, snap);
        let stats = Arc::new(Stats::default());
        let token = CancellationToken::new();
        let task = spawn(HealthTask {
            spec,
            live: Arc::clone(&parts.live),
            tx: parts.weak_tx.clone(),
            gen: 7,
            token: token.clone(),
            counted: Counted::new(&stats, Kind::Health),
        });

        // the probe fails at first: one failure with retries = 1 is a streak, reported once
        let first = tokio::time::timeout(Duration::from_secs(3), parts.rx.recv())
            .await
            .expect("a report")
            .expect("open");
        assert!(matches!(
            first,
            Command::HealthFailed {
                gen: 7,
                failures: 1,
                ..
            }
        ));
        // it keeps probing but says nothing while the streak goes on
        assert!(
            tokio::time::timeout(Duration::from_millis(1200), parts.rx.recv())
                .await
                .is_err()
        );

        // the probe starts passing: one recovery
        std::fs::write(&flag, "").unwrap();
        let second = tokio::time::timeout(Duration::from_secs(3), parts.rx.recv())
            .await
            .expect("a report")
            .expect("open");
        assert!(matches!(second, Command::HealthOk { gen: 7 }));
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), parts.rx.recv())
                .await
                .is_err(),
            "nothing more while it keeps passing"
        );

        token.cancel();
        std::fs::remove_file(&flag).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), parts.rx.recv())
                .await
                .is_err(),
            "a cancelled loop probes no more"
        );
        drop(task);
        drop(handle);
    }
}
