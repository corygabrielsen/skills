//! Typed projection for the review-thread + reviewer-request
//! GraphQL query.
//!
//! # Invariants
//!
//! - **Nested-list pagination is fetcher-side**: every nested list
//!   that carries a page cursor must be drained by the fetcher
//!   before the bundle leaves the boundary; downstream consumers
//!   assume the bundle holds every node, not just a first page.
//! - **Wire/domain separation**: this module is pure shape — the
//!   typed domain projection lives in the orient layer.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug, TeamName, Timestamp};

use super::gh::{GhError, gh_json};

/// Submit a GraphQL query body to the host CLI. `body` is the raw
/// query; the wrapper builds the argv form so callers do not embed
/// the host CLI's argv-key contract inside their own format strings.
fn gh_graphql<T: DeserializeOwned>(body: &str) -> Result<T, GhError> {
    let arg = format!("query={body}");
    gh_json(&["api", "graphql", "-f", &arg])
}

/// Fetch one page of review threads plus the (page-invariant)
/// reviewer-request list. Pagination at any level is the caller's
/// responsibility — see the module-level nested-list invariant.
pub(crate) fn fetch_review_threads_page(
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
          isOutdated
          path
          line
          comments(first:100) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{ databaseId author {{ login }} createdAt body }}
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

