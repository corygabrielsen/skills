//! `CopilotAxis` — `Axis` impl wrapping the Copilot bot-review lane.
//!
//! Same shape as [`super::cursor::CursorAxis`]: thin wrapper
//! around `orient_copilot` + `decide::copilot::candidates`. Two
//! distinct absences produce an empty candidate set:
//!
//! - **No policy**: `config` is `None` — the repo has no Copilot
//!   reviewer configured. The wrapper short-circuits without
//!   calling `orient_copilot`, mirroring the call-site shape
//!   (`obs.copilot_config.map(...).and_then(...)`).
//! - **Disabled policy**: `config` is `Some` but
//!   `config.enabled` is false. `orient_copilot` returns `None`.
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
use crate::orient::copilot::{CopilotRepoConfig, orient_copilot};
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

/// Wrapper exposing Copilot as an [`Axis`] impl.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CopilotAxis;

impl<'a> Axis<CopilotObservation<'a>> for CopilotAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CopilotObservation<'a>) -> Vec<Action> {
        let Some(config) = obs.config else {
            return Vec::new();
        };
        let Some(report) = orient_copilot(
            config,
            obs.events,
            obs.reviews,
            obs.threads,
            obs.requested,
            obs.head,
            obs.commits,
            obs.now,
        ) else {
            return Vec::new();
        };
        crate::decide::copilot::candidates(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::requested_reviewers::RequestedReviewers;
    use crate::observe::github::review_threads::empty_review_threads_response;

    #[test]
    fn copilot_axis_emits_no_candidates_when_no_policy() {
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
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "absent config should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
