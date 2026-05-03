//! Typed view of the GraphQL `reviewThreads` + `reviewRequests` query
//! for a pull request.
//!
//! GraphQL shape is deeply nested (`data.repository.pullRequest.…`);
//! each level is its own small struct. Pagination cursors live on the
//! `reviewThreads` page; merging pages is the observer's job, not the
//! type layer's.

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{gh_json, GhError};

/// Run a GraphQL query body via `gh api graphql`. `body` is the
/// raw GraphQL string (no `query=` prefix); this helper builds the
/// `-f query=<body>` argument so the `gh` `-f key=value` contract
/// is explicit at the call site rather than buried inside a
/// `format!("query={{ ... }}")` template. The earlier inlined form
/// was correct but easy to misread as a bare body — this wrapper
/// removes the ambiguity for any reader (and reviewer).
fn gh_graphql<T: DeserializeOwned>(body: &str) -> Result<T, GhError> {
    let arg = format!("query={body}");
    gh_json(&["api", "graphql", "-f", &arg])
}

/// Fetch one page of review threads + the list of pending review
/// requests. Pagination (via `page_info.end_cursor`) is the
/// observer's job — this function performs a single GraphQL call.
///
/// Per-thread `comments` are also paginated (`first:100` with a
/// pageInfo). Threads with >100 comments are followed up by
/// `fetch_thread_comments_page` so stale-reply detection sees the
/// full comment history. Class invariant: every nested `first:N`
/// GraphQL list with a pageInfo must be followed up if
/// `hasNextPage`, or downstream consumers see a truncated view.
pub fn fetch_review_threads_page(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    cursor: Option<&str>,
) -> Result<ReviewThreadsResponse, GhError> {
    let after = match cursor {
        Some(c) => format!(r#",after:"{c}""#),
        None => String::new(),
    };
    let owner = slug.owner().as_str();
    let name = slug.repo().as_str();
    let body = format!(
        r#"{{
  repository(owner:"{owner}",name:"{name}") {{
    pullRequest(number:{pr}) {{
      reviewThreads(first:100{after}) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{
          id
          isResolved
          comments(first:100) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{ author {{ login }} createdAt body }}
          }}
        }}
      }}
      reviewRequests(first:100) {{
        nodes {{
          requestedReviewer {{
            ... on User {{ __typename login }}
            ... on Bot {{ __typename login }}
            ... on Team {{ __typename name }}
            ... on Mannequin {{ __typename login }}
          }}
        }}
      }}
    }}
  }}
}}"#
    );
    gh_graphql(&body)
}

