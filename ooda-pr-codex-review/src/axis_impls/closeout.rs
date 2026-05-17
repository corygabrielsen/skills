//! `CloseoutAxis` — pre-handoff sign-off convergence gate as an
//! `Axis` impl.
//!
//! Declared deps: own report + attest-path location + PR number
//! (for prompt rendering).

use crate::decide::action::{Action, ActionKind};
use crate::ids::PullRequestNumber;
use crate::orient::closeout::Closeout;
use ooda_core::Axis;

pub(crate) struct CloseoutObservation<'a> {
    pub closeout: &'a Closeout,
    pub attest_path: Option<&'a std::path::Path>,
    pub pr: PullRequestNumber,
}

pub(crate) struct CloseoutAxis;

impl<'a> Axis<CloseoutObservation<'a>> for CloseoutAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CloseoutObservation<'a>) -> Vec<Action> {
        crate::decide::closeout::candidates(obs.closeout, obs.attest_path, obs.pr)
    }
}
