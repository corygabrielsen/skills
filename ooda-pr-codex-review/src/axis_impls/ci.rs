//! `CiAxis` — canonical first impl of [`ooda_core::Axis`].
//!
//! Wraps the existing `orient/ci.rs` projection and
//! `decide/ci.rs` candidate emitter behind a single trait method.
//! Projection (`orient_ci`) survives as a private helper called
//! inside [`Axis::candidates`]; it is no longer part of the trait
//! contract (see `ooda_core::axis` module doc).
//!
//! # Per-axis observation
//!
//! [`CiObservation`] names every input CI reads, including
//! cross-axis dependencies (`has_open_parent_pr` reads through
//! from the state axis's `PullRequestProjection`). The driver
//! constructs `CiObservation` from the global `GitHubObservations`
//! plus the state-axis report in topological order.

use crate::decide::action::{Action, ActionKind};
use crate::ids::{CheckName, GitCommitSha, Timestamp};
use crate::observe::github::checks::PullRequestCheck;
use crate::observe::github::workflow_runs::WorkflowRun;
use crate::orient::ci::orient_ci;
use ooda_core::Axis;

/// Per-axis observation slice for [`CiAxis`].
///
/// `has_open_parent_pr` is the only cross-axis field; it threads
/// through from the state axis's `PullRequestProjection`.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CiObservation<'a> {
    pub checks: &'a [PullRequestCheck],
    pub required: &'a [CheckName],
    pub has_open_parent_pr: bool,
    pub workflow_runs: &'a [WorkflowRun],
    pub head: &'a GitCommitSha,
    pub now: Timestamp,
}

/// Wrapper exposing CI as an [`Axis`] impl. Zero-sized; per-tick
/// state lives in the report computed inside [`Axis::candidates`].
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CiAxis;

impl<'a> Axis<CiObservation<'a>> for CiAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CiObservation<'a>) -> Vec<Action> {
        let report = orient_ci(
            obs.checks,
            obs.required,
            obs.has_open_parent_pr,
            obs.workflow_runs,
            obs.head,
            obs.now,
        );
        crate::decide::ci::candidates(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CheckName;

    #[test]
    fn ci_axis_produces_no_candidates_on_idle() {
        let axis = CiAxis;
        let required: Vec<CheckName> = vec![];
        let obs = CiObservation {
            checks: &[],
            required: &required,
            has_open_parent_pr: false,
            workflow_runs: &[],
            head: &GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            now: Timestamp::parse("2026-05-17T12:00:00Z").unwrap(),
        };
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "idle CI should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
