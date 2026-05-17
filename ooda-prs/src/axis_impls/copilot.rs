//! `CopilotAxis` — `Axis` impl wrapping the Copilot bot-review lane.
//!
//! Same shape as [`super::cursor::CursorAxis`]: `Report =
//! Option<CopilotReport>`. The Option carries two distinct
//! absences:
//!
//! - **No policy**: `config` is `None` — the repo has no Copilot
//!   reviewer configured. The wrapper short-circuits to `None`
//!   without calling `orient_copilot`, mirroring the call-site
//!   shape (`obs.copilot_config.map(...).and_then(...)`).
//! - **Disabled policy**: `config` is `Some` but
//!   `config.enabled` is false. `orient_copilot` itself returns
//!   `None` in that case.
//!
//! Either way the candidate set is empty.
//!
//! # Cross-axis deps
//!
//! None. Every field reads from the global observation slice.

use crate::decide::action::{Action, ActionKind};
use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::issue_events::IssueEvent;
use crate::observe::github::pull_request_view::Commit;
use crate::observe::github::requested_reviewers::RequestedReviewers;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::orient::copilot::{CopilotRepoConfig, CopilotReport, orient_copilot};
use ooda_core::Axis;

/// Per-axis observation slice for [`CopilotAxis`].
///
/// `config: Option<CopilotRepoConfig>` carries the "no Copilot
/// policy configured at all" case; an enabled-but-dormant policy
/// is `Some` here and `orient_copilot` returns `None` from it.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CopilotObservation<'a> {
    pub config: Option<CopilotRepoConfig>,
    pub events: &'a [IssueEvent],
    pub reviews: &'a [PullRequestReview],
    pub threads: &'a ReviewThreadsResponse,
    pub requested: &'a RequestedReviewers,
    pub head: &'a GitCommitSha,
    pub commits: &'a [Commit],
    pub now: Timestamp,
}

/// Wrapper exposing Copilot's project + candidates as an [`Axis`] impl.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CopilotAxis;

impl<'a> Axis<CopilotObservation<'a>> for CopilotAxis {
    type Report = Option<CopilotReport>;
    type ActionKind = ActionKind;

    fn project(&self, obs: &CopilotObservation<'a>) -> Self::Report {
        let config = obs.config?;
        orient_copilot(
            config,
            obs.events,
            obs.reviews,
            obs.threads,
            obs.requested,
            obs.head,
            obs.commits,
            obs.now,
        )
    }

    fn candidates(&self, report: &Self::Report) -> Vec<Action> {
        match report {
            Some(r) => crate::decide::copilot::candidates(r),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::requested_reviewers::RequestedReviewers;
    use crate::observe::github::review_threads::empty_review_threads_response;

    #[test]
    fn copilot_axis_emits_no_candidates_when_no_policy() {
        // config: None ⇒ no Copilot reviewer configured ⇒ project
        // short-circuits without invoking orient_copilot.
        let axis = CopilotAxis;
        let threads = empty_review_threads_response();
        let requested = RequestedReviewers::default();
        let obs = CopilotObservation {
            config: None,
            events: &[],
            reviews: &[],
            threads: &threads,
            requested: &requested,
            head: &GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            commits: &[],
            now: Timestamp::parse("2026-05-17T12:00:00Z").unwrap(),
        };
        let report = axis.project(&obs);
        assert!(report.is_none(), "absent config should produce no report");
        let candidates = axis.candidates(&report);
        assert!(
            candidates.is_empty(),
            "absent copilot report should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
