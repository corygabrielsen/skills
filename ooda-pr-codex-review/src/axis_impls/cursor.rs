//! `CursorAxis` — `Axis` impl wrapping the Cursor bot-review lane.
//!
//! Same shape as [`super::ci::CiAxis`]: a thin wrapper around
//! `orient/cursor.rs` + `decide/cursor.rs`. Projection lives
//! inside [`Axis::candidates`] as a private helper.
//!
//! # Absent-observation case
//!
//! `orient_cursor` returns `Option<CursorReport>` to distinguish
//! "no observation at all" from "observation says
//! `NotApplicable`." When the inner projection is `None` the
//! axis emits no candidates — mirroring how `decide.rs` only
//! calls `cursor::candidates` when `oriented.cursor` is `Some`.
//!
//! # Cross-axis deps
//!
//! None. Every field reads from the global observation slice.

use crate::decide::action::{Action, ActionKind};
use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::cursor_status::CursorStatus;
use crate::observe::github::pull_request_view::PullRequestAuthor;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::orient::cursor::orient_cursor;
use ooda_core::Axis;

/// Per-axis observation slice for [`CursorAxis`].
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CursorObservation<'a> {
    pub reviews: &'a [PullRequestReview],
    pub threads: &'a ReviewThreadsResponse,
    pub cursor_status: &'a CursorStatus,
    pub author: Option<&'a PullRequestAuthor>,
    pub head: &'a GitCommitSha,
    pub now: Timestamp,
}

/// Wrapper exposing Cursor as an [`Axis`] impl.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CursorAxis;

impl<'a> Axis<CursorObservation<'a>> for CursorAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &CursorObservation<'a>) -> Vec<Action> {
        let Some(report) = orient_cursor(
            obs.reviews,
            obs.threads,
            obs.cursor_status,
            obs.author,
            obs.head,
            obs.now,
        ) else {
            return Vec::new();
        };
        crate::decide::cursor::candidates(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::cursor_status::CursorStatus;
    use crate::observe::github::review_threads::empty_review_threads_response;

    #[test]
    fn cursor_axis_emits_no_candidates_when_no_observation() {
        let axis = CursorAxis;
        let threads = empty_review_threads_response();
        let cursor_status = CursorStatus {
            suite: None,
            run: None,
        };
        let obs = CursorObservation {
            reviews: &[],
            threads: &threads,
            cursor_status: &cursor_status,
            author: None,
            head: &GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            now: Timestamp::parse("2026-05-17T12:00:00Z").unwrap(),
        };
        let candidates = axis.candidates(&obs);
        assert!(
            candidates.is_empty(),
            "no signal should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
