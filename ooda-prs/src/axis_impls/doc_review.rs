//! `DocReviewAxis` — doc-review-attestation lane as an `Axis` impl.
//!
//! Declared deps: state projection (for commit-count gate) + own
//! review report + attest-path location + PR number (for prompt
//! rendering).

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::doc_review::DocReview;
use crate::orient::state::PullRequestProjection;
use ooda_core::Axis;

pub(crate) struct DocReviewObservation<'a> {
    pub state: &'a PullRequestProjection,
    pub doc_review: &'a DocReview,
    pub attest_path: Option<&'a std::path::Path>,
    pub pr: PullRequestNumber,
}

pub(crate) struct DocReviewAxis;

impl<'a> Axis<DocReviewObservation<'a>> for DocReviewAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &DocReviewObservation<'a>) -> Vec<Action> {
        crate::decide::doc_review::candidates(obs.state, obs.doc_review, obs.attest_path, obs.pr)
    }
}
