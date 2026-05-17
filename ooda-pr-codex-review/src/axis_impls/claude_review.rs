//! `ClaudeReviewAxis` — Claude-review-attestation lane as an `Axis` impl.

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::OrientedState;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ClaudeReviewObservation<'a> {
    pub oriented: &'a OrientedState,
    pub pr: PullRequestNumber,
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ClaudeReviewAxis;

impl<'a> Axis<ClaudeReviewObservation<'a>> for ClaudeReviewAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &ClaudeReviewObservation<'a>) -> Vec<Action> {
        crate::decide::claude_review::candidates(obs.oriented, obs.pr)
    }
}
