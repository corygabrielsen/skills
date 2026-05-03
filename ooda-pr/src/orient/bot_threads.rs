//! Shared bot-thread summarization. Parameterized by an identity
//! predicate — the algorithm is identical across reviewer bots.

use crate::ids::Timestamp;
use crate::observe::github::review_threads::ReviewThreadsResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BotThreadSummary {
    pub total: u32,
    pub resolved: u32,
    /// Threads that are `is_resolved=false` AND `is_outdated=false` —
    /// the actor can act on them. Outdated threads are tracked
    /// separately in `outdated`.
    pub unresolved: u32,
    /// Threads that are `is_resolved=false` AND `is_outdated=true` —
    /// GitHub flagged the anchor line as moved by a rebase/amend.
    /// Surfaced for diagnostics (e.g., fitness comment) but excluded
    /// from `unresolved` so they do not drive AddressThreads actions.
    pub outdated: u32,
    /// Thread has any non-bot comment authored strictly after
    /// `latest_reviewed_at` — the bot hasn't observed it.
    pub stale: u32,
}

/// Summarize review threads authored by a specific bot.
///
/// Authorship is determined by the *first* comment on the thread
/// (the thread-opening comment). Subsequent replies do not change
/// authorship for counting purposes.
///
/// `stale` only fires when `latest_reviewed_at` is `Some` — without
/// a completed review, no thread can be stale relative to it.
pub fn count_bot_threads<F>(
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
        let Some(author) = &first.author else { continue };
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
