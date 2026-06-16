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
use crate::axis_impls::branch_sync::BranchSyncAxis;
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
use crate::decide::action::{Action, rate_limit_wait_action};
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
/// Class invariant — *specific preempts fallback*: a candidate
/// whose axis names a concrete gate must outrank a candidate that
/// only reports "no modeled gate fires". The fallback merge-state
/// blocker (`merge_blocked_policy`) fires when GitHub reports
/// `BLOCKED` and no modeled axis explains the block; if any
/// candidate with a different blocker exists, the policy fallback
/// is dropped before `out.sort_by_key(|a| a.urgency)` so the
/// specific axis wins regardless of tier. The sort is stable: axis
/// order within a tier is preserved.
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
    out.extend(BranchSyncAxis.candidates(&&oriented.branch_sync));
    // Merge-eligibility closure check. Runs unconditionally — the
    // axis pattern below trusts a positive-eligibility predicate
    // over `mergeStateStatus`, drilled-into-cause for BLOCKED, and
    // stale-state cross-checks for CLEAN/UNSTABLE/HAS_HOOKS. Without
    // this, OODA's verdict-by-absence would project a still-
    // unmergeable PR into `Decision::Halt(Success)` whenever the
    // explaining gate sits outside the modeled axis set.
    //
    // Composes via urgency ordering, not via masking: other axes'
    // `BlockingFix` candidates outrank this axis's `BlockingHuman`
    // candidates, so concurrent firings are decided by the picker.
    out.extend(
        crate::decide::merge_eligibility::merge_eligibility_candidates(
            &oriented.state,
            &oriented.threads,
            &oriented.reviews,
            &oriented.ci,
        ),
    );
    out.extend(crate::decide::signing_eligibility::signing_eligibility_candidates(&oriented.state));
    yield_policy_to_actionable(&mut out);
    out.sort_by_key(|a| a.urgency);
    out
}

