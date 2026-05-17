//! `CloseoutAxis` — pre-handoff sign-off convergence gate as an
//! `Axis` impl.
//!
//! Reads across the entire [`OrientedState`]: closeout fires only
//! when every other axis is quiescent. The candidate emitter
//! lives in `decide::closeout::candidates`; this wrapper is
//! pure delegation.

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::OrientedState;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CloseoutObservation<'a> {
    pub oriented: &'a OrientedState,
    pub pr: PullRequestNumber,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CloseoutAxis;

impl<'a> Axis<CloseoutObservation<'a>> for CloseoutAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CloseoutObservation<'a>) -> Vec<Action> {
        crate::decide::closeout::candidates(obs.oriented, obs.pr)
    }
}
