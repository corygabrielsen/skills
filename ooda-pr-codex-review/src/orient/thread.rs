//! Domain model for review threads.
//!
//! # Invariants
//!
//! - **Wire vs domain separation**: the wire shape mirrors the host
//!   API; this domain shape carries a parsed author, a partitioned
//!   lifecycle state, and a typed location. Renaming a wire field
//!   never propagates into decide.
//! - **Originator-defines-authorship**: thread authorship is fixed
//!   by the first comment. Replies — bot or human — do not reassign
//!   it. Same rule as the bot-thread accountant.
//! - **Lifecycle partition is exhaustive**: every wire pair of
//!   `(resolved, outdated)` maps to exactly one lifecycle variant.
//!   Resolution dominates outdation when both flags are set.
//! - **Witness, not cardinality**: decide consumes the literal
//!   threads so prompts carry the source text directly, with no
//!   secondary round-trip to the host needed to render them.

use crate::ids::{GitHubLogin, Timestamp};
use crate::observe::github::review_threads::ReviewThread as WireThread;
use crate::orient::copilot::is_copilot;
use crate::orient::cursor::is_cursor;
use serde::Serialize;

/// Opaque host node id for a review thread. Used for paginating
/// further comments and as a stable key for cross-iteration
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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

/// Repo-relative file path in canonical form: forward slashes, no
/// leading slash. Same shape every source (host, VCS index) yields.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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

/// Anchor of a thread on HEAD. Line is absent when the anchor has
/// moved off a live line (outdated lifecycle state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// Reviewer-bot identity. Typed variants for first-class bots; an
/// `Other` arm carries the raw login for any other bot — preserves
/// authorship without coupling the type to vendor enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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

/// Thread authorship — bot or human — fixed by the originating
/// comment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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

/// Lifecycle state of a thread. Total partition over the wire pair
/// `(resolved, outdated)`; resolution dominates outdation when both
/// bits are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ThreadState {
    /// Open and anchored to a live line; actionable.
    Live,
    /// Open but anchor moved off a live line; not actionable.
    Outdated,
    /// Closed by reviewer or actor.
    Resolved,
}

/// Domain review thread. Carries everything decide and rendering
/// need: identity, author, location, originating body, lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewThread {
    pub id: ThreadId,
    pub author: ThreadAuthor,
    pub location: ThreadLocation,
    /// Body of the originating comment — the issue as authored.
    pub body: String,
    pub state: ThreadState,
    pub created_at: Timestamp,
}

impl ReviewThread {
    /// Project the wire shape into the domain model.
    ///
    /// Returns `None` for threads with no originating comment or no
    /// originating author — both are pathological wire states (orphan
    /// thread, deleted account). Such threads cannot be attributed or
    /// described and are dropped at the boundary.
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

/// Classify a login into a typed author. First-class bot identities
/// route to their named variants; any other bot-suffixed login routes
/// to the open bot arm; non-bot logins are human.
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
        let t =
            ReviewThread::from_wire(&wire(false, true, "src/foo.rs", None, "alice", "needs fix"))
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
        let t = ReviewThread::from_wire(&wire(false, false, "src/foo.rs", Some(1), "alice", "x"))
            .unwrap();
        match t.author {
            ThreadAuthor::Human(l) => assert_eq!(l.as_str(), "alice"),
            other @ ThreadAuthor::Bot(_) => panic!("expected Human, got {other:?}"),
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
