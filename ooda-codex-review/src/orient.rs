//! Orient stage: typed view over [`CodexObservations`] for the
//! decide layer.
//!
//! Forwarding layer that copies the observation fields the state
//! machine needs.

use serde::Serialize;

use crate::decide::action::CodexReasoningLevel;
use crate::observe::codex::CodexObservations;
use crate::observe::codex::batch::BatchState;

/// Inputs decide consumes. Pure data — no methods on this struct
/// have any policy.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OrientedState {
    /// Reasoning level the current batch is running at.
    pub current_level: CodexReasoningLevel,
    /// Top of the configured ladder. When `current_level == ceiling`
    /// AND the batch is `Complete { all_clean }`, decide halts with
    /// `Terminal(Succeeded)` (the codex-review fixed point at the
    /// ceiling) instead of emitting a `Retrospective` handoff.
    pub ceiling: CodexReasoningLevel,
    /// Filesystem-derived batch state (`NotStarted` / Running /
    /// Complete) for `current_level`.
    pub batch_state: BatchState,
    /// Configured `n` — how many reviews `RunReviews` was
    /// dispatched to spawn for this batch. Decide reads this to
    /// compute `pending` and to construct `RunReviews { n }`.
    pub expected: u32,
}

/// Build an `OrientedState` from raw observations + the ceiling
/// the invocation was configured with. `ceiling` lives outside the
/// observation because it's a CLI/orchestrator-time configuration
/// value, not a filesystem-derived signal.
pub(crate) fn orient(obs: &CodexObservations, ceiling: CodexReasoningLevel) -> OrientedState {
    OrientedState {
        current_level: obs.current_level,
        ceiling,
        batch_state: obs.batch_state.clone(),
        expected: obs.expected,
    }
}