/// Fetch one additional page of comments for a single thread,
/// addressed by node id. Used to drain the per-thread comment
/// pageInfo past the first page.
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
        nodes {{ databaseId author {{ login }} createdAt body }}
      }}
    }}
  }}
}}"#
    );
    gh_graphql(&body)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ThreadCommentsPageResponse {
    data: ThreadCommentsPageData,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ThreadCommentsPageData {
    node: ThreadCommentsPageNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ThreadCommentsPageNode {
    comments: ThreadComments,
}

// -- top-level wrapping -----------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewThreadsResponse {
    pub data: ReviewThreadsData,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewThreadsData {
    pub repository: ReviewThreadsRepo,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewThreadsRepo {
    pub pull_request: ReviewThreadsPr,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewThreadsPr {
    pub review_threads: ReviewThreadsPage,
    pub review_requests: ReviewRequestsPage,
}

// -- reviewThreads ----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewThreadsPage {
    pub page_info: PageInfo,
    pub nodes: Vec<ReviewThread>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    /// Node id for cross-iteration identity and follow-up
    /// pagination. Default-empty for legacy fixtures.
    #[serde(default)]
    pub id: String,
    pub is_resolved: bool,
    /// Anchor moved off a live line (rebase/amend shifted lines).
    /// Excluded from the actionable count by the orient layer.
    /// Default-false for legacy fixtures.
    #[serde(default)]
    pub is_outdated: bool,
    /// Repo-relative file path the thread is anchored to.
    #[serde(default)]
    pub path: String,
    /// Anchored line at HEAD; absent on outdated threads.
    #[serde(default)]
    pub line: Option<u32>,
    pub comments: ThreadComments,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComments {
    /// Default-empty pageInfo so fixtures decode as "no further
    /// pages."
    #[serde(default)]
    pub page_info: PageInfo,
    pub nodes: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComment {
    /// Numeric REST id for this comment. The line-anchored-replies
    /// API (`POST /pulls/{n}/comments/{id}/replies`) is keyed by
    /// this id, NOT by line — required when replying to outdated /
    /// null-line threads. Optional only because older fixtures
    /// don't carry it.
    #[serde(default)]
    pub database_id: Option<u64>,
    /// Absent when the comment's authoring identity has been
    /// deleted.
    pub author: Option<CommentAuthor>,
    pub created_at: Timestamp,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommentAuthor {
    pub login: GitHubLogin,
}

// -- reviewRequests ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewRequestsPage {
    pub nodes: Vec<ReviewRequestNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewRequestNode {
    /// Absent when the requested reviewer has been removed or the
    /// actor is otherwise unknown.
    pub requested_reviewer: Option<RequestedReviewer>,
}

/// Reviewer-request union tagged by host typename. Identity-bearing
/// variants carry a login; team variants carry a name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "__typename")]
pub(crate) enum RequestedReviewer {
    User { login: GitHubLogin },
    Bot { login: GitHubLogin },
    Team { name: TeamName },
    Mannequin { login: GitHubLogin },
}

// -- shared -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// Drain every page of review threads plus every per-thread comment
/// page into a single bundled response. The reviewer-request list
/// is page-invariant and taken from the first page only.
pub(crate) fn fetch_all_review_threads(
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

/// Drain remaining comment pages onto a thread in place. No-op
/// when the first page already covered the thread.
fn drain_comment_pages(thread: &mut ReviewThread) -> Result<(), GhError> {
    if !thread.comments.page_info.has_next_page {
        return Ok(());
    }
    if thread.id.is_empty() {
        // Defensive: a fixture without an id cannot be paginated;
        // treat as end-of-stream rather than fail.
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

/// Empty bundle for callers that need a stub when no fetch is
/// performed (e.g., terminal-PR short-circuit). No threads, no
/// reviewer requests.
pub(crate) fn empty_review_threads_response() -> ReviewThreadsResponse {
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
    fn deserializes_fixture_merged_pull_request() {
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
            Some(RequestedReviewer::Team { name }) if name.as_str() == "backend"
        ));
        assert!(matches!(
            &nodes[3].requested_reviewer,
            Some(RequestedReviewer::Mannequin { login }) if login.as_str() == "ghost"
        ));
        assert_eq!(nodes[4].requested_reviewer, None);
    }

    #[test]
    fn thread_comment_carries_database_id_when_present() {
        // GraphQL `databaseId` round-trips as `Option<u64>`. Needed
        // for the line-anchored-replies endpoint on outdated/null-
        // line threads (keyed by comment id, not line).
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{
                    "pageInfo":{"hasNextPage":false,"endCursor":null},
                    "nodes":[{
                        "id":"PRRT_1",
                        "isResolved":false,
                        "isOutdated":true,
                        "path":"docker/Dockerfile.ci",
                        "line":null,
                        "comments":{
                            "pageInfo":{"hasNextPage":false,"endCursor":null},
                            "nodes":[{
                                "databaseId":3377501272,
                                "author":{"login":"cursor"},
                                "createdAt":"2026-04-23T00:00:00Z",
                                "body":"x"
                            }]
                        }
                    }]
                },
                "reviewRequests":{"nodes":[]}
            }}}
        }"#;
        let resp: ReviewThreadsResponse = serde_json::from_str(json).unwrap();
        let t = &resp.data.repository.pull_request.review_threads.nodes[0];
        assert_eq!(t.comments.nodes[0].database_id, Some(3_377_501_272));
    }

    #[test]
    fn thread_comment_database_id_missing_decodes_as_none() {
        // Fixtures that don't carry databaseId still decode — the
        // field is optional and defaults to None.
        let json = r#"{
            "data":{"repository":{"pullRequest":{
                "reviewThreads":{
                    "pageInfo":{"hasNextPage":false,"endCursor":null},
                    "nodes":[{
                        "id":"PRRT_1",
                        "isResolved":false,
                        "isOutdated":false,
                        "path":"src/foo.rs",
                        "line":1,
                        "comments":{
                            "pageInfo":{"hasNextPage":false,"endCursor":null},
                            "nodes":[{
                                "author":{"login":"alice"},
                                "createdAt":"2026-04-23T00:00:00Z",
                                "body":"x"
                            }]
                        }
                    }]
                },
                "reviewRequests":{"nodes":[]}
            }}}
        }"#;
        let resp: ReviewThreadsResponse = serde_json::from_str(json).unwrap();
        let t = &resp.data.repository.pull_request.review_threads.nodes[0];
        assert_eq!(t.comments.nodes[0].database_id, None);
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
        let c = &resp.data.repository.pull_request.review_threads.nodes[0]
            .comments
            .nodes[0];
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
