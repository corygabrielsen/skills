//! Decide stage: pure state machine over [`OrientedState`].
//!
//! Phase 6 covers the in-batch decisions only:
//!
//! ```text
//! BatchState::NotStarted                  → Execute(RunReviews)
//! BatchState::Running { c < expected }    → Execute(AwaitReviews)
//! BatchState::Complete { all_clean }      → Halt(AgentNeeded(Retrospective))
//! BatchState::Complete { has_issues }     → Halt(AgentNeeded(AddressBatch))
//! ```
//!
//! Cross-iteration transitions — `AdvanceLevel`, `DropLevel`,
//! `RestartFromFloor`, `RunTests` — fire from recorder-derived
//! state and land in Phase 6b once the recorder is wired.

pub mod action;
pub mod decision;

use crate::ids::BlockerKey;
use crate::observe::codex::VerdictClass;
use crate::observe::codex::batch::{BatchState, VerdictRecord};
use crate::orient::OrientedState;

use action::{Action, ActionEffect, ActionKind, ReasoningLevel, TargetEffect, Urgency};
use decision::{Decision, DecisionHalt, Terminal};

/// Default polling cadence for `AwaitReviews`. The runner sleeps
/// this long between observations while a batch is in flight.
/// Matches the `S=30` reference in loop-codex-review's polling
/// one-liner.
///
/// Tests and unusual deployments override via the
/// `OODA_AWAIT_SECS` env var. Must be ≥ 1; a value of 0 (or any
/// unparseable string) falls back to [`DEFAULT_AWAIT_SECS`] —
/// [`ooda_core::PollingInterval`] enforces the no-busy-loop
/// invariant structurally.
const DEFAULT_AWAIT_SECS: u64 = 30;

fn await_interval() -> ooda_core::PollingInterval {
    let secs = std::env::var("OODA_AWAIT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_AWAIT_SECS);
    ooda_core::PollingInterval::from_secs(secs)
}

pub fn decide(oriented: &OrientedState) -> Decision {
    let action = match &oriented.batch_state {
        BatchState::NotStarted => mk_run_reviews(oriented.current_level, oriented.expected),
        BatchState::Running { completed, .. } => {
            let pending = oriented.expected.saturating_sub(*completed);
            mk_await_reviews(oriented.current_level, pending)
        }
        BatchState::Complete { verdicts } if all_clean(verdicts) => {
            // At ceiling + all clean: terminal fixed point. The
            // orchestrator may still want to dispatch a final
            // retrospective, but the binary's job is done; signal
            // it via the Terminal halt rather than a Retrospective
            // handoff. Below ceiling: hand off to retrospective
            // synthesis as before.
            if oriented.current_level == oriented.ceiling {
                return Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded));
            }
            mk_retrospective(oriented.current_level)
        }
        BatchState::Complete { verdicts } => {
            let has_issues_count = verdicts
                .iter()
                .filter(|v| matches!(v.class, VerdictClass::HasIssues))
                .count() as u32;
            mk_address_batch(oriented.current_level, has_issues_count)
        }
    };
    classify(action)
}

fn classify(action: Action) -> Decision {
    // Match on a borrow of `effect` so we can move `action` into the
    // resulting variant. The four `ActionEffect` variants partition
    // the action space into "loop drives it" (Full/Wait → Execute)
    // and "external resolver needed" (Agent/Human → Halt).
    match &action.effect {
        ActionEffect::Full { .. } | ActionEffect::Wait { .. } => Decision::Execute(action),
        ActionEffect::Agent { .. } => Decision::Halt(DecisionHalt::AgentNeeded(action)),
        ActionEffect::Human { .. } => Decision::Halt(DecisionHalt::HumanNeeded(action)),
    }
}

fn all_clean(verdicts: &[VerdictRecord]) -> bool {
    verdicts
        .iter()
        .all(|v| matches!(v.class, VerdictClass::Clean))
}

fn mk_run_reviews(level: ReasoningLevel, n: u32) -> Action {
    Action {
        kind: ActionKind::RunReviews { level, n },
        effect: ActionEffect::Full {
            log: format!(
                "Spawn {n} `codex review` subprocesses at reasoning level {}.",
                level.as_str()
            ),
        },
        target_effect: TargetEffect::Advances,
        urgency: Urgency::Critical,
        blocker: BlockerKey::tag(format!("runreviews:{}", level.as_str())),
    }
}

fn mk_await_reviews(level: ReasoningLevel, pending: u32) -> Action {
    Action {
        kind: ActionKind::AwaitReviews { level, pending },
        effect: ActionEffect::Wait {
            interval: await_interval(),
            log: format!(
                "Polling: {pending} review(s) still streaming at level {}.",
                level.as_str()
            ),
        },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::BlockingWait,
        blocker: BlockerKey::tag(format!("await:{}", level.as_str())),
    }
}

