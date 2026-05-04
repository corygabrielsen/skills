//! Orient stage: typed view over [`CodexObservations`] for the
//! decide layer.
//!
//! Phase 6 surface: just a forwarding layer that copies the
//! observation fields the state machine needs. Cross-iteration
//! state (level history, last test run) lands here when the
//! recorder is wired (Phase 8).

use serde::Serialize;

use crate::decide::action::ReasoningLevel;
use crate::observe::codex::CodexObservations;
use crate::observe::codex::batch::BatchState;

/// Inputs decide consumes. Pure data — no methods on this struct
/// have any policy.
#[derive(Debug, Clone, Serialize)]
pub struct OrientedState {
    /// Reasoning level the current batch is running at.
    pub current_level: ReasoningLevel,
    /// Top of the configured ladder. When `current_level == ceiling`
    /// AND the batch is `Complete { all_clean }`, decide halts with
    /// `Terminal(FixedPoint)` instead of emitting a `Retrospective`
    /// handoff.
    pub ceiling: ReasoningLevel,
    /// Filesystem-derived batch state (NotStarted / Running /
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
pub fn orient(obs: &CodexObservations, ceiling: ReasoningLevel) -> OrientedState {
    OrientedState {
        current_level: obs.current_level,
        ceiling,
        batch_state: obs.batch_state.clone(),
        expected: obs.expected,
    }
}
