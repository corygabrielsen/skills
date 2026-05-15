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
//!
//! `LoopConfig::max_iterations` is `NonZeroU32` so iter 1 is
//! structurally guaranteed to run; the driver splits iter 1 from
//! the subsequent iterations so `last_attempted` flows as a typed
//! `Action` (not `Option<Action>`) into the eventual
//! `HaltReason::CapReached` — eliminating the runtime expect that
//! previously documented this invariant.

use std::num::NonZeroU32;

use crate::act::{ActContext, ActError, act};
use crate::decide::action::{Action, CodexReasoningLevel};
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
    /// Iteration cap. `NonZeroU32` so the driver's "iter 1
    /// always runs" guarantee is structural.
    pub max_iterations: NonZeroU32,
    /// Top of the reasoning ladder. When the loop reaches an
    /// all-clean batch at this level, decide halts with
    /// `Terminal(Succeeded)` (the codex-review fixed point at the
    /// ceiling) instead of emitting a `Retrospective` handoff.
    pub ceiling: CodexReasoningLevel,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: NonZeroU32::new(50).expect("50 is non-zero"),
            ceiling: CodexReasoningLevel::Xhigh,
        }
    }
}

/// One iteration's typed outcome. The loop body produces either
/// an early-halt (Decision::Halt or stall-detected) or a completed
/// Execute that we keep as the running "last attempted" anchor.
enum IterStep {
    Halt(HaltReason),
    Executed(Action),
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
    let max_iter = config.max_iterations.get();

    // Iter 1 is guaranteed to run; stall is structurally impossible
    // there (no prior key). Run it explicitly so `last_attempted` is
    // initialized as a typed `Action`.
    let mut last_attempted: Action = match run_iter(
        repo_id,
        target,
        config.ceiling,
        ctx,
        &mut observe,
        &mut on_state,
        1,
        None,
    )? {
        IterStep::Halt(reason) => return Ok(reason),
        IterStep::Executed(action) => action,
    };
    let mut last_non_wait_key = if last_attempted.effect.is_wait() {
        None
    } else {
        Some(last_attempted.stall_key())
    };

    for iter in 2..=max_iter {
        let step = run_iter(
            repo_id,
            target,
            config.ceiling,
            ctx,
            &mut observe,
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

/// Run one full observe → orient → decide → act cycle.
#[allow(clippy::too_many_arguments)]
fn run_iter(
    repo_id: &RepoId,
    target: &ReviewTarget,
    ceiling: CodexReasoningLevel,
    ctx: &ActContext,
    mut observe: impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String>,
    mut on_state: impl FnMut(u32, &OrientedState, &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey<crate::decide::action::ActionKind>>,
) -> Result<IterStep, LoopError> {
    let obs = observe(repo_id, target).map_err(LoopError::Observe)?;
    let oriented = orient(&obs, ceiling);
    let decision = decide(&oriented);
    on_state(iter, &oriented, &decision);

    match decision {
        Decision::Halt(halt) => Ok(IterStep::Halt(HaltReason::Decision(halt))),
        Decision::Execute(action) => {
            let current_key = action.stall_key();
            if last_non_wait_key == Some(&current_key) {
                return Ok(IterStep::Halt(HaltReason::Stalled(action)));
            }
            act(&action, ctx).map_err(LoopError::Act)?;
            Ok(IterStep::Executed(action))
        }
    }
}
