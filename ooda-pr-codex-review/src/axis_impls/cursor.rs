//! `CursorAxis` — `Axis` impl wrapping the Cursor bot-review lane.
//!
//! Declared deps: own report (Option to carry the "no signal"
//! vs `NotApplicable` distinction from `orient_cursor`). Wrapper
//! is a 1-line delegation; projection lives in the driver.

use crate::decide::action::{Action, ActionKind};
use crate::orient::cursor::CursorReport;
use ooda_core::Axis;

/// Per-axis observation slice for [`CursorAxis`].
///
/// `report` is `Option` because `orient_cursor` returns `None`
/// when no observation source contributes anything — that's
/// distinct from a present-but-NotApplicable report.
pub(crate) struct CursorObservation<'a> {
    pub report: Option<&'a CursorReport>,
}

/// Wrapper exposing Cursor as an [`Axis`] impl.
pub(crate) struct CursorAxis;

impl<'a> Axis<CursorObservation<'a>> for CursorAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CursorObservation<'a>) -> Vec<Action> {
        match obs.report {
            Some(r) => crate::decide::cursor::candidates(r),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_axis_emits_no_candidates_when_no_observation() {
        let axis = CursorAxis;
        let obs = CursorObservation { report: None };
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "no signal should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
