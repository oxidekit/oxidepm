//! The crash restart policy, as a pure state machine.
//!
//! The supervisor tells a [`CrashTracker`] every time a launch ended (the child exited, a probe
//! failed, the spawn itself failed) and gets back what to do: restart after a delay, give up, or
//! leave the app alone. Time is passed in, so the whole policy is testable without a clock.
//!
//! Rules (`record_exit`):
//! 1. A crash or a clean exit is only restarted when `auto_restart` is on and the app's
//!    `restart_mode` wants it; a failed health check, a limit restart and a failed spawn inside a
//!    restart chain ignore both.
//! 2. Exits older than `crash_window_secs` are forgotten. The new exit is remembered, except a
//!    limit restart of a run of at least [`MIN_RUN_FLOOR`]: a periodic restart the operator asked
//!    for never counts towards `max_restarts`.
//! 3. A run of at least `stable_uptime_secs` starts the doubling delay over.
//! 4. More than `max_restarts` remembered exits are a crash loop.
//! 5. Otherwise the delay is `restart_delay_ms` doubled for every attempt, up to
//!    `max_restart_delay_ms` (a `restart_delay_ms` above the cap is used as it is).

use crate::types::{RestartMode, RestartPolicy};
use crate::validate::MAX_APP_TIMER_MS;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A limit restart of a run shorter than this counts as a crash: a limit that fires right after
/// every start is a misconfiguration that would otherwise spin.
pub const MIN_RUN_FLOOR: Duration = Duration::from_secs(5);

/// Why a launch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCause {
    /// The child died by itself with a non-zero code or a signal.
    Crash {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// The child exited with code 0.
    CleanExit,
    /// An automatic launch could not spawn the child.
    SpawnFailed,
    /// The health check failed and the app asks to be restarted for it.
    HealthFailed,
    /// A memory or uptime limit replaced the child.
    LimitRestart,
}

/// What the supervisor does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Launch again after `delay`; `attempt` counts the consecutive attempts (1 = first).
    Restart { delay: Duration, attempt: u32 },
    /// Too many exits inside the window: stop trying.
    GiveUp { exits_in_window: u32 },
    /// The policy does not want a restart (auto restart off, or the restart mode says no).
    NoRestart,
}

#[derive(Debug, Clone)]
pub struct CrashTracker {
    policy: RestartPolicy,
    history: VecDeque<Instant>,
    attempt: u32,
}

