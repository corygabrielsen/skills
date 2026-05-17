//! `ReviewsAxis` — generic-reviewer lane as an `Axis` impl.
//!
//! Convergence axis: `decide::reviews::candidates` reads across
//! the whole [`OrientedState`] to coordinate human-reviewer
//! requests against bot reviewer state. See sibling-module doc
//! ([`super`]) for the projection-vs-convergence split.

use crate::decide::action::{Action, ActionKind};
use crate::orient::OrientedState;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ReviewsObservation<'a> {
    pub oriented: &'a OrientedState,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ReviewsAxis;

impl<'a> Axis<ReviewsObservation<'a>> for ReviewsAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &ReviewsObservation<'a>) -> Vec<Action> {
        crate::decide::reviews::candidates(obs.oriented)
    }
}
