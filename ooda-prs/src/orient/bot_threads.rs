//! Per-bot review-thread accounting, parameterized by an authorship
//! predicate.
//!
//! # Invariants
//!
//! - **Originator-defines-authorship**: a thread is attributed to the
//!   bot iff its first comment was authored by the bot. Subsequent
//!   replies (human or bot) do not reassign attribution; the
//!   originating utterance fixes the row.
//! - **Lifecycle partition is total**: every counted thread falls
//!   into exactly one of `resolved`, `outdated`, or `unresolved`, so
//!   `total = resolved + outdated + unresolved` holds by construction.
//! - **Outdated is not actionable**: outdated threads (anchor moved
//!   by rebase/amend) are tracked separately for diagnostics but
//!   never contribute to the actionable-work count that drives
//!   remediation actions.
//! - **Staleness needs an anchor**: a thread can only be stale
//!   relative to a recorded review; absent that anchor the predicate
//!   is silent, never false-positive.

use crate::ids::Timestamp;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub(crate) struct BotThreadSummary {
    pub total: u32,
    pub resolved: u32,
    /// Actionable count — open and anchored to a live line.
    pub unresolved: u32,
    /// Open but anchor moved by a rebase/amend. Diagnostic only;
    /// excluded from the actionable count.
    pub outdated: u32,
    /// Thread carries a non-bot reply newer than the last recorded
    /// review by the same bot — the bot has not yet read the latest
    /// human response.
    pub stale: u32,
}

/// Summarize review threads attributed to the bot named by `is_bot`.
pub(crate) fn count_bot_threads<F>(
    threads: &ReviewThreadsResponse,
    latest_reviewed_at: Option<&Timestamp>,
    is_bot: F,
) -> BotThreadSummary
where
    F: Fn(&str) -> bool,
{
    let mut s = BotThreadSummary::default();
    for t in &threads.data.repository.pull_request.review_threads.nodes {
        let Some(first) = t.comments.nodes.first() else {
            continue;
        };
        let Some(author) = &first.author else {
            continue;
        };
        if !is_bot(author.login.as_str()) {
            continue;
        }
        s.total += 1;
        if t.is_resolved {
            s.resolved += 1;
        } else if t.is_outdated {
            s.outdated += 1;
        } else {
            s.unresolved += 1;
        }
        if let Some(reviewed_at) = latest_reviewed_at {
            for c in &t.comments.nodes {
                let Some(a) = &c.author else { continue };
                if is_bot(a.login.as_str()) {
                    continue;
                }
                if &c.created_at > reviewed_at {
                    s.stale += 1;
                    break;
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitHubLogin;
    use crate::observe::github::review_threads::{
        CommentAuthor, PageInfo, ReviewRequestsPage, ReviewThread, ReviewThreadsData,
        ReviewThreadsPage, ReviewThreadsPr, ReviewThreadsRepo, ReviewThreadsResponse,
        ThreadComment, ThreadComments,
    };

    fn bot_thread(resolved: bool, outdated: bool) -> ReviewThread {
        ReviewThread {
            id: String::new(),
            is_resolved: resolved,
            is_outdated: outdated,
            path: String::new(),
            line: None,
            comments: ThreadComments {
                page_info: PageInfo::default(),
                nodes: vec![ThreadComment {
                    database_id: None,
                    author: Some(CommentAuthor {
                        login: GitHubLogin::parse("copilot[bot]").unwrap(),
                    }),
                    created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                    body: "issue".into(),
                }],
            },
        }
    }

    fn response(threads: Vec<ReviewThread>) -> ReviewThreadsResponse {
        ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo::default(),
                            nodes: threads,
                        },
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        }
    }

    #[test]
    fn outdated_threads_partition_into_outdated_not_unresolved() {
        // 1 resolved + 1 unresolved + 2 outdated = 4 total
        // unresolved count must be 1 (outdated excluded), outdated count must be 2.
        let r = response(vec![
            bot_thread(true, false),
            bot_thread(false, false),
            bot_thread(false, true),
            bot_thread(false, true),
        ]);
        let s = count_bot_threads(&r, None, |login| login == "copilot[bot]");
        assert_eq!(s.total, 4);
        assert_eq!(s.resolved, 1);
        assert_eq!(s.unresolved, 1);
        assert_eq!(s.outdated, 2);
    }

    #[test]
    fn algebra_holds_total_eq_resolved_plus_unresolved_plus_outdated() {
        let r = response(vec![
            bot_thread(true, false),
            bot_thread(false, false),
            bot_thread(false, false),
            bot_thread(false, true),
        ]);
        let s = count_bot_threads(&r, None, |login| login == "copilot[bot]");
        assert_eq!(s.total, s.resolved + s.unresolved + s.outdated);
    }
}
