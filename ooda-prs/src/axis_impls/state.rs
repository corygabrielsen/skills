//! `StateAxis` — mechanical merge-shape blockers as an `Axis` impl.
//!
//! Wraps `decide::state::blocking_candidates`. Unlike the health
//! axes (CI / Cursor / Copilot), this axis is a **convergence**
//! axis: its candidate emitter reads across the whole
//! [`OrientedState`] bundle, not just its own per-axis report,
//! because merge-shape blockers cross-reference multiple axes
//! (draft, WIP label, mergeability, base-branch deltas, etc.).
//! The wrapper therefore takes a reference to the full oriented
//! bundle as its observation.
//!
//! The companion `fallback_merge_state_blocker` is not wrapped
//! here — it is composed alongside the per-axis candidate sets at
//! the driver level (`decide.rs`), not inside this axis.

use crate::decide::action::{Action, ActionKind};
use crate::orient::OrientedState;
use ooda_core::Axis;

/// Per-axis observation slice for [`StateAxis`].
///
/// Carries the whole oriented bundle: blocking candidates may
/// cross-reference any axis. Specific fields read are documented
/// inside `decide::state::blocking_candidates`.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct StateObservation<'a> {
    pub oriented: &'a OrientedState,
}

/// Wrapper exposing mechanical merge-shape blockers as an [`Axis`] impl.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct StateAxis;

impl<'a> Axis<StateObservation<'a>> for StateAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &StateObservation<'a>) -> Vec<Action> {
        crate::decide::state::blocking_candidates(obs.oriented)
    }
}
