//! OODA loop driver — observe → orient → decide → act → repeat
//! until a halt condition fires.
//!
//! Stall detection: if the same (kind, blocker) pair fires twice
//! in a row, the loop halts Stalled. Coarse — only catches the
//! one-action-spinning case. The iteration cap is the second line
//! of defense and surfaces as `HaltReason::CapReached`.
//!
//! The loop returns `HaltReason` directly — there is no separate
//! "outcome" type. Cap, stall, success, terminal, and handoff are
//! all variants of the same partition. Exit-code mapping lives on
//! `HaltReason::exit_code()`.
//!
//! `LoopConfig::max_iterations` is `NonZeroU32` so iter 1 is
//! structurally guaranteed to run; the driver splits iter 1 from
//! the subsequent iterations so `last_attempted` flows as a typed
//! `Action` (not `Option<Action>`) into the eventual
//! `HaltReason::CapReached` — eliminating the runtime expect that
//! previously documented this invariant.

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

/// Read the wall-clock once per iteration. Axes that need a clock
/// (copilot health, future CI queue-stall) take this as a parameter
/// so behavior under test is deterministic.
pub fn current_timestamp() -> Timestamp {
    let now = chrono::Utc::now().to_rfc3339();
    // `to_rfc3339` always produces a parseable RFC-3339 string;
    // this round-trip cannot fail.
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

pub struct LoopConfig {
    /// Iteration cap. `NonZeroU32` so the driver's "iter 1
    /// always runs" guarantee is structural.
    pub max_iterations: NonZeroU32,
    /// Codex review ladder configuration. `None` disables the
    /// codex review axis entirely (observe skips the filesystem
    /// scan, orient gets `codex_review = None`).
    pub codex_review: Option<CodexReviewConfig>,
}

#[derive(Debug, Clone)]
pub struct CodexReviewConfig {
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

/// One iteration's typed outcome. The loop body produces either
/// an early-halt (`Decision::Halt` or stall-detected) or a completed
/// Execute that we keep as the running "last attempted" anchor.
enum IterStep {
    Halt(HaltReason),
    Executed(Action),
}

/// Drive a PR until a halt fires or the iteration cap trips.
///
/// `on_state` is called once per iteration after decide and before
/// act, with the iteration index, raw observation bundle, oriented
/// state, full candidate set, and chosen decision. Halt decisions
/// also fire it before returning. Use it to render iteration logs,
/// post comments, and record the run bundle.
pub fn run_loop(
    mut ctx: ActContext,
    state_root: Option<&std::path::Path>,
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<HaltReason, LoopError> {
    let max_iter = config.max_iterations.get();
    let codex_cfg = config.codex_review;

    // Iter 1 is guaranteed by NonZeroU32. Stall is structurally
    // impossible there (no prior key).
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
    // Wait is stall-exempt; any axis adding health detection MUST
    // emit a non-Wait action when degraded (see
    // CopilotActivity::Requested(InFlightHealth::Degraded) →
    // Full(RerequestCopilot)). Changing only the Wait's blocker tag
    // is invisible here.
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

    Ok(HaltReason::CapReached(last_attempted))
}

/// Run one full observe (PR + optional codex) → orient → decide →
/// act cycle. Returns either a halt reason or the action executed.
#[allow(clippy::too_many_arguments)]
fn run_iter(
    ctx: &mut ActContext,
    state_root: Option<&std::path::Path>,
    codex_cfg: Option<&CodexReviewConfig>,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey<crate::decide::action::ActionKind>>,
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
            // Rate-limited mid-observe. Synthesize the
            // WaitForRateLimit action and sleep its retry window;
            // the next iteration re-observes from fresh state.
            // No orient/decide call this iteration — every axis
            // would be operating on stale or absent data.
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

    // Refresh the codex side of the act context with this
    // iteration's head SHA and base branch so RunCodexReviewBatch
    // spawns under the correct batch directory, writes
    // head_sha.txt consistently with what observe just read, and
    // points `codex review --base` at the PR's actual base.
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
