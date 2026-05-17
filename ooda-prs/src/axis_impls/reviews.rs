//! `ReviewsAxis` — generic-reviewer lane as an `Axis` impl.
//!
//! Declared deps: own review report + CI report (for the
//! `ci_clean` approval gate) + bot-review-axis presence (for
//! the bot-review shadow filter) + threads (for the
//! `threads_clean` approval gate).

use crate::decide::action::{Action, ActionKind};
use crate::orient::ci::CiReport;
use crate::orient::copilot::CopilotReport;
use crate::orient::reviews::ReviewSummary;
use crate::orient::thread::ReviewThread;
use ooda_core::Axis;

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ReviewsObservation<'a> {
    pub reviews: &'a ReviewSummary,
    pub ci: &'a CiReport,
    pub copilot: Option<&'a CopilotReport>,
    pub threads: &'a [ReviewThread],
}

#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct ReviewsAxis;

impl<'a> Axis<ReviewsObservation<'a>> for ReviewsAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &ReviewsObservation<'a>) -> Vec<Action> {
        crate::decide::reviews::candidates(obs.reviews, obs.ci, obs.copilot, obs.threads)
    }
}
