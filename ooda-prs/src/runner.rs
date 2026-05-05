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

use crate::act::{ActError, act};
use crate::decide::action::{Action, Automation};
use crate::decide::decision::{Decision, HaltReason};
use crate::decide::{candidates, decide_from_candidates};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::GitHubObservations;
use crate::observe::github::fetch_all;
use crate::observe::github::gh::GhError;
use crate::orient::OrientedState;
use crate::orient::orient;
use crate::recorder::Recorder;

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

pub struct LoopConfig {
    pub max_iterations: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self { max_iterations: 50 }
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
    slug: &RepoSlug,
    pr: PullRequestNumber,
    config: LoopConfig,
    recorder: &Recorder,
    mut on_state: impl FnMut(u32, &GitHubObservations, &OrientedState, &[Action], &Decision),
) -> Result<HaltReason, LoopError> {
    // Two trackers, two purposes:
    //   * `last_non_wait`: feeds the stall comparator. Polling
    //     (Wait) is expected to repeat — only same-(kind, blocker)
    //     non-Wait actions firing twice in a row trip Stalled.
    //     Storing only non-Wait actions makes "Wait is invisible
    //     to stall detection" structural rather than checked at
    //     comparison time.
    //   * `last_attempted`: feeds CapReached's diagnostic payload.
    //     Includes Wait actions — "we ran out of cap while waiting
    //     for CI" is a useful triage signal.
    let mut last_non_wait: Option<Action> = None;
    let mut last_attempted: Option<Action> = None;

    for iter in 1..=config.max_iterations {
        recorder.set_iteration(Some(iter));
        recorder.record_observe_start(iter);
        let obs = match fetch_all(slug, pr) {
            Ok(obs) => {
                recorder.record_observe_end(iter, Ok(()));
                obs
            }
            Err(e) => {
                recorder.record_observe_end(iter, Err(e.to_string()));
                return Err(LoopError::Observe(e));
            }
        };
        let oriented = orient(&obs, None);
        let candidates = candidates(&oriented);
        let decision = decide_from_candidates(candidates.clone(), obs.pr_view.state);
        on_state(iter, &obs, &oriented, &candidates, &decision);

        match decision {
            Decision::Halt(halt) => return Ok(HaltReason::Decision(halt)),
            Decision::Execute(action) => {
                // Stall check BEFORE act so a side-effecting Full
                // action (e.g. RerequestCopilot) doesn't fire twice
                // when GitHub's eventual consistency hasn't surfaced
                // the previous call yet.
                if same_action_repeated(last_non_wait.as_ref(), &action) {
                    return Ok(HaltReason::Stalled(action));
                }
                let is_wait = matches!(action.automation, Automation::Wait { .. });
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
                    act_result.as_ref().map(|_| ()).map_err(ToString::to_string),
                );
                act_result.map_err(LoopError::Act)?;
                last_attempted = Some(action.clone());
                if !is_wait {
                    last_non_wait = Some(action);
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

fn same_action_repeated(prev: Option<&Action>, current: &Action) -> bool {
    // `prev` is structurally non-Wait (runner skips Wait actions
    // when assigning last_action). So we compare directly without
    // a current=Wait gate. A Wait current vs non-Wait prev cannot
    // satisfy kind equality (kinds are partitioned by automation
    // intent), so the comparison naturally returns false.
    prev.is_some_and(|p| p.kind == current.kind && p.blocker == current.blocker)
}
