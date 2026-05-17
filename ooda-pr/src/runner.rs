//! OODA loop driver: iterate observe → orient → decide → act until
//! a halt fires.
//!
//! # Invariants
//!
//! - **Iteration ≥ 1**: at least one full cycle runs on every
//!   invocation. Established by carrying the cap as `NonZeroU32`
//!   and unrolling the first iteration before the loop body.
//! - **Last-attempted is typed**: the cap-reached path returns the
//!   most recent action as `Action`, not `Option<Action>`. The
//!   unrolled first iteration is the proof that an action exists.
//! - **Stall detection on a stable key**: two consecutive Execute
//!   iterations with the same `(action discriminant, blocker)`
//!   halt as stalled. Wait actions are exempt — they are the
//!   designed shape for "no progress, by intent".
//! - **One halt taxonomy**: `HaltReason` partitions every exit
//!   path (success, terminal, handoff, stall, cap). There is no
//!   parallel "outcome" type at the loop boundary; exit-code
//!   projection lives on `HaltReason`.
//!
//! Stall detection is coarse — it catches single-action spinning,
//! not multi-step cycles. The iteration cap bounds the worst case.

use std::num::NonZeroU32;

use crate::act::{ActError, act};
use crate::axis_impls::ci::{CiAxis, CiObservation};
use crate::axis_impls::claude_review::{ClaudeReviewAxis, ClaudeReviewObservation};
use crate::axis_impls::closeout::{CloseoutAxis, CloseoutObservation};
use crate::axis_impls::copilot::{CopilotAxis, CopilotObservation};
use crate::axis_impls::cursor::{CursorAxis, CursorObservation};
use crate::axis_impls::doc_review::{DocReviewAxis, DocReviewObservation};
use crate::axis_impls::pull_request_metadata::{
    PullRequestMetadataAxis, PullRequestMetadataObservation,
};
use crate::axis_impls::reviews::{ReviewsAxis, ReviewsObservation};
use crate::axis_impls::state::{StateAxis, StateObservation};
use crate::decide::action::{Action, TargetEffect, rate_limit_wait_action};
use crate::decide::decision::{Decision, HaltReason};
use crate::ids::{PullRequestNumber, RepoSlug, Timestamp};
use crate::observe::github::gh::GhError;
use crate::observe::github::{FetchOutcome, GitHubObservations, fetch_all};
use crate::orient::OrientedState;
use crate::orient::orient;
use crate::recorder::Recorder;
use ooda_core::{Axis, decide_from_candidates};
use ooda_state::ObserveOutcome;

/// Driver-level orchestration: invoke each axis's `candidates()` via
/// the [`Axis`] trait and merge into one urgency-sorted list.
///
/// Composition is hand-written rather than trait-dispatched over a
/// uniform iterator. Two reasons: (a) each axis's `Observation` is
/// intentionally distinct (the declared-deps shape) — unifying them
/// behind a single dispatch fn would require per-axis adapters that
/// reconstruct typed Observations from a shared context, defeating
/// the declared-deps invariant; (b) cross-axis dep ordering (state
/// must project before CI consumes `has_open_parent_pr`) is intrinsic
/// and locally checkable when the call list is explicit.
///
/// Class invariant — *advancement preempts passivity*: an active
/// candidate the system can drive must outrank a candidate that
/// only waits on an external signal. The fallback merge-state
/// blocker fires only when no axis produced an advancement path;
/// `out.sort_by_key(|a| a.urgency)` performs the merge step
/// (stable: axis order within a tier is preserved).
pub(crate) fn drive(oriented: &OrientedState, pr: PullRequestNumber) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    out.extend(StateAxis.candidates(&StateObservation {
        state: &oriented.state,
        threads: &oriented.threads,
        merge_base_delta: oriented.merge_base_delta.as_ref(),
    }));
    out.extend(CiAxis.candidates(&CiObservation {
        report: &oriented.ci,
    }));
    out.extend(ReviewsAxis.candidates(&ReviewsObservation {
        reviews: &oriented.reviews,
        ci: &oriented.ci,
        copilot: oriented.copilot.as_ref(),
        threads: &oriented.threads,
    }));
    out.extend(CopilotAxis.candidates(&CopilotObservation {
        report: oriented.copilot.as_ref(),
    }));
    out.extend(CursorAxis.candidates(&CursorObservation {
        report: oriented.cursor.as_ref(),
    }));
    out.extend(
        PullRequestMetadataAxis.candidates(&PullRequestMetadataObservation {
            state: &oriented.state,
            pull_request_metadata: &oriented.pull_request_metadata,
            attest_path: oriented.attest_path.as_deref(),
            pr,
        }),
    );
    out.extend(DocReviewAxis.candidates(&DocReviewObservation {
        state: &oriented.state,
        doc_review: &oriented.doc_review,
        attest_path: oriented.doc_review_attest_path.as_deref(),
        pr,
    }));
    out.extend(ClaudeReviewAxis.candidates(&ClaudeReviewObservation {
        claude_review: &oriented.claude_review,
        attest_path: oriented.claude_review_attest_path.as_deref(),
        pr,
    }));
    out.extend(CloseoutAxis.candidates(&CloseoutObservation {
        closeout: &oriented.closeout,
        attest_path: oriented.closeout_attest_path.as_deref(),
        pr,
    }));
    let has_advancement_path = out.iter().any(|a| {
        matches!(
            a.target_effect,
            TargetEffect::Blocks | TargetEffect::Advances,
        )
    });
    if !has_advancement_path {
        out.extend(crate::decide::state::fallback_merge_state_blocker(
            &oriented.state,
        ));
    }
    out.sort_by_key(|a| a.urgency);
    out
}

