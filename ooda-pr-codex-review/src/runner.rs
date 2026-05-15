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

use crate::act::{ActContext, ActError, act};
use crate::decide::action::{Action, Automation};
use crate::decide::decision::{Decision, HaltReason};
use crate::decide::{candidates, decide_from_candidates};
use crate::ids::ReasoningLevel;
use crate::observe::codex::{CodexObservations, fetch_all as fetch_codex};
use crate::observe::github::GitHubObservations;
use crate::observe::github::fetch_all;
use crate::observe::github::gh::GhError;
use crate::orient::OrientedState;
use crate::orient::orient;
use crate::recorder::Recorder;

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
    pub max_iterations: u32,
    /// Codex review ladder configuration. `None` disables the
    /// codex review axis entirely (observe skips the filesystem
    /// scan, orient gets `codex_review = None`).
    pub codex_review: Option<CodexReviewConfig>,
}

#[derive(Debug, Clone)]
pub struct CodexReviewConfig {
    pub floor: ReasoningLevel,
    pub ceiling: ReasoningLevel,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            codex_review: None,
        }
    }
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
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<HaltReason, LoopError> {
    let slug = ctx.slug.clone();
    let pr = ctx.pr;
    // Two trackers, two purposes:
    //   * `last_non_wait_key`: feeds the stall comparator. Polling
    //     (Wait) is expected to repeat — only same-(kind, blocker)
    //     non-Wait actions firing twice in a row trip Stalled.
    //     Storing only non-Wait keys makes "Wait is invisible to
    //     stall detection" structural rather than checked at
    //     comparison time. The StallKey<K> newtype makes the
    //     "compare on (kind, blocker)" rule the type, not a
    //     comment.
    //   * `last_attempted`: feeds CapReached's diagnostic payload.
    //     Includes Wait actions — "we ran out of cap while waiting
    //     for CI" is a useful triage signal.
    let mut last_non_wait_key: Option<ooda_core::StallKey<crate::decide::action::ActionKind>> =
        None;
    let mut last_attempted: Option<Action> = None;

    for iter in 1..=config.max_iterations {
        recorder.set_iteration(Some(iter));
        recorder.record_observe_start(iter);
        let obs = match fetch_all(&slug, pr) {
            Ok(obs) => {
                recorder.record_observe_end(iter, Ok(()));
                obs
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
        let codex_obs: Option<CodexObservations> = if let (Some(codex_cfg), Some(codex_ctx)) =
            (config.codex_review.as_ref(), ctx.codex.as_mut())
        {
            codex_ctx.head_sha = obs.pr_view.head_ref_oid.as_str().to_string();
            codex_ctx.base_branch = obs.pr_view.base_ref_name.as_str().to_string();
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

        let oriented = orient(&obs, codex_obs.as_ref(), None);
        let candidates = candidates(&oriented);
        let decision = decide_from_candidates(candidates.clone(), obs.pr_view.state);
        on_state(iter, &obs, &oriented, &candidates, &decision);

        match decision {
            Decision::Halt(halt) => return Ok(HaltReason::Decision(halt)),
            Decision::Execute(action) => {
                // Stall check BEFORE act so a side-effecting Full
                // action (e.g. RerequestCopilot) doesn't fire twice
                // when GitHub's eventual consistency hasn't surfaced
                // the previous call yet. Comparison is on the
                // typed StallKey<K> — equality of (kind, blocker)
                // alone IS the stall test.
                let current_key = action.stall_key();
                if last_non_wait_key.as_ref() == Some(&current_key) {
                    return Ok(HaltReason::Stalled(action));
                }
                let is_wait = matches!(action.automation, Automation::Wait { .. });
                recorder.record_action_start(iter, &action);
                if is_wait {
                    recorder.record_wait_start(iter, &action);
                }
                let act_result = act(&action, &ctx);
                if is_wait && act_result.is_ok() {
                    recorder.record_wait_end(iter, &action);
                }
                recorder.record_action_end(
                    iter,
                    &action,
                    act_result.as_ref().map(|_| ()).map_err(ToString::to_string),
                );
                act_result.map_err(LoopError::Act)?;
                last_attempted = Some(action);
                if !is_wait {
                    last_non_wait_key = Some(current_key);
                }
            }
        }
    }

    // CapReached fires only when the for-loop completes without
    // an early return, which requires every iteration to have run
    // an Execute (Halt returns early; Stalled returns early; Act
    // failure returns early). With --max-iter ≥ 1 (parser-validated),
    // at least one Execute fired and `last_attempted` is Some(_).
    Ok(HaltReason::CapReached(last_attempted.expect(
        "CapReached requires --max-iter ≥ 1 and one Execute iteration",
    )))
}
