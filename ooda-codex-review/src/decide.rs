//! Decide stage: pure state machine over [`OrientedState`].
//!
//! In-batch transition table:
//!
//! ```text
//! NotStarted                            → Execute(RunReviews)
//! Running { completed < expected }      → Execute(AwaitReviews)
//! Complete { all_clean, at_ceiling }    → Halt(Terminal(Succeeded))
//! Complete { all_clean, below_ceiling } → Halt(AgentNeeded(Retrospective))
//! Complete { has_issues }               → Halt(AgentNeeded(AddressBatch))
//! ```
//!
//! Cross-iteration ladder transitions are recorder-driven and do
//! not appear in this table; they reach decide via the recorder's
//! mutation of `current_level` between iterations.

use ooda_core::MidTier;

pub(crate) mod action;
pub(crate) mod decision;

use crate::ids::BlockerKey;
use std::time::{Duration, SystemTime};

use crate::observe::codex::VerdictClass;
use crate::observe::codex::batch::{ALIVE_THRESHOLD, BatchState, VerdictRecord};
use crate::orient::OrientedState;

use action::{Action, ActionEffect, ActionKind, CodexReasoningLevel, TargetEffect, Urgency};
use decision::{Decision, DecisionHalt, Terminal};

/// Default polling cadence for in-flight review batches.
///
/// Override via the `OODA_AWAIT_SECS` env var; values that fail to
/// parse to a positive integer fall back to this default. The
/// no-busy-loop invariant is established structurally by
/// [`ooda_core::PollingInterval`] regardless of the source.
const DEFAULT_AWAIT_SECS: u64 = 30;

fn await_interval() -> ooda_core::PollingInterval {
    let secs = std::env::var("OODA_AWAIT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_AWAIT_SECS);
    ooda_core::PollingInterval::from_secs(secs)
}

pub(crate) fn decide(oriented: &OrientedState) -> Decision {
    // Running state goes through an alive/idle discriminator:
    // any pending slot whose log mtime is within the
    // [`ALIVE_THRESHOLD`] window is genuinely streaming, so we
    // emit an `AwaitReviews` Wait and keep polling. Only when
    // EVERY pending slot has been idle past the threshold does
    // the discriminator promote the state to a synthetic
    // Complete with [`VerdictClass::Abandoned`] verdicts and
    // fall through to the normal terminal fork. This catches
    // the "slow but progressing" tail (a slot still logging at
    // 30+ min wall time on xhigh) without ever silently
    // claiming fixed point on a partial sample.
    let synthesized = match &oriented.batch_state {
        BatchState::Running {
            pending_slots,
            completed_verdicts: _,
        } => {
            let now = SystemTime::now();
            let any_alive = pending_slots
                .iter()
                .any(|s| is_alive(s.log_mtime, now, ALIVE_THRESHOLD));
            if any_alive {
                None
            } else {
                oriented
                    .batch_state
                    .project_abandoning_pending("all pending slots idle past alive threshold")
            }
        }
        _ => None,
    };
    let batch_state = synthesized.as_ref().unwrap_or(&oriented.batch_state);

    let action = match batch_state {
        BatchState::NotStarted => mk_run_reviews(oriented.current_level, oriented.expected),
        BatchState::Running {
            completed_verdicts, ..
        } => {
            let completed =
                u32::try_from(completed_verdicts.len()).expect("completed slot count fits in u32");
            let pending = oriented.expected.saturating_sub(completed);
            mk_await_reviews(oriented.current_level, pending)
        }
        BatchState::InconsistentState { reason, .. } => {
            mk_batch_state_inconsistent(oriented.current_level, reason)
        }
        BatchState::Complete { verdicts } if all_clean(verdicts) => {
            // All-clean at ceiling is the terminal fixed point.
            // All-clean below ceiling defers to retrospective
            // synthesis which decides whether to advance.
            if oriented.current_level == oriented.ceiling {
                return Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded));
            }
            mk_retrospective(oriented.current_level)
        }
        BatchState::Complete { verdicts } => {
            // Bounded by per-batch reviewer fan-out; fits in u32.
            // `Abandoned` is counted alongside `HasIssues` /
            // `Indeterminate` so a partial sample with abandoned
            // pending slots never silently claims fixed point.
            let has_issues_count = u32::try_from(
                verdicts
                    .iter()
                    .filter(|v| {
                        matches!(
                            v.class,
                            VerdictClass::HasIssues
                                | VerdictClass::Indeterminate
                                | VerdictClass::Abandoned
                        )
                    })
                    .count(),
            )
            .expect("verdict count fits in u32");
            mk_address_batch(oriented.current_level, has_issues_count)
        }
    };
    ooda_core::classify(action)
}

