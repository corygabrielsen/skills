//! Bounded waits that end early on an observed change or a shutdown.
//!
//! # Invariants
//!
//! - **Deadline bound**: [`wait_until`] returns no later than the
//!   deadline plus one [`SLICE`], plus the duration of a probe
//!   already in flight, which is handed the remaining time as its
//!   budget.
//! - **Elapsed never probes**: a [`Wake::Elapsed`] wait consults
//!   only the clock and the interrupt predicate.
//! - **Interrupt outranks**: the predicate is checked between
//!   slices, handed to the probe to check between its requests, and
//!   checked after every probe, before its result is classified. A
//!   request already in flight delays it by at most that request's
//!   own deadline.
//! - **Failure degrades to sleeping**: after a failed or throttled
//!   probe the wait stops probing and sleeps to its deadline, then
//!   reports [`WaitEnd::CheckFailed`].

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::action::Wake;
use crate::rate_limit::RateLimitHit;
use crate::single_line_string::SingleLineString;

/// Granularity at which a wait observes the interrupt predicate.
pub const SLICE: Duration = Duration::from_millis(200);

/// One probe pass's verdict. The change source is opaque to this
/// crate: the probe names it, the recorder carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Unchanged,
    Changed(SingleLineString),
    Failed(SingleLineString),
    Throttled(RateLimitHit),
}

/// A cheap check for whether the state a wait observes has changed
/// since the probe's baseline.
pub trait ChangeProbe {
    /// Run one probe pass, finishing within `budget` and stopping early
    /// once `interrupted` holds.
    fn poll(&mut self, budget: Duration, interrupted: &dyn Fn() -> bool) -> ProbeResult;
}

/// A probe that never reports a change: a wait with it is a plain
/// interruptible sleep.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverChanges;

impl ChangeProbe for NeverChanges {
    fn poll(&mut self, _budget: Duration, _interrupted: &dyn Fn() -> bool) -> ProbeResult {
        ProbeResult::Unchanged
    }
}

/// Why a wait ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitEnd {
    /// The deadline passed.
    Elapsed,
    /// A probe reported a change at `source`.
    Changed { source: String },
    /// Probing failed; the wait slept to its deadline instead.
    CheckFailed { reason: String },
    /// The interrupt predicate fired.
    Interrupted,
}

/// Time source and sleeper, injected so waits are testable without
/// spending wall-clock time.
pub trait WaitClock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

