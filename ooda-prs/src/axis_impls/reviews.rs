//! `ReviewsAxis` — generic-reviewer lane as an `Axis` impl.
//!
//! Declared deps: own review report + CI report (for the
//! `ci_clean` approval gate) + bot-review-axis presence (for
//! the bot-review shadow filter) + threads (for the
//! `threads_clean` approval gate) + review-class attestation
//! report and path (for the class-sweep gate) + PR number (for
//! prompt rendering).

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::ci::CiReport;
use crate::orient::copilot::CopilotReport;
use crate::orient::review_class::ReviewClass;
use crate::orient::reviews::ReviewSummary;
use crate::orient::thread::ReviewThread;
use ooda_core::Axis;

pub(crate) struct ReviewsObservation<'a> {
    pub reviews: &'a ReviewSummary,
    pub ci: &'a CiReport,
    pub copilot: Option<&'a CopilotReport>,
    pub threads: &'a [ReviewThread],
    pub review_class: &'a ReviewClass,
    pub review_class_attest_path: Option<&'a std::path::Path>,
    pub pr: PullRequestNumber,
}

pub(crate) struct ReviewsAxis;

impl<'a> Axis<ReviewsObservation<'a>> for ReviewsAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &ReviewsObservation<'a>) -> Vec<Action> {
        crate::decide::reviews::candidates(
            obs.reviews,
            obs.ci,
            obs.copilot,
            obs.threads,
            obs.review_class,
            obs.review_class_attest_path,
            obs.pr,
        )
    }
}
