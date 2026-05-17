//! `DocReviewAxis` — doc-review-attestation lane as an `Axis` impl.

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::OrientedState;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct DocReviewObservation<'a> {
    pub oriented: &'a OrientedState,
    pub pr: PullRequestNumber,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct DocReviewAxis;

impl<'a> Axis<DocReviewObservation<'a>> for DocReviewAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &DocReviewObservation<'a>) -> Vec<Action> {
        crate::decide::doc_review::candidates(obs.oriented, obs.pr)
    }
}
