//! Shared bot-thread summarization. Parameterized by an identity
//! predicate — the algorithm is identical across reviewer bots.

use crate::ids::Timestamp;
use crate::observe::github::review_threads::ReviewThreadsResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BotThreadSummary {
    pub total: u32,
    pub resolved: u32,
    pub unresolved: u32,
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
