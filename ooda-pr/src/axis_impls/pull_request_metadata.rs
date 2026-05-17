//! `PullRequestMetadataAxis` — metadata-attestation lane as an `Axis` impl.
//!
//! Declared deps: state projection (for commit-count gate) + own
//! metadata report + attest-path location + PR number (for prompt
//! rendering).

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use crate::orient::state::PullRequestProjection;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct PullRequestMetadataObservation<'a> {
    pub state: &'a PullRequestProjection,
    pub pull_request_metadata: &'a PullRequestMetadata,
    pub attest_path: Option<&'a std::path::Path>,
    pub pr: PullRequestNumber,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct PullRequestMetadataAxis;

impl<'a> Axis<PullRequestMetadataObservation<'a>> for PullRequestMetadataAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &PullRequestMetadataObservation<'a>) -> Vec<Action> {
        crate::decide::pull_request_metadata::candidates(
            obs.state,
            obs.pull_request_metadata,
            obs.attest_path,
            obs.pr,
        )
    }
}