/// Wall-clock for one iteration's worth of orient work.
///
/// Read once per iteration so every axis sees the same instant.
/// Axes that need the clock take it as a parameter — tests pass a
/// fixed value to keep behaviour deterministic.
pub(crate) fn current_timestamp() -> Timestamp {
    let now = chrono::Utc::now().to_rfc3339();
    // System clock's RFC-3339 rendering round-trips by construction.
    Timestamp::parse(&now).expect("chrono::Utc::now() round-trips through RFC-3339")
}

#[derive(Debug)]
pub enum LoopError {
    Observe(GhError),
    Act(ActError),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observe(e) => write!(f, "observe: {e}"),
            Self::Act(e) => write!(f, "act: {e}"),
        }
    }
}

impl std::error::Error for LoopError {}

#[derive(Clone, Copy)]
pub(crate) struct LoopConfig {
    /// Iteration cap. Carrying it as `NonZeroU32` makes the
    /// "iteration ≥ 1" invariant structural — input validation
    /// happens once at the boundary, and the loop body relies on
    /// the type rather than re-checking.
    pub max_iterations: NonZeroU32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: NonZeroU32::new(50).expect("50 is non-zero"),
        }
    }
}

/// One iteration's terminal step. Partitions an iteration's outcome
/// into the two arms the driver dispatches on: an early-halt that
/// short-circuits the loop, or a completed Execute that the driver
/// records as the running last-attempted action.
pub(crate) enum IterStep {
    /// Halt immediately with this reason.
    Halt(HaltReason),
    /// Executed; this action becomes last-attempted and (if not a
    /// Wait) updates the stall comparator.
    Executed(Action),
}

