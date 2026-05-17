//! `CopilotAxis` — `Axis` impl wrapping the Copilot bot-review lane.
//!
//! Declared deps: own report (Option to carry both "no policy
//! configured" and "policy configured but disabled" absences;
//! both produced by the projection layer).

use crate::decide::action::{Action, ActionKind};
use crate::orient::copilot::CopilotReport;
use ooda_core::Axis;

/// Per-axis observation slice for [`CopilotAxis`].
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CopilotObservation<'a> {
    pub report: Option<&'a CopilotReport>,
}

/// Wrapper exposing Copilot as an [`Axis`] impl.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CopilotAxis;

impl<'a> Axis<CopilotObservation<'a>> for CopilotAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CopilotObservation<'a>) -> Vec<Action> {
        match obs.report {
            Some(r) => crate::decide::copilot::candidates(r),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_axis_emits_no_candidates_when_no_report() {
        let axis = CopilotAxis;
        let obs = CopilotObservation { report: None };
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "absent report should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