/// `true` iff `mtime` is within `threshold` of `now` — the slot has
/// produced log output recently enough that the codex subprocess
/// is presumed to still be making forward progress.
fn is_alive(mtime: SystemTime, now: SystemTime, threshold: Duration) -> bool {
    now.duration_since(mtime).is_ok_and(|d| d <= threshold)
}

fn all_clean(verdicts: &[VerdictRecord]) -> bool {
    verdicts
        .iter()
        .all(|v| matches!(v.class, VerdictClass::Clean))
}

fn mk_run_reviews(level: CodexReasoningLevel, n: u32) -> Action {
    Action {
        kind: ActionKind::RunReviews { level, n },
        effect: ActionEffect::Full {
            log: format!(
                "Spawn {n} `codex review` subprocesses at reasoning level {}.",
                level.as_str()
            ),
            // Codex review children are spawned detached; the act
            // call returns once the processes are launched. The
            // children's completion is observed via per-slot log
            // and exit files that only appear after each subprocess
            // finishes. The next observe pass may still see no
            // completion until the workers land their outputs.
            //
            // 60s gives slot-0 a realistic chance to surface its
            // exit file at typical codex reasoning latencies;
            // longer ladders propagate over multiple iterations via
            // the iteration cap.
            upstream: ooda_core::UpstreamConsistency::Eventual(
                ooda_core::PollingInterval::from_secs(60),
            ),
        },
        target_effect: TargetEffect::Advances,
        urgency: Urgency::Mid(MidTier::Critical),
        blocker: BlockerKey::typed("runreviews", &level),
    }
}

fn mk_await_reviews(level: CodexReasoningLevel, pending: u32) -> Action {
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
        urgency: Urgency::Mid(MidTier::BlockingWait),
        blocker: BlockerKey::typed("await", &level),
    }
}

fn mk_address_batch(level: CodexReasoningLevel, issue_count: u32) -> Action {
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
        urgency: Urgency::Mid(MidTier::BlockingFix),
        blocker: BlockerKey::typed("address", &level),
    }
}

fn mk_batch_state_inconsistent(level: CodexReasoningLevel, reason: &str) -> Action {
    Action {
        kind: ActionKind::BatchStateInconsistent {
            level,
            reason: reason.to_string(),
        },
        effect: ActionEffect::Human {
            prompt: ooda_core::HandoffPrompt::new(format!(
                "Stale codex-review batch state at level {}: {reason}. \
                 The auto-loop has no safe recovery: inspect the batch \
                 directory, clear stray log/exit files (or remove \
                 `head_sha.txt` to force a fresh batch), then re-run.",
                level.as_str(),
            )),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::Pathology),
        blocker: BlockerKey::typed("inconsistent", &level),
    }
}