impl CrashTracker {
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            history: VecDeque::new(),
            attempt: 0,
        }
    }

    /// Record that a launch ended at `now` after running for `ran_for`.
    pub fn record_exit(
        &mut self,
        now: Instant,
        ran_for: Duration,
        cause: ExitCause,
        mode: RestartMode,
    ) -> Decision {
        // rule 1: only an exit the app itself caused needs the policy's permission
        let permitted = match cause {
            ExitCause::Crash { code, .. } => mode.should_restart(code),
            ExitCause::CleanExit => mode.should_restart(Some(0)),
            ExitCause::SpawnFailed | ExitCause::HealthFailed | ExitCause::LimitRestart => true,
        };
        let asked = matches!(cause, ExitCause::Crash { .. } | ExitCause::CleanExit);
        if asked && (!self.policy.auto_restart || !permitted) {
            return Decision::NoRestart;
        }

        // rule 2
        let window = Duration::from_secs(self.policy.crash_window_secs);
        while let Some(first) = self.history.front() {
            if now.saturating_duration_since(*first) > window {
                self.history.pop_front();
            } else {
                break;
            }
        }
        if !(cause == ExitCause::LimitRestart && ran_for >= MIN_RUN_FLOOR) {
            self.history.push_back(now);
        }

        // rules 3 and 4
        let stable_uptime = self.policy.stable_uptime_secs;
        if stable_uptime != 0 && ran_for >= Duration::from_secs(stable_uptime) {
            self.attempt = 0;
        }
        self.attempt = self.attempt.saturating_add(1);

        // rule 5
        let exits = u32::try_from(self.history.len()).unwrap_or(u32::MAX);
        if exits > self.policy.max_restarts {
            return Decision::GiveUp {
                exits_in_window: exits,
            };
        }
        Decision::Restart {
            delay: self.delay(),
            attempt: self.attempt,
        }
    }

    /// `restart_delay_ms` doubled `attempt - 1` times, capped (saturating). A base above the cap
    /// is the operator's choice and is never cut down to it.
    fn delay(&self) -> Duration {
        let base = self.policy.restart_delay_ms;
        let cap = self
            .policy
            .max_restart_delay_ms
            .max(base)
            .min(MAX_APP_TIMER_MS);
        let doublings = self.attempt.saturating_sub(1);
        let factor = if doublings >= 63 {
            u64::MAX
        } else {
            1u64 << doublings
        };
        Duration::from_millis(base.saturating_mul(factor).min(cap))
    }

    /// An operator start or restart gives the app a fresh budget.
    pub fn reset(&mut self) {
        self.history.clear();
        self.attempt = 0;
    }

    /// Consecutive attempts so far (0 after a reset or a stable run).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    fn policy() -> RestartPolicy {
        RestartPolicy::default()
    }

    fn crash() -> ExitCause {
        ExitCause::Crash {
            code: Some(1),
            signal: None,
        }
    }

    fn rec(t: &mut CrashTracker, at: Instant, ran: Duration, cause: ExitCause) -> Decision {
        t.record_exit(at, ran, cause, RestartMode::OnCrash)
    }

    fn delay_of(d: Decision) -> Duration {
        match d {
            Decision::Restart { delay, .. } => delay,
            other => panic!("expected a restart, got {other:?}"),
        }
    }

    #[test]
    fn stored_zero_stable_uptime_does_not_reset_backoff_on_spawn_failure() {
        let mut policy = policy();
        policy.stable_uptime_secs = 0;
        let mut tracker = CrashTracker::new(policy);
        let now = Instant::now();
        for attempt in 1..=2 {
            let decision = tracker.record_exit(
                now,
                Duration::ZERO,
                ExitCause::SpawnFailed,
                RestartMode::OnCrash,
            );
            assert!(
                matches!(decision, Decision::Restart { attempt: actual, .. } if actual == attempt)
            );
        }
    }

    #[test]
    fn crash_tracker_16_crashes_inside_window_give_up() {
        let mut t = CrashTracker::new(policy());
        let t0 = Instant::now();
        for i in 0..15u64 {
            let d = rec(
                &mut t,
                t0 + SEC * i as u32,
                Duration::from_millis(100),
                crash(),
            );
            assert!(matches!(d, Decision::Restart { .. }), "crash {i}: {d:?}");
        }
        let d = rec(&mut t, t0 + SEC * 15, Duration::from_millis(100), crash());
        assert_eq!(
            d,
            Decision::GiveUp {
                exits_in_window: 16
            }
        );
    }

    #[test]
    fn crash_tracker_16_crashes_spaced_beyond_window_keep_restarting() {
        let mut t = CrashTracker::new(policy());
        let t0 = Instant::now();
        // one crash every 100 s: only six fit in the 600 s window, however many happen
        for i in 0..40u32 {
            let d = rec(
                &mut t,
                t0 + SEC * 100 * i,
                Duration::from_millis(100),
                crash(),
            );
            assert!(matches!(d, Decision::Restart { .. }), "crash {i}: {d:?}");
        }
    }

    #[test]
    fn crash_tracker_delay_doubles_and_caps() {
        let mut p = policy();
        p.max_restarts = 1000;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        let mut delays = Vec::new();
        for i in 0..10u32 {
            let d = rec(&mut t, t0 + SEC * i, Duration::from_millis(10), crash());
            delays.push(delay_of(d).as_millis() as u64);
        }
        assert_eq!(
            delays,
            vec![500, 1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000, 60000]
        );
    }

    #[test]
    fn crash_tracker_resets_attempt_after_stable_uptime() {
        let mut t = CrashTracker::new(policy());
        let t0 = Instant::now();
        for i in 0..4u32 {
            rec(&mut t, t0 + SEC * i, Duration::from_millis(10), crash());
        }
        assert_eq!(t.attempt(), 4);
        // ran for the stable uptime: the delay starts over
        let d = rec(&mut t, t0 + SEC * 100, Duration::from_secs(60), crash());
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(500),
                attempt: 1
            }
        );
        // a run just below it does not
        let d = rec(&mut t, t0 + SEC * 101, Duration::from_secs(59), crash());
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(1000),
                attempt: 2
            }
        );
    }

    #[test]
    fn crash_tracker_spawn_failure_counts() {
        let mut p = policy();
        p.max_restarts = 3;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        assert_eq!(
            delay_of(rec(&mut t, t0, SEC, crash())),
            Duration::from_millis(500)
        );
        // a failed spawn is one more attempt: longer delay, and it fills the history
        let d = rec(&mut t, t0 + SEC, Duration::ZERO, ExitCause::SpawnFailed);
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(1000),
                attempt: 2
            }
        );
        let d = rec(&mut t, t0 + SEC * 2, Duration::ZERO, ExitCause::SpawnFailed);
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(2000),
                attempt: 3
            }
        );
        let d = rec(&mut t, t0 + SEC * 3, Duration::ZERO, ExitCause::SpawnFailed);
        assert_eq!(d, Decision::GiveUp { exits_in_window: 4 });
    }

    #[test]
    fn crash_tracker_spawn_failure_ignores_mode_and_auto_restart() {
        // the chain was permitted by the crash that started it
        let mut p = policy();
        p.auto_restart = false;
        let mut t = CrashTracker::new(p);
        let d = t.record_exit(
            Instant::now(),
            Duration::ZERO,
            ExitCause::SpawnFailed,
            RestartMode::Never,
        );
        assert!(matches!(d, Decision::Restart { .. }));
    }

    #[test]
    fn crash_tracker_mode_never_no_restart() {
        let mut t = CrashTracker::new(policy());
        let now = Instant::now();
        for cause in [
            crash(),
            ExitCause::CleanExit,
            ExitCause::Crash {
                code: None,
                signal: Some(9),
            },
        ] {
            let d = t.record_exit(now, SEC, cause, RestartMode::Never);
            assert_eq!(d, Decision::NoRestart, "{cause:?}");
        }
        // nothing was remembered
        assert_eq!(t.attempt(), 0);
        let mut p = policy();
        p.auto_restart = false;
        let mut t = CrashTracker::new(p);
        assert_eq!(
            t.record_exit(now, SEC, crash(), RestartMode::Always),
            Decision::NoRestart
        );
    }

    #[test]
    fn crash_tracker_exit_zero_on_crash_mode_no_restart() {
        let mut t = CrashTracker::new(policy());
        let now = Instant::now();
        assert_eq!(
            t.record_exit(now, SEC, ExitCause::CleanExit, RestartMode::OnCrash),
            Decision::NoRestart
        );
        // `always` restarts a clean exit too; a signal death counts as a crash for on-crash
        assert!(matches!(
            t.record_exit(now, SEC, ExitCause::CleanExit, RestartMode::Always),
            Decision::Restart { .. }
        ));
        assert!(matches!(
            t.record_exit(
                now,
                SEC,
                ExitCause::Crash {
                    code: None,
                    signal: Some(11)
                },
                RestartMode::OnCrash
            ),
            Decision::Restart { .. }
        ));
    }

    #[test]
    fn crash_tracker_health_failure_ignores_mode_and_auto_restart() {
        let mut p = policy();
        p.auto_restart = false;
        let mut t = CrashTracker::new(p);
        let d = t.record_exit(
            Instant::now(),
            SEC,
            ExitCause::HealthFailed,
            RestartMode::Never,
        );
        assert!(matches!(d, Decision::Restart { .. }));
    }

    #[test]
    fn crash_tracker_limit_restart_ignores_mode_but_backs_off() {
        let mut t = CrashTracker::new(policy());
        let t0 = Instant::now();
        let d = t.record_exit(
            t0,
            Duration::from_secs(10),
            ExitCause::LimitRestart,
            RestartMode::Never,
        );
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(500),
                attempt: 1
            }
        );
        let d = t.record_exit(
            t0 + SEC,
            Duration::from_secs(10),
            ExitCause::LimitRestart,
            RestartMode::Never,
        );
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(1000),
                attempt: 2
            }
        );
    }

    #[test]
    fn crash_tracker_limit_restart_at_or_above_floor_stays_out_of_history() {
        // max_uptime (30 s) below stable_uptime (60 s): the delay grows, the breaker never trips
        let mut p = policy();
        p.max_restarts = 3;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        for i in 0..50u32 {
            let d = t.record_exit(
                t0 + SEC * 30 * i,
                Duration::from_secs(30),
                ExitCause::LimitRestart,
                RestartMode::OnCrash,
            );
            assert!(matches!(d, Decision::Restart { .. }), "restart {i}: {d:?}");
        }
        // exactly at the floor is still out of the history
        let d = t.record_exit(
            t0 + SEC * 2000,
            MIN_RUN_FLOOR,
            ExitCause::LimitRestart,
            RestartMode::OnCrash,
        );
        assert!(matches!(d, Decision::Restart { .. }));
    }

    #[test]
    fn crash_tracker_limit_restart_below_floor_trips_breaker() {
        let mut p = policy();
        p.max_restarts = 3;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        let ran = Duration::from_secs(1);
        for i in 0..3u32 {
            let d = t.record_exit(
                t0 + SEC * i,
                ran,
                ExitCause::LimitRestart,
                RestartMode::OnCrash,
            );
            assert!(matches!(d, Decision::Restart { .. }), "restart {i}: {d:?}");
        }
        let d = t.record_exit(
            t0 + SEC * 3,
            ran,
            ExitCause::LimitRestart,
            RestartMode::OnCrash,
        );
        assert_eq!(d, Decision::GiveUp { exits_in_window: 4 });
    }

    #[test]
    fn crash_tracker_reset_clears_history() {
        let mut p = policy();
        p.max_restarts = 2;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        rec(&mut t, t0, SEC, crash());
        rec(&mut t, t0 + SEC, SEC, crash());
        assert_eq!(
            rec(&mut t, t0 + SEC * 2, SEC, crash()),
            Decision::GiveUp { exits_in_window: 3 }
        );
        t.reset();
        assert_eq!(t.attempt(), 0);
        let d = rec(&mut t, t0 + SEC * 3, SEC, crash());
        assert_eq!(
            d,
            Decision::Restart {
                delay: Duration::from_millis(500),
                attempt: 1
            }
        );
    }

    #[test]
    fn crash_tracker_zero_base_delay_stays_zero_and_relies_on_the_breaker() {
        let mut p = policy();
        p.restart_delay_ms = 0;
        p.max_restarts = 2;
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        assert_eq!(delay_of(rec(&mut t, t0, SEC, crash())), Duration::ZERO);
        assert_eq!(delay_of(rec(&mut t, t0, SEC, crash())), Duration::ZERO);
        assert!(matches!(
            rec(&mut t, t0, SEC, crash()),
            Decision::GiveUp { .. }
        ));
    }

    #[test]
    fn crash_tracker_delay_survives_huge_attempts_and_a_base_above_the_cap() {
        let mut p = policy();
        p.max_restarts = u32::MAX;
        p.restart_delay_ms = 90_000; // above max_restart_delay_ms: used as it is, never doubled
        let mut t = CrashTracker::new(p);
        let t0 = Instant::now();
        for i in 0..200u32 {
            let d = rec(&mut t, t0, Duration::ZERO, crash());
            assert_eq!(delay_of(d), Duration::from_millis(90_000), "attempt {i}");
        }
    }
}
