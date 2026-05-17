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
use crate::decide::action::{Action, rate_limit_wait_action};
use crate::decide::candidates;
use crate::decide::decision::{Decision, HaltReason};
use crate::ids::{CodexReasoningLevel, Timestamp};
use crate::observe::codex::{CodexObservations, fetch_all as fetch_codex};
use crate::observe::github::gh::GhError;
use crate::observe::github::{FetchOutcome, GitHubObservations, fetch_all};
use crate::orient::OrientedState;
use crate::orient::orient;
use crate::recorder::Recorder;
use ooda_core::decide_from_candidates;

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
) -> Result<HaltReason, LoopError> {
    let max_iter = config.max_iterations.get();
    let codex_cfg = config.codex_review;

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
    let obs = match fetch_all(&slug, pr, state_root) {
        Ok(FetchOutcome::Observations(obs)) => {
            recorder.record_observe_end(iter, Ok(()));
            *obs
        }
        Ok(FetchOutcome::RateLimited(hit)) => {
            // Observe was throttled before all axes could be seen.
            // The only valid response is to wait the upstream's
            // retry window and re-observe from a clean slate;
            // running orient/decide on a partial bundle would
            // surface false halts.
            recorder.record_observe_end(iter, Ok(()));
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
            recorder.record_observe_end(iter, Err(e.to_string()));
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
    let candidates = candidates(&oriented, pr);
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
