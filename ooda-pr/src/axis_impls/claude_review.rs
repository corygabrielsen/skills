//! `ClaudeReviewAxis` — Claude-review-attestation lane as an `Axis` impl.
//!
//! Declared deps: own review report + attest-path location + PR
//! number (for prompt rendering).

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::claude_review::ClaudeReview;
use ooda_core::Axis;

pub(crate) struct ClaudeReviewObservation<'a> {
    pub claude_review: &'a ClaudeReview,
    pub attest_path: Option<&'a std::path::Path>,
    pub pr: PullRequestNumber,
}

pub(crate) struct ClaudeReviewAxis;

impl<'a> Axis<ClaudeReviewObservation<'a>> for ClaudeReviewAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &ClaudeReviewObservation<'a>) -> Vec<Action> {
        crate::decide::claude_review::candidates(obs.claude_review, obs.attest_path, obs.pr)
    }
}