/// Drive a PR until a halt fires or the iteration cap trips.
///
/// `on_state` runs once per iteration between decide and act, and
/// once again on a halt decision before returning. It is the only
/// observer of per-iteration intermediate state — rendering, comment
/// posting, and run-bundle recording hang off it without further
/// coupling to the driver.
pub(crate) fn run_loop(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    state_root: Option<&std::path::Path>,
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<HaltReason, LoopError> {
    run_loop_with(config, |iter, last_non_wait_key| {
        run_iter(
            slug,
            pr,
            state_root,
            recorder,
            &mut on_state,
            iter,
            last_non_wait_key,
        )
    })
}

/// Pure loop driver. Owns the four module-level invariants —
/// iteration ≥ 1, stall comparator over non-Wait actions, Wait
/// stall-exemption, cap enforcement — independently of how an
/// iteration is realised.
///
/// The per-iteration callback is a parameter so scripted-decision
/// tests can pin each invariant in isolation; production wiring
/// supplies the observation-bound implementation.
pub(crate) fn run_loop_with<F>(
    config: LoopConfig,
    mut run_iter_fn: F,
) -> Result<HaltReason, LoopError>
where
    F: FnMut(u32, Option<&ooda_core::StallKey>) -> Result<IterStep, LoopError>,
{
    let max_iter = config.max_iterations.get();
    // First iteration is unrolled: the NonZeroU32 cap guarantees it
    // runs, and a stall comparator does not exist yet.
    let mut last_attempted: Action = match run_iter_fn(1, None)? {
        IterStep::Halt(reason) => return Ok(reason),
        IterStep::Executed(action) => action,
    };
    // Wait does not seed the stall comparator. An axis that detects
    // degradation must surface it as a non-Wait action; varying only
    // the blocker on a Wait is invisible to this comparator.
    let mut last_non_wait_key = if last_attempted.effect.is_wait() {
        None
    } else {
        Some(last_attempted.stall_key())
    };
    for iter in 2..=max_iter {
        let step = run_iter_fn(iter, last_non_wait_key.as_ref())?;
        match step {
            IterStep::Halt(reason) => return Ok(reason),
            IterStep::Executed(action) => {
                if !action.effect.is_wait() {
                    last_non_wait_key = Some(action.stall_key());
                }
                last_attempted = action;
            }
        }
    }
    // `last_attempted: Action` is the witness for the cap-reached
    // path: the unrolled first iteration either returned a Halt or
    // populated it.
    Ok(HaltReason::CapReached(last_attempted))
}

/// One observe → orient → decide → act cycle. Returns an early-halt
/// reason or the action that just executed; observation failures
/// bubble as `LoopError`.
fn run_iter(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    state_root: Option<&std::path::Path>,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey>,
) -> Result<IterStep, LoopError> {
    recorder.set_iteration(Some(iter));
    recorder.record_observe_start(iter);
    let obs = match fetch_all(slug, pr, state_root) {
        Ok(FetchOutcome::Observations(obs)) => {
            recorder.record_observe_end(iter, ObserveOutcome::Ok);
            *obs
        }
        Ok(FetchOutcome::RateLimited(hit)) => {
            // Observe was throttled before all axes could be seen.
            // The only valid response is to wait the upstream's
            // retry window and re-observe from a clean slate;
            // running orient/decide on a partial bundle would
            // surface false halts.
            recorder.record_observe_end(
                iter,
                ObserveOutcome::RateLimited {
                    scope: hit.scope.name().to_string(),
                    retry_after_secs: hit.retry_after.as_duration().as_secs(),
                },
            );
            let action = rate_limit_wait_action(hit);
            recorder.record_action_start(iter, &action);
            recorder.record_wait_start(iter, &action);
            let act_result = act(&action, slug, pr);
            if act_result.is_ok() {
                recorder.record_wait_end(iter, &action);
            }
            recorder.record_action_end(
                iter,
                &action,
                act_result.as_ref().copied().map_err(ToString::to_string),
            );
            act_result.map_err(LoopError::Act)?;
            return Ok(IterStep::Executed(action));
        }
        Err(e) => {
            recorder.record_observe_end(iter, ObserveOutcome::Error(e.to_string()));
            return Err(LoopError::Observe(e));
        }
    };
    let now = current_timestamp();
    let oriented = orient(&obs, None, now);
    let candidates = drive(&oriented, pr);
    let decision = decide_from_candidates(candidates.clone(), obs.pull_request_view.state);
    on_state(iter, &obs, &oriented, &candidates, &decision);

    match decision {
        Decision::Halt(halt) => Ok(IterStep::Halt(HaltReason::Decision(halt))),
        Decision::Execute(action) => {
            // Stall comparison runs *before* the side effect: a
            // Full action whose upstream is eventually consistent
            // must not fire twice while the prior call is still
            // propagating. Equality on `StallKey<K>` — the
            // `(discriminant, blocker)` pair — is the test.
            let current_key = action.stall_key();
            if last_non_wait_key == Some(&current_key) {
                return Ok(IterStep::Halt(HaltReason::Stalled(action)));
            }
            let is_wait = action.effect.is_wait();
            recorder.record_action_start(iter, &action);
            if is_wait {
                recorder.record_wait_start(iter, &action);
            }
            let act_result = act(&action, slug, pr);
            if is_wait && act_result.is_ok() {
                recorder.record_wait_end(iter, &action);
            }
            recorder.record_action_end(
                iter,
                &action,
                act_result.as_ref().copied().map_err(ToString::to_string),
            );
            act_result.map_err(LoopError::Act)?;
            Ok(IterStep::Executed(action))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::decide::decision::{DecisionHalt, Terminal};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;
    use ooda_core::PollingInterval;
    use std::cell::RefCell;

    fn cfg(max: u32) -> LoopConfig {
        LoopConfig {
            max_iterations: NonZeroU32::new(max).expect("nonzero"),
        }
    }

    fn full_action(kind: ActionKind, blocker: &str) -> Action {
        Action {
            kind,
            effect: ActionEffect::Full { log: "stub".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::for_test(blocker),
        }
    }

    fn wait_action(kind: ActionKind, blocker: &str) -> Action {
        Action {
            kind,
            effect: ActionEffect::Wait {
                interval: PollingInterval::from_secs(1),
                log: "stub".into(),
            },
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Mid(MidTier::BlockingWait),
            blocker: BlockerKey::for_test(blocker),
        }
    }

    fn rebase() -> Action {
        full_action(ActionKind::Rebase, "rebase-needed")
    }

    /// Scripted iteration callback. Yields the next `IterStep`
    /// from a fixed sequence and applies the same pre-act stall
    /// rule the production driver uses: a scripted Execute whose
    /// key matches the supplied comparator is rewritten to a
    /// `Halt(Stalled)`. Test authors specify decisions in order;
    /// the stall invariant is still exercised end-to-end.
    fn scripted(
        seq: Vec<IterStep>,
    ) -> impl FnMut(u32, Option<&ooda_core::StallKey>) -> Result<IterStep, LoopError> {
        let cell = RefCell::new(seq.into_iter());
        move |_iter, last_non_wait_key| {
            let next = cell
                .borrow_mut()
                .next()
                .expect("run_loop_with called run_iter past the end of the scripted sequence");
            match next {
                IterStep::Executed(action) => {
                    let current_key = action.stall_key();
                    if last_non_wait_key == Some(&current_key) {
                        Ok(IterStep::Halt(HaltReason::Stalled(action)))
                    } else {
                        Ok(IterStep::Executed(action))
                    }
                }
                halt @ IterStep::Halt(_) => Ok(halt),
            }
        }
    }

    #[test]
    fn iter_1_always_runs_at_max_one() {
        // Locks the "iteration ≥ 1" invariant at its tightest cap.
        let halt = run_loop_with(
            cfg(1),
            scripted(vec![IterStep::Halt(HaltReason::Decision(
                DecisionHalt::Success,
            ))]),
        )
        .unwrap();
        assert!(matches!(halt, HaltReason::Decision(DecisionHalt::Success)));
    }

    #[test]
    fn cap_reached_carries_typed_last_action() {
        // Two distinct Execute actions across cap=2: stall does not
        // fire and the cap-reached path returns the latest typed
        // action as its witness.
        let halt = run_loop_with(
            cfg(2),
            scripted(vec![
                IterStep::Executed(rebase()),
                IterStep::Executed(full_action(ActionKind::MarkReady, "draft")),
            ]),
        )
        .unwrap();
        match halt {
            HaltReason::CapReached(action) => {
                assert!(matches!(action.kind, ActionKind::MarkReady));
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    #[test]
    fn stall_detects_repeated_non_wait_action() {
        // Two identical Rebase actions in a row → stall on iter 2.
        let halt = run_loop_with(
            cfg(10),
            scripted(vec![
                IterStep::Executed(rebase()),
                IterStep::Executed(rebase()),
            ]),
        )
        .unwrap();
        match halt {
            HaltReason::Stalled(action) => {
                assert!(matches!(action.kind, ActionKind::Rebase));
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
    }

    #[test]
    fn wait_actions_are_stall_exempt() {
        // Repeated Waits must run to cap, not stall: the comparator
        // intentionally ignores Wait keys.
        let wait = || {
            wait_action(
                ActionKind::WaitForRateLimit {
                    scope: ooda_core::RateLimitScope::GitHubGraphqlPrimary,
                },
                "rate-limit",
            )
        };
        let halt = run_loop_with(
            cfg(3),
            scripted(vec![
                IterStep::Executed(wait()),
                IterStep::Executed(wait()),
                IterStep::Executed(wait()),
            ]),
        )
        .unwrap();
        match halt {
            HaltReason::CapReached(action) => {
                assert!(action.effect.is_wait());
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    #[test]
    fn wait_does_not_seed_stall_key_for_subsequent_non_wait() {
        // Composition: Wait does not populate the comparator, so a
        // following Execute starts with no prior key; stall only
        // fires on the second Execute that repeats it.
        let halt = run_loop_with(
            cfg(5),
            scripted(vec![
                IterStep::Executed(wait_action(
                    ActionKind::WaitForRateLimit {
                        scope: ooda_core::RateLimitScope::GitHubGraphqlPrimary,
                    },
                    "rate-limit",
                )),
                IterStep::Executed(rebase()),
                IterStep::Executed(rebase()),
            ]),
        )
        .unwrap();
        match halt {
            HaltReason::Stalled(action) => {
                assert!(matches!(action.kind, ActionKind::Rebase));
            }
            other => panic!("expected Stalled on iter 3, got {other:?}"),
        }
    }

    #[test]
    fn halt_on_iter_n_returns_immediately() {
        // A mid-loop halt is the loop's exit, regardless of
        // remaining cap budget.
        let halt = run_loop_with(
            cfg(10),
            scripted(vec![
                IterStep::Executed(rebase()),
                IterStep::Halt(HaltReason::Decision(DecisionHalt::Terminal(
                    Terminal::Aborted,
                ))),
            ]),
        )
        .unwrap();
        assert!(matches!(
            halt,
            HaltReason::Decision(DecisionHalt::Terminal(Terminal::Aborted))
        ));
    }
}
