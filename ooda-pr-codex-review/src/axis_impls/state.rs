//! `StateAxis` — mechanical merge-shape blockers as an `Axis` impl.
//!
//! Wraps `decide::state::blocking_candidates`. Declared deps:
//!
//! - `state` — own projection (the merge-shape lattice).
//! - `threads` — review threads, used by the rebase-prompt
//!   enrichment to surface re-anchoring witnesses.
//! - `merge_base_delta` — base-branch delta, used by the
//!   rebase-prompt enrichment to render conflict-surface guidance.
//!
//! The companion `fallback_merge_state_blocker` is not wrapped
//! here — it is composed alongside the per-axis candidate sets
//! at the driver level (`decide.rs`), not inside this axis.

use crate::decide::action::{Action, ActionKind};
use crate::observe::github::compare::MergeBaseDelta;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::ReviewThread;
use ooda_core::Axis;

/// Per-axis observation slice for [`StateAxis`].
pub(crate) struct StateObservation<'a> {
    pub state: &'a PullRequestProjection,
    pub threads: &'a [ReviewThread],
    pub merge_base_delta: Option<&'a MergeBaseDelta>,
}

/// Wrapper exposing mechanical merge-shape blockers as an [`Axis`] impl.
pub(crate) struct StateAxis;

impl<'a> Axis<StateObservation<'a>> for StateAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &StateObservation<'a>) -> Vec<Action> {
        crate::decide::state::blocking_candidates(obs.state, obs.threads, obs.merge_base_delta)
    }
}