/// Verdict-by-absence: `merge_blocked_policy` is the closure-check
/// fallback that fires when GitHub reports `BLOCKED` and no modeled
/// gate explains the block. Whenever any other candidate is in the
/// set, that candidate names a concrete blocker — strictly more
/// informative than the fallback's "no modeled gate fires" handoff.
/// Drop the fallback so the specific axis wins the iteration,
/// independent of urgency tier.
///
/// The yield is one-way and per-blocker, not per-tier: a candidate
/// set consisting only of `merge_blocked_policy` entries leaves the
/// fallback in place — it is then the only signal.
fn yield_policy_to_actionable(candidates: &mut Vec<Action>) {
    let has_specific = candidates
        .iter()
        .any(|a| a.blocker.as_str() != "merge_blocked_policy");
    if has_specific {
        candidates.retain(|a| a.blocker.as_str() != "merge_blocked_policy");
    }
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
    /// Per-PR index directory or other recorder-owned path could
    /// not be resolved (mutex poison, `create_dir_all` failure).
    /// Surfaced explicitly rather than degrading to a cwd-relative
    /// fallback path that would silently collapse distinct PRs
    /// onto a shared lock / dedup / sticky file.
    Recorder(crate::recorder::RecorderError),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observe(e) => write!(f, "observe: {e}"),
            Self::Act(e) => write!(f, "act: {e}"),
            Self::Recorder(e) => write!(f, "recorder: {e}"),
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

/// Loop terminator. `Halted` is the ordinary exit path (decision /
/// stall / cap); `SignalInterrupted` carries the trapped
/// `SIGINT` / `SIGTERM` exit code observed at an iteration
/// boundary. Both arms route through `Outcome::from(_)` at the
/// `main`-side boundary.
#[derive(Debug)]
pub(crate) enum LoopExit {
    Halted(HaltReason),
    SignalInterrupted { exit_code: u8 },
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
    repo_root: &std::path::Path,
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<LoopExit, LoopError> {
    run_loop_with(config, |iter, last_non_wait_key, auto_wait_used_for| {
        run_iter(
            slug,
            pr,
            state_root,
            repo_root,
            recorder,
            &mut on_state,
            iter,
            last_non_wait_key,
            auto_wait_used_for,
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
) -> Result<LoopExit, LoopError>
where
    F: FnMut(
        u32,
        Option<&ooda_core::StallKey>,
        Option<&ooda_core::StallKey>,
    ) -> Result<IterStep, LoopError>,
{
    let max_iter = config.max_iterations.get();
    // Boundary signal poll. The handler runs in signal context and
    // only stores into the atomic; the loop owns every side effect
    // (recorder.halt + live-marker release) so the terminal event
    // lands on the same write path as every other halt.
    if let Some(exit_code) = crate::signal::check_shutdown() {
        return Ok(LoopExit::SignalInterrupted { exit_code });
    }
    // First iteration is unrolled: the NonZeroU32 cap guarantees it
    // runs, and a stall comparator does not exist yet.
    let mut last_attempted: Action = match run_iter_fn(1, None, None)? {
        IterStep::Halt(reason) => return Ok(LoopExit::Halted(reason)),
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
    // Eventual-consistency auto-Wait state. After a Full+Eventual
    // action repeats and the runner converts it to a synthetic Wait,
    // we stash its stall key here. A third consecutive non-Wait
    // emission of the same key is a genuine stall — the
    // propagation window was already granted and produced no
    // observable change.
    //
    // Reset when a non-Wait action with a different key fires
    // (real progress elsewhere): the K1 blocker is no longer the
    // tip of the loop's attention, so re-arming auto-Wait for K1
    // on a future return is appropriate. Natural Wait actions and
    // synthetic Wait actions leave the state intact so an
    // oscillation pattern (K1-Full → K2-Wait → K1-Full → …)
    // halts correctly on the second K1 reappearance.
    let mut auto_wait_used_for: Option<ooda_core::StallKey> = None;
    for iter in 2..=max_iter {
        if let Some(exit_code) = crate::signal::check_shutdown() {
            return Ok(LoopExit::SignalInterrupted { exit_code });
        }
        let step = run_iter_fn(
            iter,
            last_non_wait_key.as_ref(),
            auto_wait_used_for.as_ref(),
        )?;
        match step {
            IterStep::Halt(reason) => return Ok(LoopExit::Halted(reason)),
            IterStep::Executed(action) => {
                let key = action.stall_key();
                let is_wait = action.effect.is_wait();
                // Synthetic-Wait detection: a Wait whose key
                // matches the still-pending non-Wait stall key
                // can only have arrived via the synthetic-Wait
                // conversion below (natural Wait kinds use a
                // different `ActionKind` discriminant, so their
                // keys differ). Seed `auto_wait_used_for` so the
                // next iteration of the same key halts.
                let synthetic_wait_conversion = is_wait && last_non_wait_key.as_ref() == Some(&key);
                if synthetic_wait_conversion {
                    auto_wait_used_for = Some(key.clone());
                } else if !is_wait {
                    // Non-Wait action. Update the comparator.
                    // If this is a *different* key than the
                    // currently-armed auto-Wait, real progress
                    // happened: clear the auto-Wait state. If it
                    // matches, the run_iter callee already halted
                    // via the "already auto-waited" branch — we
                    // never reach this match arm in that case.
                    if auto_wait_used_for.as_ref() != Some(&key) {
                        auto_wait_used_for = None;
                    }
                    last_non_wait_key = Some(key);
                }
                last_attempted = action;
            }
        }
    }
    // `last_attempted: Action` is the witness for the cap-reached
    // path: the unrolled first iteration either returned a Halt or
    // populated it.
    Ok(LoopExit::Halted(HaltReason::CapReached(last_attempted)))
}

/// Pre-act stall comparator outcomes. `Proceed` carries the
/// (possibly synthetic-Wait-converted) action forward to the
/// recorder/act path; `Halt` short-circuits to a Stalled halt.
enum StallCheckOutcome {
    Proceed(Action),
    Halt(HaltReason),
}

/// Three outcomes on a non-Wait `(kind, blocker)` repeat:
///
/// 1. Already auto-waited for this key (`auto_wait_used_for` matches):
///    the propagation window was granted and produced no observable
///    change. Halt as Stalled.
/// 2. Eventual upstream declared on the effect: convert the Full to
///    a synthetic Wait of the declared propagation interval. The
///    outer loop detects the conversion via stall-key match on a
///    Wait action and arms `auto_wait_used_for` so a third repeat
///    halts via path #1.
/// 3. Sync upstream: a repeat IS a genuine stall — orient re-derived
///    the blocker after the Sync effect should have resolved it.
///    Halt as Stalled.
fn apply_stall_check(
    mut action: Action,
    last_non_wait_key: Option<&ooda_core::StallKey>,
    auto_wait_used_for: Option<&ooda_core::StallKey>,
) -> StallCheckOutcome {
    let current_key = action.stall_key();
    if last_non_wait_key == Some(&current_key) {
        if auto_wait_used_for == Some(&current_key) {
            return StallCheckOutcome::Halt(HaltReason::Stalled(action));
        }
        if let Some(wait_effect) = action.effect.synthetic_wait_on_repeat() {
            action.effect = wait_effect;
        } else {
            return StallCheckOutcome::Halt(HaltReason::Stalled(action));
        }
    }
    StallCheckOutcome::Proceed(action)
}

/// One observe → orient → decide → act cycle. Returns an early-halt
/// reason or the action that just executed; observation failures
/// bubble as `LoopError`.
#[allow(clippy::too_many_arguments)]
fn run_iter(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    state_root: Option<&std::path::Path>,
    repo_root: &std::path::Path,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey>,
    auto_wait_used_for: Option<&ooda_core::StallKey>,
) -> Result<IterStep, LoopError> {
    recorder.set_iteration(Some(iter));
    recorder.record_observe_start(iter);
    let sticky_path = recorder
        .last_seen_head_path()
        .map_err(LoopError::Recorder)?;
    let obs = match fetch_all(slug, pr, state_root, Some(&sticky_path), repo_root) {
        Ok(FetchOutcome::Observations(obs)) => {
            recorder.record_observe_end(iter, ObserveOutcome::Ok);
            // Post-observe sticky update: record the current
            // remote head as the new baseline. Best-effort —
            // a sticky write failure leaves the divergence
            // signal stale for one iteration, never bricks
            // the loop.
            let _ = crate::observe::branch::write_sticky(
                &sticky_path,
                obs.pull_request_view.head_ref_oid.as_str(),
                false,
            );
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
            // Pairing invariant: every `action_started` /
            // `wait_started` event must be followed by a matching
            // `action_end` / `wait_end` on the same iteration.
            // Resolve the lock path *before* emitting started events
            // — if `action_lock_path` errs (`RecorderError`), `?`
            // propagates immediately, so no started events are ever
            // written. The on-disk story stays consistent: either
            // both halves of the pair are present, or neither is.
            let lock_path = recorder.action_lock_path().map_err(LoopError::Recorder)?;
            recorder.record_action_start(iter, &action);
            recorder.record_wait_start(iter, &action);
            let act_result = act(&action, slug, pr, &lock_path, repo_root);
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
        Decision::Execute(mut action) => {
            // Stall comparator: see [`apply_stall_check`].
            match apply_stall_check(action, last_non_wait_key, auto_wait_used_for) {
                StallCheckOutcome::Halt(reason) => return Ok(IterStep::Halt(reason)),
                StallCheckOutcome::Proceed(a) => action = a,
            }
            let is_wait = action.effect.is_wait();
            // Pairing invariant — see rate-limit branch above. Lock
            // path resolves before any started event is emitted so a
            // `RecorderError` here cannot leave a started↔end pair
            // half-open.
            let lock_path = recorder.action_lock_path().map_err(LoopError::Recorder)?;
            recorder.record_action_start(iter, &action);
            if is_wait {
                recorder.record_wait_start(iter, &action);
            }
            let act_result = act(&action, slug, pr, &lock_path, repo_root);
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
            effect: ActionEffect::Full {
                log: "stub".into(),
                upstream: ooda_core::UpstreamConsistency::Sync,
            },
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

    fn eventual_full_action(kind: ActionKind, blocker: &str) -> Action {
        Action {
            kind,
            effect: ActionEffect::Full {
                log: "stub".into(),
                upstream: ooda_core::UpstreamConsistency::Eventual(PollingInterval::from_secs(30)),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::for_test(blocker),
        }
    }

    fn rerequest_copilot() -> Action {
        eventual_full_action(
            ActionKind::RerequestCopilot { symptom: None },
            "copilot_tier_gold",
        )
    }

    /// Scripted iteration callback. Yields the next `IterStep`
    /// from a fixed sequence and applies the same pre-act stall
    /// rule the production driver uses: a scripted Execute whose
    /// key matches the supplied comparator is either rewritten
    /// to a `Halt(Stalled)` (Sync upstream, or already-auto-waited
    /// Eventual upstream) or to a synthetic `Wait` (first repeat
    /// of an Eventual Full). Test authors specify decisions in
    /// order; the stall + auto-wait invariants are still
    /// exercised end-to-end.
    fn scripted(
        seq: Vec<IterStep>,
    ) -> impl FnMut(
        u32,
        Option<&ooda_core::StallKey>,
        Option<&ooda_core::StallKey>,
    ) -> Result<IterStep, LoopError> {
        let cell = RefCell::new(seq.into_iter());
        move |_iter, last_non_wait_key, auto_wait_used_for| {
            let next = cell
                .borrow_mut()
                .next()
                .expect("run_loop_with called run_iter past the end of the scripted sequence");
            match next {
                IterStep::Executed(mut action) => {
                    let current_key = action.stall_key();
                    if last_non_wait_key == Some(&current_key) {
                        if auto_wait_used_for == Some(&current_key) {
                            return Ok(IterStep::Halt(HaltReason::Stalled(action)));
                        }
                        if let Some(wait_effect) = action.effect.synthetic_wait_on_repeat() {
                            action.effect = wait_effect;
                        } else {
                            return Ok(IterStep::Halt(HaltReason::Stalled(action)));
                        }
                    }
                    Ok(IterStep::Executed(action))
                }
                halt @ IterStep::Halt(_) => Ok(halt),
            }
        }
    }

    fn unwrap_halt(exit: LoopExit) -> HaltReason {
        match exit {
            LoopExit::Halted(reason) => reason,
            LoopExit::SignalInterrupted { exit_code } => {
                panic!("unexpected signal-interrupt exit (code {exit_code})")
            }
        }
    }

    #[test]
    fn iter_1_always_runs_at_max_one() {
        // Locks the "iteration ≥ 1" invariant at its tightest cap.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
            cfg(1),
            scripted(vec![IterStep::Halt(HaltReason::Decision(
                DecisionHalt::Success,
            ))]),
        )
        .unwrap();
        let halt = unwrap_halt(exit);
        assert!(matches!(halt, HaltReason::Decision(DecisionHalt::Success)));
    }

    #[test]
    fn cap_reached_carries_typed_last_action() {
        // Two distinct Execute actions across cap=2: stall does not
        // fire and the cap-reached path returns the latest typed
        // action as its witness.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
            cfg(2),
            scripted(vec![
                IterStep::Executed(rebase()),
                IterStep::Executed(full_action(ActionKind::MarkReady, "draft")),
            ]),
        )
        .unwrap();
        match unwrap_halt(exit) {
            HaltReason::CapReached(action) => {
                assert!(matches!(action.kind, ActionKind::MarkReady));
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    #[test]
    fn stall_detects_repeated_non_wait_action() {
        // Two identical Rebase actions in a row → stall on iter 2.
        // Rebase is a Sync Full (the test stub builders default to
        // Sync); the synthetic-Wait conversion is opt-in via the
        // typed Eventual marker on the effect, so Sync stalls
        // immediately as it always did.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
            cfg(10),
            scripted(vec![
                IterStep::Executed(rebase()),
                IterStep::Executed(rebase()),
            ]),
        )
        .unwrap();
        match unwrap_halt(exit) {
            HaltReason::Stalled(action) => {
                assert!(matches!(action.kind, ActionKind::Rebase));
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
    }

    #[test]
    fn eventual_full_repeat_converts_to_synthetic_wait_then_halts_on_third() {
        // Peer-reported StuckRepeated regression on the stale-Copilot
        // axis (per the on-disk run trace at the time):
        //
        //   iter1: RerequestCopilot(Full, blocker copilot_tier:gold)
        //   iter2: same key — pre-fix the runner halted Stalled
        //   immediately, even though the upstream `gh api POST` was
        //   either eventual (timeline event lag) or idempotent
        //   (already-pending reviewer → no new event).
        //
        // With Eventual upstream declared on the effect, iter2 is
        // converted to a synthetic Wait so propagation has a chance
        // to surface. A third consecutive identical key IS a genuine
        // stall — the propagation window was already granted — and
        // halts as Stalled.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
            cfg(10),
            scripted(vec![
                IterStep::Executed(rerequest_copilot()),
                IterStep::Executed(rerequest_copilot()),
                IterStep::Executed(rerequest_copilot()),
            ]),
        )
        .unwrap();
        match unwrap_halt(exit) {
            HaltReason::Stalled(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::RerequestCopilot { symptom: None },
                ));
            }
            other => panic!("expected Stalled on iter 3, got {other:?}"),
        }
    }

    #[test]
    fn eventual_full_then_progress_rearms_auto_wait() {
        // After a synthetic-Wait conversion at iter2, a non-Wait
        // action with a *different* key at iter3 represents real
        // progress on another axis. Returning to the original Full
        // at iter5 must trigger a fresh auto-Wait (not an immediate
        // halt) — the propagation budget resets per "consecutive
        // repeat episode," not for the whole loop.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
            cfg(10),
            scripted(vec![
                IterStep::Executed(rerequest_copilot()),
                IterStep::Executed(rerequest_copilot()), // synthetic Wait
                IterStep::Executed(rebase()),            // different key, real progress
                IterStep::Executed(rerequest_copilot()), // re-armed; passes stall check
                IterStep::Executed(rerequest_copilot()), // synthetic Wait again
                IterStep::Executed(rerequest_copilot()), // genuine stall
            ]),
        )
        .unwrap();
        match unwrap_halt(exit) {
            HaltReason::Stalled(action) => {
                assert!(matches!(
                    action.kind,
                    ActionKind::RerequestCopilot { symptom: None },
                ));
            }
            other => panic!("expected Stalled on iter 6, got {other:?}"),
        }
    }

    #[test]
    fn wait_actions_are_stall_exempt() {
        // Repeated Waits must run to cap, not stall: the comparator
        // intentionally ignores Wait keys.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let wait = || {
            wait_action(
                ActionKind::WaitForRateLimit {
                    scope: ooda_core::RateLimitScope::GitHubGraphqlPrimary,
                },
                "rate-limit",
            )
        };
        let exit = run_loop_with(
            cfg(3),
            scripted(vec![
                IterStep::Executed(wait()),
                IterStep::Executed(wait()),
                IterStep::Executed(wait()),
            ]),
        )
        .unwrap();
        match unwrap_halt(exit) {
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
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
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
        match unwrap_halt(exit) {
            HaltReason::Stalled(action) => {
                assert!(matches!(action.kind, ActionKind::Rebase));
            }
            other => panic!("expected Stalled on iter 3, got {other:?}"),
        }
    }

    /// Process-global mutex guarding the shutdown atomic for
    /// signal-poll unit tests. The atomic is global; sibling tests
    /// must serialize against `set_for_test` so their boundary
    /// poll doesn't observe a stray signal stored by another.
    static SIGNAL_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn signal_interrupt_short_circuits_before_first_iteration() {
        // Direct-poke the shutdown atomic to simulate a `SIGTERM`
        // landing before the loop body runs. The driver must
        // observe the signal at the boundary check and route to
        // `LoopExit::SignalInterrupted` without dispatching the
        // scripted callback. Reset the atomic afterwards so
        // sibling tests in the same process see a clean slate.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::set_for_test(143);
        let exit = run_loop_with(
            cfg(10),
            scripted(vec![IterStep::Halt(HaltReason::Decision(
                DecisionHalt::Success,
            ))]),
        )
        .unwrap();
        crate::signal::reset_for_test();
        match exit {
            LoopExit::SignalInterrupted { exit_code } => assert_eq!(exit_code, 143),
            halt @ LoopExit::Halted(_) => panic!("expected SignalInterrupted, got {halt:?}"),
        }
    }

    #[test]
    fn signal_interrupt_at_mid_loop_iteration_boundary() {
        // Drive iter 1 to completion, then arm the signal so iter
        // 2's boundary check observes it. Validates the in-loop
        // poll (not just the pre-loop one).
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let cell = std::cell::RefCell::new(0u32);
        let exit = run_loop_with(cfg(10), |iter, _, _| {
            *cell.borrow_mut() = iter;
            if iter == 1 {
                // Arm the signal *after* iter 1 so the boundary
                // check at the top of iter 2 picks it up.
                crate::signal::set_for_test(130);
                Ok(IterStep::Executed(rebase()))
            } else {
                // Iter 2 should never run — the boundary poll
                // short-circuits before invoking the closure.
                panic!("iter {iter} ran past signal-arming");
            }
        })
        .unwrap();
        crate::signal::reset_for_test();
        assert_eq!(*cell.borrow(), 1, "iter 1 must have run");
        match exit {
            LoopExit::SignalInterrupted { exit_code } => assert_eq!(exit_code, 130),
            halt @ LoopExit::Halted(_) => panic!("expected SignalInterrupted(130), got {halt:?}"),
        }
    }

    #[test]
    fn halt_on_iter_n_returns_immediately() {
        // A mid-loop halt is the loop's exit, regardless of
        // remaining cap budget.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        let exit = run_loop_with(
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
            unwrap_halt(exit),
            HaltReason::Decision(DecisionHalt::Terminal(Terminal::Aborted))
        ));
    }

    /// Test helper: build a Full action at an arbitrary `MidTier`.
    /// Lets the yield-filter tests cover every tier the fallback
    /// can lose to under stable-sort, not just `BlockingFix`.
    fn action_at(tier: MidTier, blocker: &str) -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Full {
                log: "stub".into(),
                upstream: ooda_core::UpstreamConsistency::Sync,
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(tier),
            blocker: BlockerKey::for_test(blocker),
        }
    }

    fn policy() -> Action {
        // `merge_blocked_policy` is emitted at `MidTier::Pathology`
        // by the merge-eligibility axis; the yield filter is
        // per-blocker, but mirroring the production tier keeps the
        // fixtures honest.
        action_at(MidTier::Pathology, "merge_blocked_policy")
    }

    #[test]
    fn yield_drops_policy_when_pathology_peer_present() {
        // Sibling at the same tier with a different blocker — the
        // B1/H2 case (`signing_blocked_unverified` recovery recipe).
        let mut cands = vec![
            policy(),
            action_at(MidTier::Pathology, "signing_blocked_unverified"),
        ];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].blocker.as_str(), "signing_blocked_unverified");
    }

    #[test]
    fn yield_drops_policy_when_blocking_human_peer_present() {
        // A named-reviewer / approval gate at BlockingHuman is more
        // informative than the "do not infer the cause" handoff.
        let mut cands = vec![
            policy(),
            action_at(MidTier::BlockingHuman, "merge_blocked_pending_approval"),
        ];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].blocker.as_str(), "merge_blocked_pending_approval");
    }

    #[test]
    fn yield_drops_policy_when_blocking_fix_peer_present() {
        // Original case the pre-generalization filter handled —
        // must continue to hold under the broader rule.
        let mut cands = vec![
            policy(),
            action_at(MidTier::BlockingFix, "unresolved_threads"),
        ];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].blocker.as_str(), "unresolved_threads");
    }

    #[test]
    fn yield_drops_policy_when_critical_peer_present() {
        // H4 case: any Critical-tier candidate (e.g. a rate-limit
        // wait or state-axis conflict) names a more specific gate.
        let mut cands = vec![policy(), action_at(MidTier::Critical, "rate_limit_wait")];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].blocker.as_str(), "rate_limit_wait");
    }

    #[test]
    fn yield_keeps_policy_when_alone() {
        // Policy is the only signal — the verdict-by-absence
        // contract requires it survive so closure check still
        // surfaces the BLOCKED state.
        let mut cands = vec![policy()];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].blocker.as_str(), "merge_blocked_policy");
    }

    #[test]
    fn yield_keeps_policy_when_only_other_candidate_is_also_policy() {
        // Degenerate but tractable: two policy entries, no specific
        // blocker present. Filter must not self-drop on a peer with
        // the same blocker key.
        let mut cands = vec![policy(), policy()];
        yield_policy_to_actionable(&mut cands);
        assert_eq!(cands.len(), 2);
        assert!(
            cands
                .iter()
                .all(|a| a.blocker.as_str() == "merge_blocked_policy")
        );
    }
}