/// Fetch one additional page of comments for a single review
/// thread, addressed by its GraphQL global node id. Used by
/// `fetch_all_review_threads` to drain `comments.pageInfo` past the
/// initial 100.
fn fetch_thread_comments_page(
    thread_id: &str,
    cursor: &str,
) -> Result<ThreadCommentsPageResponse, GhError> {
    let body = format!(
        r#"{{
  node(id:"{thread_id}") {{
    ... on PullRequestReviewThread {{
      comments(first:100,after:"{cursor}") {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{ author {{ login }} createdAt body }}
      }}
    }}
  }}
}}"#
    );
    gh_graphql(&body)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ThreadCommentsPageResponse {
    data: ThreadCommentsPageData,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ThreadCommentsPageData {
    node: ThreadCommentsPageNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ThreadCommentsPageNode {
    comments: ThreadComments,
}

// -- top-level wrapping -----------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewThreadsResponse {
    pub data: ReviewThreadsData,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewThreadsData {
    pub repository: ReviewThreadsRepo,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadsRepo {
    pub pull_request: ReviewThreadsPr,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadsPr {
    pub review_threads: ReviewThreadsPage,
    pub review_requests: ReviewRequestsPage,
}

// -- reviewThreads ----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadsPage {
    pub page_info: PageInfo,
    pub nodes: Vec<ReviewThread>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    /// GraphQL global node id. Used by
    /// `fetch_thread_comments_page` to fetch additional comment
    /// pages on threads with >100 comments. `#[serde(default)]`
    /// keeps the existing test fixtures (which omit `id`)
    /// deserializable.
    #[serde(default)]
    pub id: String,
    pub is_resolved: bool,
    pub comments: ThreadComments,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComments {
    /// `#[serde(default)]` so test fixtures and inline JSON without
    /// pageInfo deserialize as "no further pages".
    #[serde(default)]
    pub page_info: PageInfo,
    pub nodes: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComment {
    /// Null if the commenter's account has been deleted.
    pub author: Option<CommentAuthor>,
    pub created_at: Timestamp,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommentAuthor {
    pub login: GitHubLogin,
}

// -- reviewRequests ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewRequestsPage {
    pub nodes: Vec<ReviewRequestNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequestNode {
    /// Null when the requested reviewer has been removed or the actor
    /// is otherwise unknown.
    pub requested_reviewer: Option<RequestedReviewer>,
}

/// Tagged by GraphQL `__typename`. Users/Bots/Mannequins carry a
/// login; Teams carry a name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "__typename")]
pub enum RequestedReviewer {
    User { login: GitHubLogin },
    Bot { login: GitHubLogin },
    Team { name: String },
    Mannequin { login: GitHubLogin },
}

// -- shared -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// Fetch every page of review threads, merging them into one
/// `ReviewThreadsResponse`. `reviewRequests` is page-invariant —
/// taken from the first page only. Per-thread `comments` are also
/// drained beyond the initial 100 via
/// `fetch_thread_comments_page` so stale-reply detection sees the
/// full comment history.
///
/// Callers that don't expect >100 unresolved threads can call
/// `fetch_review_threads_page` directly; `fetch_all` uses this
/// loop for correctness on big PRs.
pub fn fetch_all_review_threads(
    slug: &crate::ids::RepoSlug,
    pr: crate::ids::PullRequestNumber,
) -> Result<ReviewThreadsResponse, GhError> {
    let first = fetch_review_threads_page(slug, pr, None)?;
    let pull_first = first.data.repository.pull_request;
    let review_requests = pull_first.review_requests;
    let mut all_threads: Vec<ReviewThread> = pull_first.review_threads.nodes;
    let mut cursor = next_cursor(pull_first.review_threads.page_info);

    while let Some(c) = cursor {
        let page = fetch_review_threads_page(slug, pr, Some(&c))?;
        let pull = page.data.repository.pull_request;
        all_threads.extend(pull.review_threads.nodes);
        cursor = next_cursor(pull.review_threads.page_info);
    }

    for thread in &mut all_threads {
        drain_comment_pages(thread)?;
    }

    Ok(ReviewThreadsResponse {
        data: ReviewThreadsData {
            repository: ReviewThreadsRepo {
                pull_request: ReviewThreadsPr {
                    review_threads: ReviewThreadsPage {
                        page_info: PageInfo {
                            has_next_page: false,
                            end_cursor: None,
                        },
                        nodes: all_threads,
                    },
                    review_requests,
                },
            },
        },
    })
}

/// If `thread.comments` reports `hasNextPage`, paginate via
/// `fetch_thread_comments_page` and append every subsequent page's
/// nodes onto the in-place `comments.nodes`. The thread's own id is
/// the GraphQL node id used to address it. No-op when the first
/// page already covered the thread.
fn drain_comment_pages(thread: &mut ReviewThread) -> Result<(), GhError> {
    if !thread.comments.page_info.has_next_page {
        return Ok(());
    }
    if thread.id.is_empty() {
        // Defensive: a fixture or inline JSON without an id can't
        // be paginated. Treat as end-of-stream rather than fail.
        return Ok(());
    }
    let mut cursor = next_cursor(thread.comments.page_info.clone());
    while let Some(c) = cursor {
        let page = fetch_thread_comments_page(&thread.id, &c)?;
        thread.comments.nodes.extend(page.data.node.comments.nodes);
        cursor = next_cursor(page.data.node.comments.page_info);
    }
    thread.comments.page_info = PageInfo::default();
    Ok(())
}

fn next_cursor(info: PageInfo) -> Option<String> {
    if info.has_next_page {
        info.end_cursor
    } else {
        None
    }
}

/// Empty `ReviewThreadsResponse` for callers that need a stub when
/// no actual fetch is required (e.g. terminal-PR short-circuit in
/// `fetch_all`). Has no threads and no review requests.
pub fn empty_review_threads_response() -> ReviewThreadsResponse {
    ReviewThreadsResponse {
        data: ReviewThreadsData {
            repository: ReviewThreadsRepo {
                pull_request: ReviewThreadsPr {
                    review_threads: ReviewThreadsPage {
                        page_info: PageInfo::default(),
                        nodes: vec![],
                    },
                    review_requests: ReviewRequestsPage { nodes: vec![] },
                },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREADS_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/review_threads_page.json");

    #[test]
    fn deserializes_fixture_merged_pr() {
        let resp: ReviewThreadsResponse = serde_json::from_str(THREADS_FIXTURE).unwrap();
        let pr = &resp.data.repository.pull_request;

        // 3 resolved Copilot threads, no pending requests.
        assert_eq!(pr.review_threads.nodes.len(), 3);
        assert!(pr.review_threads.nodes.iter().all(|t| t.is_resolved));
        assert!(!pr.review_threads.page_info.has_next_page);
        assert!(pr.review_threads.page_info.end_cursor.is_some());

        // Each thread has at least one comment from Copilot.
        for t in &pr.review_threads.nodes {
            assert!(!t.comments.nodes.is_empty());
            let a = t.comments.nodes[0]
                .author
                .as_ref()
                .expect("fixture has non-null authors");
            assert_eq!(a.login.as_str(), "copilot-pull-request-reviewer");
        }

        assert_eq!(pr.review_requests.nodes.len(), 0);
    }

    #[test]
    fn requested_reviewer_variants_all_parse() {
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},
                "reviewRequests":{"nodes":[
                    {"requestedReviewer":{"__typename":"User","login":"alice"}},
                    {"requestedReviewer":{"__typename":"Bot","login":"dependabot[bot]"}},
                    {"requestedReviewer":{"__typename":"Team","name":"backend"}},
                    {"requestedReviewer":{"__typename":"Mannequin","login":"ghost"}},
                    {"requestedReviewer":null}
                ]}
            }}}
        }"#;
        let resp: ReviewThreadsResponse = serde_json::from_str(json).unwrap();
        let nodes = &resp.data.repository.pull_request.review_requests.nodes;
        assert_eq!(nodes.len(), 5);

        assert!(matches!(
            &nodes[0].requested_reviewer,
            Some(RequestedReviewer::User { login }) if login.as_str() == "alice"
        ));
        assert!(matches!(
            &nodes[1].requested_reviewer,
            Some(RequestedReviewer::Bot { login }) if login.as_str() == "dependabot[bot]"
        ));
        assert!(matches!(
            &nodes[2].requested_reviewer,
            Some(RequestedReviewer::Team { name }) if name == "backend"
        ));
        assert!(matches!(
            &nodes[3].requested_reviewer,
            Some(RequestedReviewer::Mannequin { login }) if login.as_str() == "ghost"
        ));
        assert_eq!(nodes[4].requested_reviewer, None);
    }

    #[test]
    fn null_comment_author_survives() {
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{
                    "pageInfo":{"hasNextPage":false,"endCursor":null},
                    "nodes":[{"isResolved":false,"comments":{"nodes":[
                        {"author":null,"createdAt":"2026-04-23T00:00:00Z","body":"deleted account"}
                    ]}}]
                },
                "reviewRequests":{"nodes":[]}
            }}}
        }"#;
        let resp: ReviewThreadsResponse = serde_json::from_str(json).unwrap();
        let c = &resp.data.repository.pull_request.review_threads.nodes[0].comments.nodes[0];
        assert!(c.author.is_none());
        assert_eq!(c.body, "deleted account");
    }

    #[test]
    fn pagination_cursor_round_trips() {
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{
                    "pageInfo":{"hasNextPage":true,"endCursor":"abc123"},
                    "nodes":[]
                },
                "reviewRequests":{"nodes":[]}
            }}}
        }"#;
        let resp: ReviewThreadsResponse = serde_json::from_str(json).unwrap();
        let info = &resp.data.repository.pull_request.review_threads.page_info;
        assert!(info.has_next_page);
        assert_eq!(info.end_cursor.as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_unknown_typename() {
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},
                "reviewRequests":{"nodes":[{"requestedReviewer":{"__typename":"Martian","login":"x"}}]}
            }}}
        }"#;
        let err = serde_json::from_str::<ReviewThreadsResponse>(json).unwrap_err();
        assert!(err.to_string().contains("Martian") || err.to_string().contains("unknown variant"));
    }
}