/// The process clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl WaitClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Wait until `deadline`, ending early on interruption or — for
/// [`Wake::OnChange`] — on a change `probe` reports. Probe passes
/// start every `tick`, measured start to start, the first one `tick`
/// after the wait begins; a tick shorter than [`SLICE`] is raised to
/// it, so probing never spins.
pub fn wait_until(
    deadline: Instant,
    wake: Wake,
    tick: Duration,
    probe: &mut dyn ChangeProbe,
    interrupted: &dyn Fn() -> bool,
    clock: &dyn WaitClock,
) -> WaitEnd {
    let tick = tick.max(SLICE);
    let mut probing = wake == Wake::OnChange;
    let mut failure: Option<String> = None;
    let mut next_probe = clock.now() + tick;
    loop {
        if interrupted() {
            return WaitEnd::Interrupted;
        }
        let now = clock.now();
        if now >= deadline {
            return failure.map_or(WaitEnd::Elapsed, |reason| WaitEnd::CheckFailed { reason });
        }
        if probing && now >= next_probe {
            next_probe = now + tick;
            let result = probe.poll(deadline - now, interrupted);
            if interrupted() {
                return WaitEnd::Interrupted;
            }
            match result {
                ProbeResult::Unchanged => {}
                ProbeResult::Changed(source) => {
                    return WaitEnd::Changed {
                        source: source.as_str().to_string(),
                    };
                }
                ProbeResult::Failed(reason) => {
                    probing = false;
                    failure = Some(reason.as_str().to_string());
                }
                ProbeResult::Throttled(hit) => {
                    probing = false;
                    failure = Some(format!("throttled: {}", hit.scope.name()));
                }
            }
            continue;
        }
        let until = if probing {
            deadline.min(next_probe)
        } else {
            deadline
        };
        clock.sleep(SLICE.min(until.saturating_duration_since(now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PollingInterval;
    use crate::rate_limit::RateLimitScope;
    use std::cell::{Cell, RefCell};

    /// Virtual clock: `sleep` advances `now` without blocking.
    struct FakeClock {
        now: Cell<Instant>,
        slept: Cell<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Cell::new(Instant::now()),
                slept: Cell::new(Duration::ZERO),
            }
        }
    }

    impl WaitClock for FakeClock {
        fn now(&self) -> Instant {
            self.now.get()
        }

        fn sleep(&self, duration: Duration) {
            self.now.set(self.now.get() + duration);
            self.slept.set(self.slept.get() + duration);
        }
    }

    /// Replays scripted results, then reports `Unchanged`.
    struct Scripted {
        results: RefCell<Vec<ProbeResult>>,
        polls: Cell<u32>,
    }

    impl Scripted {
        fn new(mut results: Vec<ProbeResult>) -> Self {
            results.reverse();
            Self {
                results: RefCell::new(results),
                polls: Cell::new(0),
            }
        }
    }

    impl ChangeProbe for Scripted {
        fn poll(&mut self, _budget: Duration, _interrupted: &dyn Fn() -> bool) -> ProbeResult {
            self.polls.set(self.polls.get() + 1);
            self.results
                .borrow_mut()
                .pop()
                .unwrap_or(ProbeResult::Unchanged)
        }
    }

    const TICK: Duration = Duration::from_secs(5);
    const INTERVAL: Duration = Duration::from_mins(1);

    fn run(wake: Wake, probe: &mut Scripted, clock: &FakeClock) -> WaitEnd {
        wait_until(clock.now() + INTERVAL, wake, TICK, probe, &|| false, clock)
    }

    fn line(s: &str) -> SingleLineString {
        SingleLineString::new(s)
    }

    #[test]
    fn elapsed_wait_runs_to_its_deadline_without_probing() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![ProbeResult::Changed(line("reviews"))]);
        assert_eq!(run(Wake::Elapsed, &mut probe, &clock), WaitEnd::Elapsed);
        assert_eq!(probe.polls.get(), 0);
        assert_eq!(clock.slept.get(), INTERVAL);
    }

    #[test]
    fn unchanged_probe_runs_to_the_deadline_polling_each_tick() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![]);
        assert_eq!(run(Wake::OnChange, &mut probe, &clock), WaitEnd::Elapsed);
        assert_eq!(clock.slept.get(), INTERVAL);
        assert_eq!(probe.polls.get(), 11, "ticks at 5s..55s");
    }

    #[test]
    fn change_ends_the_wait_at_the_tick_that_saw_it() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![
            ProbeResult::Unchanged,
            ProbeResult::Changed(line("check-runs")),
        ]);
        assert_eq!(
            run(Wake::OnChange, &mut probe, &clock),
            WaitEnd::Changed {
                source: "check-runs".into()
            }
        );
        assert_eq!(clock.slept.get(), 2 * TICK);
    }

    #[test]
    fn failed_probe_degrades_to_sleeping_out_the_deadline() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![
            ProbeResult::Failed(line("HTTP 500")),
            ProbeResult::Changed(line("never consulted")),
        ]);
        assert_eq!(
            run(Wake::OnChange, &mut probe, &clock),
            WaitEnd::CheckFailed {
                reason: "HTTP 500".into()
            }
        );
        assert_eq!(probe.polls.get(), 1);
        assert_eq!(clock.slept.get(), INTERVAL);
    }

    #[test]
    fn throttled_probe_stops_probing_and_names_the_scope() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![ProbeResult::Throttled(RateLimitHit {
            scope: RateLimitScope::GitHubSecondary,
            retry_after: PollingInterval::from_secs(60),
        })]);
        let end = run(Wake::OnChange, &mut probe, &clock);
        assert_eq!(
            end,
            WaitEnd::CheckFailed {
                reason: format!("throttled: {}", RateLimitScope::GitHubSecondary.name())
            }
        );
        assert_eq!(probe.polls.get(), 1);
    }

    #[test]
    fn interrupt_ends_the_wait_within_one_slice() {
        let clock = FakeClock::new();
        let fired_at = clock.now() + Duration::from_secs(12);
        let mut probe = NeverChanges;
        let end = wait_until(
            clock.now() + INTERVAL,
            Wake::Elapsed,
            TICK,
            &mut probe,
            &|| clock.now() >= fired_at,
            &clock,
        );
        assert_eq!(end, WaitEnd::Interrupted);
        assert!(clock.slept.get() <= Duration::from_secs(12) + SLICE);
    }

    #[test]
    fn interrupt_between_probe_ticks_ends_an_observing_wait() {
        let clock = FakeClock::new();
        let fired_at = clock.now() + Duration::from_secs(7);
        let mut probe = Scripted::new(vec![]);
        let end = wait_until(
            clock.now() + INTERVAL,
            Wake::OnChange,
            TICK,
            &mut probe,
            &|| clock.now() >= fired_at,
            &clock,
        );
        assert_eq!(end, WaitEnd::Interrupted);
        assert!(clock.slept.get() <= Duration::from_secs(7) + SLICE);
        assert_eq!(
            probe.polls.get(),
            1,
            "the 5s tick ran; the 10s one never did"
        );
    }

    #[test]
    fn a_zero_tick_is_raised_to_one_slice() {
        let clock = FakeClock::new();
        let mut probe = Scripted::new(vec![]);
        wait_until(
            clock.now() + Duration::from_secs(1),
            Wake::OnChange,
            Duration::ZERO,
            &mut probe,
            &|| false,
            &clock,
        );
        assert_eq!(probe.polls.get(), 4, "one poll per 200ms slice before 1s");
    }

    #[test]
    fn interrupt_outranks_a_change_reported_by_the_same_probe_pass() {
        struct ChangeAndInterrupt<'a>(&'a Cell<bool>);
        impl ChangeProbe for ChangeAndInterrupt<'_> {
            fn poll(&mut self, _budget: Duration, _interrupted: &dyn Fn() -> bool) -> ProbeResult {
                self.0.set(true);
                ProbeResult::Changed(SingleLineString::new("reviews"))
            }
        }
        let clock = FakeClock::new();
        let interrupted = Cell::new(false);
        let end = wait_until(
            clock.now() + INTERVAL,
            Wake::OnChange,
            TICK,
            &mut ChangeAndInterrupt(&interrupted),
            &|| interrupted.get(),
            &clock,
        );
        assert_eq!(end, WaitEnd::Interrupted);
    }

    #[test]
    fn probe_budget_is_the_time_left_before_the_deadline() {
        struct Budgets(Vec<Duration>);
        impl ChangeProbe for Budgets {
            fn poll(&mut self, budget: Duration, _interrupted: &dyn Fn() -> bool) -> ProbeResult {
                self.0.push(budget);
                ProbeResult::Unchanged
            }
        }
        let clock = FakeClock::new();
        let mut probe = Budgets(Vec::new());
        wait_until(
            clock.now() + Duration::from_secs(12),
            Wake::OnChange,
            TICK,
            &mut probe,
            &|| false,
            &clock,
        );
        assert_eq!(
            probe.0,
            vec![Duration::from_secs(7), Duration::from_secs(2)]
        );
    }

    #[test]
    fn wait_end_serializes_with_a_kind_tag() {
        let json = serde_json::to_string(&WaitEnd::Changed {
            source: "reviews".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"changed","source":"reviews"}"#);
        let back: WaitEnd = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back,
            WaitEnd::Changed {
                source: "reviews".into()
            }
        );
    }
}
