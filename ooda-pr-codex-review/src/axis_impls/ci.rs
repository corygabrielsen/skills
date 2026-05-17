//! `CiAxis` — canonical first impl of [`ooda_core::Axis`].
//!
//! Validates the trait shape against real PR-domain code. The
//! existing `orient/ci.rs` and `decide/ci.rs` modules retain their
//! free-function shape; this file is a thin wrapper that exposes
//! them as a single [`ooda_core::Axis`] impl.
//!
//! # Why this shape (per [[project-axis-trait-next-steps]])
//!
//! - **Per-axis [`CiObservation`]**: the axis declares its input
//!   slice explicitly, including cross-axis dependencies
//!   (`has_open_parent_pr` reads through from the state axis's
//!   `PullRequestProjection`). The driver constructs `CiObservation`
//!   from the global `GitHubObservations` plus the state-axis's
//!   report, in topological order.
//!
//! - **No new logic**: `project` calls the existing `orient_ci`;
//!   `candidates` calls the existing `decide::ci::candidates`. The
//!   refactor lifts the surface, not the contents.

use crate::decide::action::{Action, ActionKind};
use crate::ids::{CheckName, GitCommitSha, Timestamp};
use crate::observe::github::checks::PullRequestCheck;
use crate::observe::github::workflow_runs::WorkflowRun;
use crate::orient::ci::{CiReport, orient_ci};
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

/// Wrapper exposing CI's project + candidates as an [`Axis`] impl.
///
/// Zero-sized; the axis carries no per-instance state. Per-tick
/// state lives in the projected [`CiReport`].
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CiAxis;

impl<'a> Axis<CiObservation<'a>> for CiAxis {
    type Report = CiReport;
    type ActionKind = ActionKind;

    fn project(&self, obs: &CiObservation<'a>) -> Self::Report {
        orient_ci(
            obs.checks,
            obs.required,
            obs.has_open_parent_pr,
            obs.workflow_runs,
            obs.head,
            obs.now,
        )
    }

    fn candidates(&self, report: &Self::Report) -> Vec<Action> {
        crate::decide::ci::candidates(report)
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
        let report = axis.project(&obs);
        let candidates = axis.candidates(&report);
        assert!(
            candidates.is_empty(),
            "idle CI should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
