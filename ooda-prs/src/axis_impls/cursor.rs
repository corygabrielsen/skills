//! `CursorAxis` — `Axis` impl wrapping the Cursor bot-review lane.
//!
//! Same shape as [`super::ci::CiAxis`]: thin wrapper around the
//! existing `orient/cursor.rs` and `decide/cursor.rs` free
//! functions. No new logic.
//!
//! # Report shape
//!
//! `orient_cursor` returns `Option<CursorReport>` to distinguish
//! "no observation at all" from "observation says
//! `NotApplicable`." The trait's `Report` associated type carries
//! the `Option` through; `candidates` short-circuits to an empty
//! vec on `None`, matching how `decide.rs` calls the inner
//! `cursor::candidates` only when `oriented.cursor` is `Some`.
//!
//! # Cross-axis deps
//!
//! None. Every field of [`CursorObservation`] reads from the
//! global observation slice directly.

use crate::decide::action::{Action, ActionKind};
use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::cursor_status::CursorStatus;
use crate::observe::github::pull_request_view::PullRequestAuthor;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::orient::cursor::{CursorReport, orient_cursor};
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

/// Wrapper exposing Cursor's project + candidates as an [`Axis`] impl.
///
/// Zero-sized; per-tick state lives in the projected
/// `Option<CursorReport>`.
#[allow(dead_code)] // Wired into the driver in the next arc; today reachable only via tests.
pub(crate) struct CursorAxis;

impl<'a> Axis<CursorObservation<'a>> for CursorAxis {
    type Report = Option<CursorReport>;
    type ActionKind = ActionKind;

    fn project(&self, obs: &CursorObservation<'a>) -> Self::Report {
        orient_cursor(
            obs.reviews,
            obs.threads,
            obs.cursor_status,
            obs.author,
            obs.head,
            obs.now,
        )
    }

    fn candidates(&self, report: &Self::Report) -> Vec<Action> {
        match report {
            Some(r) => crate::decide::cursor::candidates(r),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::cursor_status::CursorStatus;
    use crate::observe::github::review_threads::empty_review_threads_response;

    #[test]
    fn cursor_axis_emits_no_candidates_when_no_observation() {
        // No reviews, no cursor suite, no author → orient_cursor
        // returns None → candidates short-circuits to empty.
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
        let report = axis.project(&obs);
        assert!(report.is_none(), "no signal should produce no report");
        let candidates = axis.candidates(&report);
        assert!(
            candidates.is_empty(),
            "absent cursor report should emit no candidates; got {:?}",
            candidates.iter().map(|a| &a.kind).collect::<Vec<_>>(),
        );
    }
}