fn mk_retrospective(level: CodexReasoningLevel) -> Action {
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
        urgency: Urgency::Mid(MidTier::BlockingFix),
        blocker: BlockerKey::typed("retro", &level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::codex::VerdictClass;
    use crate::observe::codex::batch::PendingSlot;
    use ooda_core::MidTier;

    fn oriented(
        batch_state: BatchState,
        level: CodexReasoningLevel,
        expected: u32,
    ) -> OrientedState {
        OrientedState {
            current_level: level,
            ceiling: CodexReasoningLevel::Xhigh,
            batch_state,
            expected,
        }
    }

    fn oriented_with_ceiling(
        batch_state: BatchState,
        level: CodexReasoningLevel,
        ceiling: CodexReasoningLevel,
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
        let o = oriented(BatchState::NotStarted, CodexReasoningLevel::Low, 3);
        let d = decide(&o);
        match d {
            Decision::Execute(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::RunReviews {
                        level: CodexReasoningLevel::Low,
                        n: 3
                    }
                ));
                assert!(matches!(action.effect, ActionEffect::Full { .. }));
                assert_eq!(action.urgency, Urgency::Mid(MidTier::Critical));
            }
            other @ Decision::Halt(_) => panic!("expected Execute(RunReviews), got {other:?}"),
        }
    }

    #[test]
    fn running_emits_await_with_pending_count() {
        let bs = BatchState::running_alive(1, 3);
        let o = oriented(bs, CodexReasoningLevel::Medium, 3);
        let d = decide(&o);
        match d {
            Decision::Execute(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::AwaitReviews {
                        level: CodexReasoningLevel::Medium,
                        pending: 2
                    }
                ));
                assert!(matches!(action.effect, ActionEffect::Wait { .. }));
            }
            other @ Decision::Halt(_) => panic!("expected Execute(AwaitReviews), got {other:?}"),
        }
    }

    #[test]
    fn running_with_idle_pending_projects_to_complete_with_abandoned_verdicts() {
        // Peer-reported StuckCapReached bug: 4 of 5 slots had clean
        // verdicts but were thrown away because the parent gave up
        // while the 5th was "still streaming." Pre-fix, a Running
        // state with one pending slot whose log_mtime had advanced
        // 30+ minutes earlier kept emitting AwaitReviews forever
        // until the iter cap tripped, then the runner dropped the
        // 4 clean verdicts on the floor.
        //
        // Now: when every pending slot has been idle past the
        // ALIVE_THRESHOLD, the discriminator projects to a synthetic
        // Complete with the completed verdicts + Abandoned verdicts
        // for the pending slots. The normal terminal fork takes it:
        // Abandoned counts as not-clean, so the loop routes to
        // AddressBatch — surfacing the 4 clean reviews + the 1
        // abandoned slot to the orchestrator instead of dropping
        // everything.
        let idle = SystemTime::now() - Duration::from_mins(10);
        let bs = BatchState::Running {
            pending_slots: vec![PendingSlot {
                slot: 5,
                log_mtime: idle,
                log_bytes: 2_800_000,
            }],
            completed_verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::Clean),
                record(4, VerdictClass::Clean),
            ],
        };
        let o = oriented(bs, CodexReasoningLevel::Xhigh, 5);
        let d = decide(&o);
        // 4 Clean + 1 Abandoned → not all clean → AddressBatch.
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(handoff)) => {
                assert!(matches!(
                    handoff.kind,
                    ActionKind::AddressBatch {
                        level: CodexReasoningLevel::Xhigh,
                        ..
                    },
                ));
            }
            other => panic!("expected AddressBatch handoff, got {other:?}"),
        }
    }

    #[test]
    fn running_with_alive_pending_continues_polling() {
        // Mirror of the failure case: when at least one pending
        // slot has produced log output within the alive window,
        // the discriminator preserves the Running state and emits
        // AwaitReviews — the slow-but-progressing tail must NOT be
        // mistaken for a hung slot. Pre-fix, the loop cap was the
        // only termination signal and it tripped on healthy long
        // runs.
        let alive = SystemTime::now() - Duration::from_secs(30); // 30s idle, under threshold
        let bs = BatchState::Running {
            pending_slots: vec![PendingSlot {
                slot: 5,
                log_mtime: alive,
                log_bytes: 2_800_000,
            }],
            completed_verdicts: vec![
                record(1, VerdictClass::Clean),
                record(2, VerdictClass::Clean),
                record(3, VerdictClass::Clean),
                record(4, VerdictClass::Clean),
            ],
        };
        let o = oriented(bs, CodexReasoningLevel::Xhigh, 5);
        let d = decide(&o);
        match d {
            Decision::Execute(action) => assert!(matches!(
                action.kind,
                ActionKind::AwaitReviews {
                    level: CodexReasoningLevel::Xhigh,
                    pending: 1,
                }
            )),
            other @ Decision::Halt(_) => panic!("expected AwaitReviews, got {other:?}"),
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
        let o = oriented(bs, CodexReasoningLevel::High, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::Retrospective {
                        level: CodexReasoningLevel::High
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
        let o = oriented(bs, CodexReasoningLevel::Xhigh, 3); // ceiling = Xhigh by default
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
        let o = oriented_with_ceiling(bs, CodexReasoningLevel::High, CodexReasoningLevel::High, 3);
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
        let o = oriented(bs, CodexReasoningLevel::Xhigh, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => match action.kind {
                ActionKind::AddressBatch { issue_count, level } => {
                    assert_eq!(issue_count, 2, "only HasIssues verdicts count");
                    assert_eq!(level, CodexReasoningLevel::Xhigh);
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
        let o = oriented(bs, CodexReasoningLevel::Low, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(action)) => {
                assert!(matches!(action.kind, ActionKind::AddressBatch { .. }));
            }
            other => panic!("expected AddressBatch, got {other:?}"),
        }
    }

    #[test]
    fn inconsistent_state_halts_for_human_resolution() {
        // Stale state: more completed slots than expected. The
        // legacy projection (Running { completed > expected })
        // would emit AwaitReviews { pending: 0 } — a Wait the
        // loop honours forever. The new variant routes the same
        // input to a HumanNeeded handoff carrying the reason.
        let bs = BatchState::InconsistentState {
            total: 4,
            completed: 4,
            expected: 3,
            reason: "stray log file from prior batch".to_string(),
        };
        let o = oriented(bs, CodexReasoningLevel::High, 3);
        let d = decide(&o);
        match d {
            Decision::Halt(DecisionHalt::HumanNeeded(action)) => match action.kind {
                ActionKind::BatchStateInconsistent { level, reason } => {
                    assert_eq!(level, CodexReasoningLevel::High);
                    assert!(reason.contains("stray log"), "reason: {reason}");
                }
                other => panic!("expected BatchStateInconsistent, got {other:?}"),
            },
            other => panic!("expected Halt(HumanNeeded), got {other:?}"),
        }
    }

    #[test]
    fn running_with_no_alive_pending_promotes_to_complete_via_discriminator() {
        // A `Running` state whose pending slots have all been idle
        // past [`ALIVE_THRESHOLD`] is projected to a synthetic
        // `Complete` (with abandoned pending verdicts). With zero
        // pending slots — the boundary case — the discriminator
        // simply takes the existing completed verdicts and runs
        // the normal terminal fork. This replaces the old
        // pre-discriminator behavior where `Running { 5/5 }` (a
        // state `scan_batch` cannot produce, but which a synthetic
        // construction could) emitted a degenerate
        // `AwaitReviews { pending: 0 }` Wait the loop honored
        // forever.
        let bs = BatchState::running_alive(5, 5);
        let o = oriented(bs, CodexReasoningLevel::Low, 5);
        let d = decide(&o);
        // 5 Clean verdicts below ceiling → retrospective handoff.
        match d {
            Decision::Halt(DecisionHalt::AgentNeeded(handoff)) => assert!(matches!(
                handoff.kind,
                ActionKind::Retrospective {
                    level: CodexReasoningLevel::Low,
                },
            )),
            other => panic!("expected Halt(AgentNeeded(Retrospective)), got {other:?}"),
        }
    }

    #[test]
    fn blocker_keys_are_level_scoped() {
        let low = mk_run_reviews(CodexReasoningLevel::Low, 3);
        let high = mk_run_reviews(CodexReasoningLevel::High, 3);
        assert_ne!(low.blocker, high.blocker);
        // Same level → identical blocker (this is what the runner's
        // stall detection compares against).
        let low_again = mk_run_reviews(CodexReasoningLevel::Low, 5);
        assert_eq!(low.blocker, low_again.blocker);
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // Class invariant: every `BatchState` variant maps to a
    // deterministic baseline behaviour (below ceiling, all-clean
    // for Complete). The exhaustive match in
    // `expected_decide_behavior` is the contract — adding a new
    // variant fails to compile here until the new arm is added,
    // forcing a decision about which Action the new state maps to.
    //
    // Complete sub-cases (at-ceiling Terminal, has-issues
    // AddressBatch) are covered by the scenario tests above.

    #[derive(Debug, PartialEq, Eq)]
    enum DecideBaselineBehavior {
        /// `Execute(RunReviews)` — the loop spawns the batch.
        ExecuteRunReviews,
        /// `Execute(AwaitReviews)` — the loop polls in-flight reviews.
        ExecuteAwaitReviews,
        /// `Halt(AgentNeeded(Retrospective))` — below ceiling with
        /// all verdicts clean: hand off to retrospective synthesis.
        HaltRetrospective,
        /// `Halt(HumanNeeded(BatchStateInconsistent))` — stale
        /// state observed; no safe auto-recovery.
        HaltBatchStateInconsistent,
    }

    fn expected_decide_behavior(batch_state: &BatchState) -> DecideBaselineBehavior {
        match batch_state {
            BatchState::NotStarted => DecideBaselineBehavior::ExecuteRunReviews,
            BatchState::Running { .. } => DecideBaselineBehavior::ExecuteAwaitReviews,
            BatchState::InconsistentState { .. } => {
                DecideBaselineBehavior::HaltBatchStateInconsistent
            }
            // Baseline: all-clean verdicts. The has-issues sub-case
            // is exercised by `complete_with_issues_halts_for_address_batch`.
            BatchState::Complete { .. } => DecideBaselineBehavior::HaltRetrospective,
        }
    }

    fn all_batch_states() -> Vec<BatchState> {
        vec![
            BatchState::NotStarted,
            BatchState::running_alive(1, 3),
            BatchState::InconsistentState {
                total: 4,
                completed: 4,
                expected: 3,
                reason: "stray log".to_string(),
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
                    panic!("decide emitted unexpected AgentNeeded kind in baseline: {other:?}")
                }
            },
            Decision::Halt(DecisionHalt::HumanNeeded(action)) => match &action.kind {
                ActionKind::BatchStateInconsistent { .. } => {
                    DecideBaselineBehavior::HaltBatchStateInconsistent
                }
                other => {
                    panic!("decide emitted unexpected HumanNeeded kind in baseline: {other:?}")
                }
            },
            other @ Decision::Halt(_) => {
                panic!("decide emitted unexpected Decision in baseline: {other:?}")
            }
        }
    }

    #[test]
    fn decide_property_holds_for_every_batch_state() {
        let states = all_batch_states();
        assert_eq!(
            states.len(),
            4,
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
                CodexReasoningLevel::Low,
                CodexReasoningLevel::Xhigh,
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
