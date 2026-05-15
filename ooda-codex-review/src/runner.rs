//! OODA loop driver — observe → orient → decide → act → repeat
//! until a halt condition fires.
//!
//! Stall detection: if the same (kind, blocker) pair fires twice
//! in a row (excluding Wait), the loop halts Stalled. Coarse —
//! only catches the one-action-spinning case. The iteration cap
//! is the second line of defense and surfaces as `HaltReason::CapReached`.
//!
//! The loop returns `HaltReason` directly. Cap, stall, success,
//! terminal, and handoff are all variants of the same partition.
//! Exit-code mapping lives on `HaltReason::exit_code()`.
//!
//! Codex-domain shape: each iteration spawns a fresh `observe`
//! (subprocess fan-out), runs `orient`/`decide`, then `act`s on
//! Full/Wait. Agent/Human halts return up to the caller.

use crate::act::{ActContext, ActError, act};
use crate::decide::action::{Action, ReasoningLevel};
use crate::decide::decide;
use crate::decide::decision::{Decision, HaltReason};
use crate::ids::{RepoId, ReviewTarget};
use crate::observe::codex::CodexObservations;
use crate::orient::OrientedState;
use crate::orient::orient;

#[derive(Debug)]
pub enum LoopError {
    Observe(String),
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
    /// Top of the reasoning ladder. When the loop reaches an
    /// all-clean batch at this level, decide halts with
    /// `Terminal(Succeeded)` (the codex-review fixed point at the
    /// ceiling) instead of emitting a `Retrospective` handoff.
    pub ceiling: ReasoningLevel,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            ceiling: ReasoningLevel::Xhigh,
        }
    }
}

/// Drive a codex-review session until a halt fires or the
/// iteration cap trips.
///
/// `observe` is parameterized: callers supply a closure that
/// fetches the current `CodexObservations` for the configured
/// `(repo_id, target)`. This lets the test harness substitute
/// stub observations without touching subprocesses.
///
/// `on_state` is called once per iteration after decide and
/// before act, with the iteration index, oriented state, and
/// chosen decision. Halt decisions also fire it before returning.
pub fn run_loop(
    repo_id: &RepoId,
    target: &ReviewTarget,
    config: LoopConfig,
    ctx: &ActContext,
    mut observe: impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String>,
    mut on_state: impl FnMut(u32, &OrientedState, &Decision),
) -> Result<HaltReason, LoopError> {
    let mut last_non_wait_key: Option<ooda_core::StallKey<crate::decide::action::ActionKind>> =
        None;
    let mut last_attempted: Option<Action> = None;

    for iter in 1..=config.max_iterations {
        let obs = observe(repo_id, target).map_err(LoopError::Observe)?;
        let oriented = orient(&obs, config.ceiling);
        let decision = decide(&oriented);
        on_state(iter, &oriented, &decision);

        match decision {
            Decision::Halt(halt) => return Ok(HaltReason::Decision(halt)),
            Decision::Execute(action) => {
                let current_key = action.stall_key();
                if last_non_wait_key.as_ref() == Some(&current_key) {
                    return Ok(HaltReason::Stalled(action));
                }
                let is_wait = action.effect.is_wait();
                act(&action, ctx).map_err(LoopError::Act)?;
                last_attempted = Some(action);
                if !is_wait {
                    last_non_wait_key = Some(current_key);
                }
            }
        }
    }

    Ok(HaltReason::CapReached(last_attempted.expect(
        "CapReached requires --max-iter ≥ 1 and one Execute iteration",
    )))
}
