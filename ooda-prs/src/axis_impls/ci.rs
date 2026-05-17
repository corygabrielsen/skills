//! `CiAxis` — `Axis` impl wrapping the CI lane.
//!
//! Declared deps: own CI report. Projection lives in the
//! driver (via `orient_ci`); the wrapper is a 1-line delegation
//! to `decide::ci::candidates`. Same shape as the convergence
//! wrappers (state, reviews, attestations, closeout): take
//! pre-projected refs, emit candidates.

use crate::decide::action::{Action, ActionKind};
use crate::orient::ci::CiReport;
use ooda_core::Axis;

/// Per-axis observation slice for [`CiAxis`].
pub(crate) struct CiObservation<'a> {
    pub report: &'a CiReport,
}

/// Wrapper exposing CI as an [`Axis`] impl.
pub(crate) struct CiAxis;

impl<'a> Axis<CiObservation<'a>> for CiAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CiObservation<'a>) -> Vec<Action> {
        crate::decide::ci::candidates(obs.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orient::ci::{CheckBucket, CiActivity, CiSummary, ResolvedState};

    fn clean_ci() -> CiReport {
        CiReport {
            summary: CiSummary {
                required: CheckBucket::default(),
                missing_names: vec![],
                completed_at: None,
                advisory: CheckBucket::default(),
            },
            activity: CiActivity::Resolved(ResolvedState::AllGreen),
        }
    }

    #[test]
    fn ci_axis_produces_no_candidates_on_idle() {
        let axis = CiAxis;
        let report = clean_ci();
        let obs = CiObservation { report: &report };
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "idle CI should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