fn mk_address_batch(level: ReasoningLevel, issue_count: u32) -> Action {
    Action {
        kind: ActionKind::AddressBatch { issue_count, level },
        effect: ActionEffect::Agent {
            prompt: ooda_core::HandoffPrompt::new(format!(
                "Verify and address {issue_count} review(s) with issues at level {}. \
                 For each issue: real bug → fix; false positive → clarify code; \
                 design tradeoff → document rationale. Then run tests.",
                level.as_str()
            )),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingFix,
        blocker: BlockerKey::tag(format!("address:{}", level.as_str())),
    }
}

fn mk_retrospective(level: ReasoningLevel) -> Action {
    Action {
        kind: ActionKind::Retrospective { level },
        effect: ActionEffect::Agent {
            prompt: ooda_core::HandoffPrompt::new(format!(
                "All reviews clean at level {}. Synthesize the issue history \
                 so far. Look for architectural patterns that would prevent \
                 3+ issues each. If patterns exist: implement and the loop \
                 restarts from the floor. If not: the loop advances.",
                level.as_str()
            )),
        },
        target_effect: TargetEffect::Advances,
        urgency: Urgency::BlockingFix,
        blocker: BlockerKey::tag(format!("retro:{}", level.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::codex::VerdictClass;

    fn oriented(batch_state: BatchState, level: ReasoningLevel, expected: u32) -> OrientedState {
        OrientedState {
            current_level: level,
            ceiling: ReasoningLevel::Xhigh,
            batch_state,
            expected,
        }
    }

    fn oriented_with_ceiling(
        batch_state: BatchState,
        level: ReasoningLevel,
        ceiling: ReasoningLevel,
        expected: u32,
    ) -> OrientedState {
        OrientedState {
            current_level: level,
            ceiling,
            batch_state,
            expected,
        }
    }

    fn record(slot: u32, class: VerdictClass) -> VerdictRecord {
        VerdictRecord {
            slot,
            body: "stub".to_string(),
            class,
        }
    }

    #[test]
    fn not_started_runs_reviews() {
        let o = oriented(BatchState::NotStarted, ReasoningLevel::Low, 3);
        let d = decide(&o);
        match d {
            Decision::Execute(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::RunReviews {
                        level: ReasoningLevel::Low,
                        n: 3
                    }
                ));
                assert!(matches!(action.effect, ActionEffect::Full { .. }));
                assert_eq!(action.urgency, Urgency::Critical);
            }
            other => panic!("expected Execute(RunReviews), got {other:?}"),
        }
    }

    #[test]
    fn running_emits_await_with_pending_count() {
        let bs = BatchState::Running {
            total: 3,
            completed: 1,
        };
        let o = oriented(bs, ReasoningLevel::Medium, 3);
        let d = decide(&o);
        match d {
            Decision::Execute(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::AwaitReviews {
                        level: ReasoningLevel::Medium,
                        pending: 2
                    }
                ));
                assert!(matches!(action.effect, ActionEffect::Wait { .. }));
            }
            other => panic!("expected Execute(AwaitReviews), got {other:?}"),
        }
    }

    #[test]
    fn complete_all_clean_below_ceiling_halts_for_retrospective() {
        let bs = BatchState::Complete {
            verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::Clean),
            ],
        };
        // current=High, ceiling=Xhigh — below ceiling → Retrospective.
        let o = oriented(bs, ReasoningLevel::High, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::Retrospective {
                        level: ReasoningLevel::High
                    }
                ));
            }
            other => panic!("expected Halt(AgentNeeded(Retrospective)), got {other:?}"),
        }
    }

    #[test]
    fn complete_all_clean_at_ceiling_halts_terminal_fixed_point() {
        let bs = BatchState::Complete {
            verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::Clean),
            ],
        };
        let o = oriented(bs, ReasoningLevel::Xhigh, 3); // ceiling = Xhigh by default
        let d = decide(&o);
        assert!(matches!(
            d,
            Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded))
        ));
    }

    #[test]
    fn ceiling_is_configurable_below_xhigh() {
        // Caller pinned ceiling = High. Reaching all-clean at high
        // should be terminal even though Xhigh exists on the ladder.
        let bs = BatchState::Complete {
            verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::Clean),
            ],
        };
        let o = oriented_with_ceiling(bs, ReasoningLevel::High, ReasoningLevel::High, 3);
        let d = decide(&o);
        assert!(matches!(
            d,
            Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded))
        ));
    }

    #[test]
    fn complete_with_issues_halts_for_address_batch() {
        let bs = BatchState::Complete {
            verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::HasIssues),
                record(3, VerdictClass::HasIssues),
            ],
        };
        let o = oriented(bs, ReasoningLevel::Xhigh, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => match action.kind {
                ActionKind::AddressBatch { issue_count, level } => {
                    assert_eq!(issue_count, 2, "only HasIssues verdicts count");
                    assert_eq!(level, ReasoningLevel::Xhigh);
                }
                other => panic!("expected AddressBatch, got {other:?}"),
            },
            other => panic!("expected Halt(AgentNeeded(AddressBatch)), got {other:?}"),
        }
    }

    #[test]
    fn single_has_issues_still_routes_to_address() {
        let bs = BatchState::Complete {
            verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::HasIssues),
            ],
        };
        let o = oriented(bs, ReasoningLevel::Low, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => {
                assert!(matches!(action.kind, ActionKind::AddressBatch { .. }));
            }
            other => panic!("expected AddressBatch, got {other:?}"),
        }
    }

    #[test]
    fn pending_clamps_at_zero_when_completed_exceeds_expected() {
        let bs = BatchState::Running {
            total: 5,
            completed: 5,
        };
        let o = oriented(bs, ReasoningLevel::Low, 3);
        let d = decide(&o);
        // saturating_sub means pending = 0 here; runner re-observes
        // and the next pass will likely transition to Complete.
        match d {
            Decision::Execute(action) => match action.kind {
                ActionKind::AwaitReviews { pending, .. } => assert_eq!(pending, 0),
                other => panic!("expected AwaitReviews, got {other:?}"),
            },
            other => panic!("expected Execute, got {other:?}"),
        }
    }

    #[test]
    fn blocker_keys_are_level_scoped() {
        let low = mk_run_reviews(ReasoningLevel::Low, 3);
        let high = mk_run_reviews(ReasoningLevel::High, 3);
        assert_ne!(low.blocker, high.blocker);
        // Same level → identical blocker (this is what the runner's
        // stall detection compares against).
        let low_again = mk_run_reviews(ReasoningLevel::Low, 5);
        assert_eq!(low.blocker, low_again.blocker);
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // Class invariant for `decide`: every `BatchState` variant has a
    // deterministic outcome in the baseline state (below ceiling,
    // all-clean verdicts in Complete). The exhaustive match in
    // `expected_decide_behavior` is the contract. Adding a new
    // `BatchState` variant fails to compile here until the new arm
    // is added — which forces a decision about which Action the new
    // state maps to.
    //
    // Sub-cases for Complete (at-ceiling → Terminal::Succeeded,
    // has-issues → AddressBatch) are exercised by the scenario
    // tests above; this property test pins the per-variant baseline.

    #[derive(Debug, PartialEq, Eq)]
    enum DecideBaselineBehavior {
        /// `Execute(RunReviews)` — the loop spawns the batch.
        ExecuteRunReviews,
        /// `Execute(AwaitReviews)` — the loop polls in-flight reviews.
        ExecuteAwaitReviews,
        /// `Halt(AgentNeeded(Retrospective))` — below ceiling with
        /// all verdicts clean: hand off to retrospective synthesis.
        HaltRetrospective,
    }

    fn expected_decide_behavior(batch_state: &BatchState) -> DecideBaselineBehavior {
        match batch_state {
            BatchState::NotStarted => DecideBaselineBehavior::ExecuteRunReviews,
            BatchState::Running { .. } => DecideBaselineBehavior::ExecuteAwaitReviews,
            // Baseline: all-clean verdicts. The has-issues sub-case
            // is exercised by `complete_with_issues_halts_for_address_batch`.
            BatchState::Complete { .. } => DecideBaselineBehavior::HaltRetrospective,
        }
    }

    fn all_batch_states() -> Vec<BatchState> {
        vec![
            BatchState::NotStarted,
            BatchState::Running {
                total: 3,
                completed: 1,
            },
            BatchState::Complete {
                verdicts: vec![
                    record(1, VerdictClass::Clean),
                    record(2, VerdictClass::Clean),
                    record(3, VerdictClass::Clean),
                ],
            },
        ]
    }

    fn observed_decide_behavior(d: &Decision) -> DecideBaselineBehavior {
        match d {
            Decision::Execute(action) => match &action.kind {
                ActionKind::RunReviews { .. } => DecideBaselineBehavior::ExecuteRunReviews,
                ActionKind::AwaitReviews { .. } => DecideBaselineBehavior::ExecuteAwaitReviews,
                other => {
                    panic!("decide emitted unexpected Execute kind in baseline: {other:?}")
                }
            },
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => match &action.kind {
                ActionKind::Retrospective { .. } => DecideBaselineBehavior::HaltRetrospective,
                other => {
                    panic!("decide emitted unexpected AgentNeeded kind in baseline: {other:?}",)
                }
            },
            other => panic!("decide emitted unexpected Decision in baseline: {other:?}"),
        }
    }

    #[test]
    fn decide_property_holds_for_every_batch_state() {
        let states = all_batch_states();
        assert_eq!(
            states.len(),
            3,
            "`all_batch_states` must include one sample per \
             `BatchState` variant; adding a new variant requires \
             adding both an arm in `expected_decide_behavior` AND a \
             sample here.",
        );
        for batch_state in states {
            // Below ceiling so the Complete-all-clean case lands on
            // Retrospective, not Terminal::Succeeded.
            let o = oriented_with_ceiling(
                batch_state.clone(),
                ReasoningLevel::Low,
                ReasoningLevel::Xhigh,
                3,
            );
            let actual = observed_decide_behavior(&decide(&o));
            let expected = expected_decide_behavior(&batch_state);
            assert_eq!(
                actual, expected,
                "decide contract violated for batch_state = {batch_state:?}",
            );
        }
    }
}
