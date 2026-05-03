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

use crate::act::{act, ActError};
use crate::decide::action::{Action, Automation};
use crate::decide::decision::{Decision, HaltReason};
use crate::decide::decide;
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::fetch_all;
use crate::observe::github::gh::GhError;
use crate::orient::orient;
use crate::orient::OrientedState;

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
/// act, with the iteration index, oriented state, and the chosen
/// decision. Halt decisions also fire it before returning. Use it
/// to render iteration logs, post comments, etc.
pub fn run_loop(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    config: LoopConfig,
    mut on_state: impl FnMut(u32, &OrientedState, &Decision),
) -> Result<HaltReason, LoopError> {
    let mut last_action: Option<Action> = None;

    for iter in 1..=config.max_iterations {
        let obs = fetch_all(slug, pr).map_err(LoopError::Observe)?;
        let oriented = orient(&obs, None);
        let decision = decide(&oriented, obs.pr_view.state);
        on_state(iter, &oriented, &decision);

        match decision {
            Decision::Halt(halt) => return Ok(HaltReason::Decision(halt)),
            Decision::Execute(action) => {
                // Stall check BEFORE act so a side-effecting Full
                // action (e.g. RerequestCopilot) doesn't fire twice
                // when GitHub's eventual consistency hasn't surfaced
                // the previous call yet.
                if same_action_repeated(last_action.as_ref(), &action) {
                    return Ok(HaltReason::Stalled);
                }
                act(&action, slug, pr).map_err(LoopError::Act)?;
                last_action = Some(action);
            }
        }
    }

    Ok(HaltReason::CapReached { last_action })
}

fn same_action_repeated(prev: Option<&Action>, current: &Action) -> bool {
    // Wait actions are *expected* to repeat — they're polling
    // external state. CI / bot / human reviews can take many
    // poll cycles. Only treat non-Wait actions as stall-eligible:
    // a Full or Agent action that fires twice means our act
    // didn't change observable state.
    if matches!(current.automation, Automation::Wait { .. }) {
        return false;
    }
    prev.is_some_and(|p| p.kind == current.kind && p.blocker == current.blocker)
}
