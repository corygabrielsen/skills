//! Typed domain model for review threads.
//!
//! Distinct from the wire-level [`crate::observe::github::review_threads::ReviewThread`]:
//! the wire model mirrors GraphQL field names; the domain model
//! carries a parsed author, a partitioned `state` enum, and a typed
//! location. Decide consumes the domain model so it can fire
//! `AddressThreads { threads: Vec<ReviewThread> }` with the literal
//! threads (witness, not cardinality) — the actor receives prompt
//! material directly, no second `gh` round-trip.

use crate::ids::{GitHubLogin, Timestamp};
use crate::observe::github::review_threads::ReviewThread as WireThread;
use crate::orient::copilot::is_copilot;
use crate::orient::cursor::is_cursor;

/// GraphQL global node id for a review thread. Opaque string used
/// for paginating comments and (eventually) for stable stall keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Repo-relative file path. Forward slashes, no leading slash —
/// the canonical form GitHub returns and `git ls-files` produces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FilePath(String);

impl FilePath {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FilePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a thread is anchored at HEAD. `line` is `None` for
/// outdated threads (anchor line shifted away by rebase/amend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadLocation {
    pub path: FilePath,
    pub line: Option<u32>,
}

impl std::fmt::Display for ThreadLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}", self.path),
            None => write!(f, "{}", self.path),
        }
    }
}

/// Recognized review bots. `Other` carries the raw login for any
/// bot we don't have a typed variant for — preserves authorship
/// without forcing a code change for every new bot vendor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BotName {
    Copilot,
    Cursor,
    Other(GitHubLogin),
}

impl std::fmt::Display for BotName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Copilot => f.write_str("Copilot"),
            Self::Cursor => f.write_str("Cursor"),
            Self::Other(l) => write!(f, "{}", l.as_str()),
        }
    }
}

/// Author of a review thread, by the *first* (originating) comment.
/// Replies do not change authorship for routing/grouping purposes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThreadAuthor {
    Bot(BotName),
    Human(GitHubLogin),
}

impl std::fmt::Display for ThreadAuthor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bot(b) => write!(f, "{b}"),
            Self::Human(l) => write!(f, "{}", l.as_str()),
        }
    }
}

/// Lifecycle state of a thread, partitioning the `(is_resolved, is_outdated)`
/// space — exactly one variant matches any wire-level pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    /// `!is_resolved && !is_outdated` — actor can address.
    Live,
    /// `!is_resolved && is_outdated` — anchor moved; not actionable.
    Outdated,
    /// `is_resolved` — closed by reviewer or actor.
    Resolved,
}

/// Domain-level review thread carrying everything decide and the
/// description renderer need: identity, author, location, body, and
/// lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: ThreadId,
    pub author: ThreadAuthor,
    pub location: ThreadLocation,
    /// Body of the originating (first) comment — the issue
    /// description as authored.
    pub body: String,
    pub state: ThreadState,
    pub created_at: Timestamp,
}

impl ReviewThread {
    /// Project a wire-level thread into the domain model.
    ///
    /// Returns `None` for threads with no first comment or no
    /// author on the first comment — both are pathological wire
    /// states (orphan thread, deleted account on the originator).
    /// Decide cannot meaningfully attribute or describe such threads,
    /// so they are dropped at the boundary.
    pub fn from_wire(wire: &WireThread) -> Option<Self> {
        let first = wire.comments.nodes.first()?;
        let author_login = first.author.as_ref()?.login.clone();
        let author = classify_author(&author_login);
        let state = match (wire.is_resolved, wire.is_outdated) {
            (true, _) => ThreadState::Resolved,
            (false, true) => ThreadState::Outdated,
            (false, false) => ThreadState::Live,
        };
        Some(ReviewThread {
            id: ThreadId::new(wire.id.clone()),
            author,
            location: ThreadLocation {
                path: FilePath::new(wire.path.clone()),
                line: wire.line,
            },
            body: first.body.clone(),
            state,
            created_at: first.created_at,
        })
    }
}

