//! `PullRequestMetadataAxis` — metadata-attestation lane as an `Axis` impl.
//!
//! Convergence axis that also carries an external identity
//! (the PR number) used to key the attestation witness path.

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::OrientedState;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct PullRequestMetadataObservation<'a> {
    pub oriented: &'a OrientedState,
    pub pr: PullRequestNumber,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct PullRequestMetadataAxis;

impl<'a> Axis<PullRequestMetadataObservation<'a>> for PullRequestMetadataAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &PullRequestMetadataObservation<'a>) -> Vec<Action> {
        crate::decide::pull_request_metadata::candidates(obs.oriented, obs.pr)
    }
}
