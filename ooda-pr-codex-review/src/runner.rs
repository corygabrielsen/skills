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

use crate::act::{ActContext, ActError, act};
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
use crate::ids::{CodexReasoningLevel, PullRequestNumber, Timestamp};
use crate::observe::codex::{CodexObservations, fetch_all as fetch_codex};
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
/// Codex-review divergence: this binary's wiring includes a tenth
/// axis call (`codex_review`) interleaved at the bot-review tier;
/// the urgency sort settles its relative position alongside the
/// other bot-review axes.
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
    if let Some(c) = &oriented.codex_review {
        out.extend(crate::decide::codex_review::candidates(c));
    }
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
            oriented.reviews.decision,
            &oriented.ci,
        ),
    );
    out.extend(crate::decide::signing_eligibility::signing_eligibility_candidates(&oriented.state));
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
    CodexObserve(std::io::Error),
    Act(ActError),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observe(e) => write!(f, "observe: {e}"),
            Self::CodexObserve(e) => write!(f, "observe (codex review): {e}"),
            Self::Act(e) => write!(f, "act: {e}"),
        }
    }
}

impl std::error::Error for LoopError {}

pub(crate) struct LoopConfig {
    /// Iteration cap. Carrying it as `NonZeroU32` makes the
    /// "iteration ≥ 1" invariant structural — input validation
    /// happens once at the boundary, and the loop body relies on
    /// the type rather than re-checking.
    pub max_iterations: NonZeroU32,
    /// Codex-axis configuration. `None` makes the axis structurally
    /// absent for this invocation: observe skips the filesystem scan
    /// and orient projects `codex_review = None`, recovering the
    /// non-codex baseline.
    pub codex_review: Option<CodexReviewConfig>,
}

/// Bounds of the codex reasoning-level ladder for this invocation.
/// `floor` and `ceiling` form an inclusive range over the totally
/// ordered ladder; the codex slice in orient walks within those
/// bounds and yields the current ladder action.
#[derive(Debug, Clone)]
pub(crate) struct CodexReviewConfig {
    pub floor: CodexReasoningLevel,
    pub ceiling: CodexReasoningLevel,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: NonZeroU32::new(50).expect("50 is non-zero"),
            codex_review: None,
        }
    }
}

/// One iteration's terminal step. Partitions an iteration's outcome
/// into the two arms the driver dispatches on: an early-halt that
/// short-circuits the loop, or a completed Execute that the driver
/// records as the running last-attempted action.
enum IterStep {
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
    mut ctx: ActContext,
    state_root: Option<&std::path::Path>,
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<LoopExit, LoopError> {
    let max_iter = config.max_iterations.get();
    let codex_cfg = config.codex_review;

    // Boundary signal poll. The handler runs in signal context and
    // only stores into the atomic; the loop owns every side effect
    // (recorder.halt + live-marker release) so the terminal event
    // lands on the same write path as every other halt.
    if let Some(exit_code) = crate::signal::check_shutdown() {
        return Ok(LoopExit::SignalInterrupted { exit_code });
    }
    // First iteration is unrolled: the NonZeroU32 cap guarantees it
    // runs, and a stall comparator does not exist yet.
    let mut last_attempted: Action = match run_iter(
        &mut ctx,
        state_root,
        codex_cfg.as_ref(),
        recorder,
        &mut on_state,
        1,
        None,
    )? {
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

    for iter in 2..=max_iter {
        if let Some(exit_code) = crate::signal::check_shutdown() {
            return Ok(LoopExit::SignalInterrupted { exit_code });
        }
        let step = run_iter(
            &mut ctx,
            state_root,
            codex_cfg.as_ref(),
            recorder,
            &mut on_state,
            iter,
            last_non_wait_key.as_ref(),
        )?;
        match step {
            IterStep::Halt(reason) => return Ok(LoopExit::Halted(reason)),
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
    Ok(LoopExit::Halted(HaltReason::CapReached(last_attempted)))
}

/// One observe → orient → decide → act cycle. Returns an early-halt
/// reason or the action that just executed; observation failures
/// bubble as `LoopError`. The codex observe pass is gated on the
/// per-invocation configuration plus the per-iteration context.
#[allow(clippy::too_many_arguments)]
fn run_iter(
    ctx: &mut ActContext,
    state_root: Option<&std::path::Path>,
    codex_cfg: Option<&CodexReviewConfig>,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey>,
) -> Result<IterStep, LoopError> {
    let slug = ctx.slug.clone();
    let pr = ctx.pr;

    recorder.set_iteration(Some(iter));
    recorder.record_observe_start(iter);
    let sticky_path = recorder.last_seen_head_path();
    let obs = match fetch_all(&slug, pr, state_root, Some(&sticky_path)) {
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
            recorder.record_action_start(iter, &action);
            recorder.record_wait_start(iter, &action);
            let act_result = act(&action, ctx);
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

    // Refresh the codex sub-context with this iteration's head SHA
    // and base branch so the side-effect surface (batch directory,
    // head stamp, codex --base argv) stays anchored on what observe
    // just read. Without this refresh the act stage could write into
    // a stale batch directory or diff against the wrong base.
    let codex_obs: Option<CodexObservations> =
        if let (Some(codex_cfg), Some(codex_ctx)) = (codex_cfg, ctx.codex.as_mut()) {
            codex_ctx.head_sha = obs.pull_request_view.head_ref_oid.as_str().to_string();
            codex_ctx.base_branch = obs.pull_request_view.base_ref_name.as_str().to_string();
            let codex_pr_root = codex_ctx.codex_pr_root.clone();
            let head_sha = codex_ctx.head_sha.clone();
            let expected = codex_ctx.n;
            match fetch_codex(
                &codex_pr_root,
                codex_cfg.floor,
                codex_cfg.ceiling,
                expected,
                &head_sha,
            ) {
                Ok(o) => Some(o),
                Err(e) => return Err(LoopError::CodexObserve(e)),
            }
        } else {
            None
        };

    let now = current_timestamp();
    let oriented = orient(&obs, codex_obs.as_ref(), None, now);
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
            let act_result = act(&action, ctx);
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

    /// Process-global mutex guarding the shutdown atomic for
    /// signal-poll unit tests. The atomic is global; sibling tests
    /// must serialize against `set_for_test` so their boundary
    /// poll doesn't observe a stray signal stored by another.
    static SIGNAL_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn check_shutdown_round_trips_through_atomic() {
        // The loop's signal-poll wraps a SeqCst load on the
        // process-global atomic. This pins the round trip so a
        // refactor that swaps the storage ordering or the wrapper
        // function shape regresses loudly.
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        crate::signal::reset_for_test();
        assert!(crate::signal::check_shutdown().is_none());
        crate::signal::set_for_test(143);
        assert_eq!(crate::signal::check_shutdown(), Some(143));
        crate::signal::set_for_test(130);
        assert_eq!(crate::signal::check_shutdown(), Some(130));
        crate::signal::reset_for_test();
        assert!(crate::signal::check_shutdown().is_none());
    }

    #[test]
    fn loop_exit_variant_carries_signal_exit_code() {
        // `LoopExit::SignalInterrupted` is the wire surface that
        // `main.rs` projects onto `Outcome::SignalInterrupted`.
        // Pin the round trip — the wrapped u8 is the exit code
        // the process returns.
        let exit = LoopExit::SignalInterrupted { exit_code: 143 };
        match exit {
            LoopExit::SignalInterrupted { exit_code } => assert_eq!(exit_code, 143),
            LoopExit::Halted(_) => panic!("expected SignalInterrupted"),
        }
    }
}