/// Map a login to a typed author. Recognized bots get their named
/// variants; unrecognized bot logins (suffix `[bot]`) become
/// `Bot(Other(login))`; everything else is a human.
fn classify_author(login: &GitHubLogin) -> ThreadAuthor {
    let s = login.as_str();
    if is_copilot(s) {
        ThreadAuthor::Bot(BotName::Copilot)
    } else if is_cursor(s) {
        ThreadAuthor::Bot(BotName::Cursor)
    } else if login.is_bot() {
        ThreadAuthor::Bot(BotName::Other(login.clone()))
    } else {
        ThreadAuthor::Human(login.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::review_threads::{
        CommentAuthor, PageInfo, ThreadComment, ThreadComments,
    };

    fn wire(
        is_resolved: bool,
        is_outdated: bool,
        path: &str,
        line: Option<u32>,
        author_login: &str,
        body: &str,
    ) -> WireThread {
        WireThread {
            id: "t1".into(),
            is_resolved,
            is_outdated,
            path: path.into(),
            line,
            comments: ThreadComments {
                page_info: PageInfo::default(),
                nodes: vec![ThreadComment {
                    author: Some(CommentAuthor {
                        login: GitHubLogin::parse(author_login).unwrap(),
                    }),
                    created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                    body: body.into(),
                }],
            },
        }
    }

    #[test]
    fn live_thread_projects_to_live_state() {
        let t = ReviewThread::from_wire(&wire(
            false,
            false,
            "src/foo.rs",
            Some(42),
            "alice",
            "needs fix",
        ))
        .unwrap();
        assert_eq!(t.state, ThreadState::Live);
        assert_eq!(t.location.line, Some(42));
    }

    #[test]
    fn outdated_thread_projects_to_outdated_state() {
        let t = ReviewThread::from_wire(&wire(
            false,
            true,
            "src/foo.rs",
            None,
            "alice",
            "needs fix",
        ))
        .unwrap();
        assert_eq!(t.state, ThreadState::Outdated);
        assert_eq!(t.location.line, None);
    }

    #[test]
    fn resolved_dominates_outdated() {
        // (is_resolved=true, is_outdated=true) → Resolved (resolution wins).
        let t = ReviewThread::from_wire(&wire(
            true,
            true,
            "src/foo.rs",
            Some(42),
            "alice",
            "needs fix",
        ))
        .unwrap();
        assert_eq!(t.state, ThreadState::Resolved);
    }

    #[test]
    fn classifies_copilot_login() {
        let t = ReviewThread::from_wire(&wire(
            false,
            false,
            "src/foo.rs",
            Some(1),
            "copilot-pull-request-reviewer[bot]",
            "x",
        ))
        .unwrap();
        assert!(matches!(t.author, ThreadAuthor::Bot(BotName::Copilot)));
    }

    #[test]
    fn classifies_cursor_login() {
        let t = ReviewThread::from_wire(&wire(
            false,
            false,
            "src/foo.rs",
            Some(1),
            "cursor[bot]",
            "x",
        ))
        .unwrap();
        assert!(matches!(t.author, ThreadAuthor::Bot(BotName::Cursor)));
    }

    #[test]
    fn classifies_unknown_bot_as_other() {
        let t = ReviewThread::from_wire(&wire(
            false,
            false,
            "src/foo.rs",
            Some(1),
            "dependabot[bot]",
            "x",
        ))
        .unwrap();
        match t.author {
            ThreadAuthor::Bot(BotName::Other(l)) => {
                assert_eq!(l.as_str(), "dependabot[bot]");
            }
            other => panic!("expected Bot(Other), got {other:?}"),
        }
    }

    #[test]
    fn classifies_human() {
        let t = ReviewThread::from_wire(&wire(
            false,
            false,
            "src/foo.rs",
            Some(1),
            "alice",
            "x",
        ))
        .unwrap();
        match t.author {
            ThreadAuthor::Human(l) => assert_eq!(l.as_str(), "alice"),
            other => panic!("expected Human, got {other:?}"),
        }
    }

    #[test]
    fn drops_thread_with_no_comments() {
        let mut w = wire(false, false, "src/foo.rs", Some(1), "alice", "x");
        w.comments.nodes.clear();
        assert!(ReviewThread::from_wire(&w).is_none());
    }

    #[test]
    fn drops_thread_with_null_first_author() {
        let mut w = wire(false, false, "src/foo.rs", Some(1), "alice", "x");
        w.comments.nodes[0].author = None;
        assert!(ReviewThread::from_wire(&w).is_none());
    }

    #[test]
    fn location_displays_with_line() {
        let loc = ThreadLocation {
            path: FilePath::new("src/foo.rs"),
            line: Some(42),
        };
        assert_eq!(loc.to_string(), "src/foo.rs:42");
    }

    #[test]
    fn location_displays_without_line_when_outdated() {
        let loc = ThreadLocation {
            path: FilePath::new("src/foo.rs"),
            line: None,
        };
        assert_eq!(loc.to_string(), "src/foo.rs");
    }
}
