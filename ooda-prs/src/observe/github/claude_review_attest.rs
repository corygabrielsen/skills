//! Observation for the reviewer-content attestation axis.
//!
//! # Invariants
//!
//! - **Content drift, not SHA drift**: the reviewer in this domain
//!   does not re-fire on push, so SHA movement past the recorded
//!   attestation carries no signal. The trigger is whether new
//!   reviewer content exists past the attestation timestamp.
//! - **Surface aggregation, no new fetch**: per-surface data is
//!   reused from the existing observation bundle — this module
//!   adds a single filesystem read (the attestation file), never a
//!   new host call.
//! - **Body witness ≠ drift witness**: the prompt witness body
//>   carries its own timestamp distinct from the cross-surface drift
//>   timestamp; the two are computed independently and surfaced as
//>   independent fields.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ooda_core::attest::{ClaudeReviewAttestation, read_claude_review};
use serde::Serialize;

use crate::ids::{GitCommitSha, PullRequestNumber};
use crate::observe::github::comments::IssueComment;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::orient::claude_review::is_claude;

const CLAUDE_REVIEW_FILE: &str = "claude_review_attest.json";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClaudeReviewObservation {
    pub attestation: Option<ClaudeReviewAttestation>,
    pub head_sha: GitCommitSha,
    /// Always absent for this axis — content drift, not SHA drift,
    /// drives orient. Field kept for serialization parity with
    /// sibling attestation observations.
    pub commits_behind: Option<usize>,
    pub attest_path: Option<PathBuf>,
    /// Cross-surface drift witness — max content timestamp across
    /// every surface the reviewer writes to.
    pub latest_claude_at: Option<DateTime<Utc>>,
    /// Prompt-witness body timestamp. Distinct from the drift
    /// witness — anchors the surfaced body, not the unrelated
    /// surface that re-armed the axis.
    pub body_at: Option<DateTime<Utc>>,
    pub latest_claude_body: Option<String>,
    pub latest_claude_url: Option<String>,
    pub inline_thread_count: usize,
}

/// Compose the attestation file path for this axis. Shared with the
/// prompt-composition layer so the agent receives the same absolute
/// path it must record against.
#[must_use]
pub(crate) fn claude_review_attest_path(
    state_root: &std::path::Path,
    pr: PullRequestNumber,
) -> PathBuf {
    state_root.join(pr.to_string()).join(CLAUDE_REVIEW_FILE)
}

/// Observe the attestation plus aggregated reviewer content across
/// every surface in the existing bundle. No new host fetch is
/// performed; per-surface data is supplied by the caller.
pub(crate) fn observe_claude_review(
    state_root: Option<&std::path::Path>,
    pr: PullRequestNumber,
    head_sha: &GitCommitSha,
    reviews: &[PullRequestReview],
    issue_comments: &[IssueComment],
    threads: &ReviewThreadsResponse,
) -> ClaudeReviewObservation {
    let path = state_root.map(|root| claude_review_attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_claude_review(p).ok().flatten());

    let aggregate = aggregate_claude_content(reviews, issue_comments, threads);

    ClaudeReviewObservation {
        attestation,
        head_sha: head_sha.clone(),
        commits_behind: None,
        attest_path: path,
        latest_claude_at: aggregate.latest_at,
        body_at: aggregate.body_at,
        latest_claude_body: aggregate.latest_body,
        latest_claude_url: aggregate.latest_url,
        inline_thread_count: aggregate.inline_thread_count,
    }
}

struct ClaudeAggregate {
    latest_at: Option<DateTime<Utc>>,
    body_at: Option<DateTime<Utc>>,
    latest_body: Option<String>,
    latest_url: Option<String>,
    inline_thread_count: usize,
}

fn aggregate_claude_content(
    reviews: &[PullRequestReview],
    issue_comments: &[IssueComment],
    threads: &ReviewThreadsResponse,
) -> ClaudeAggregate {
    // Body-surface priority: structured-review submission wins over
    // issue-level comment; within each surface, latest timestamp
    // wins. Drift timestamp is computed independently as the max
    // across both surfaces.
    let latest_review = reviews
        .iter()
        .filter(|r| r.user.as_ref().is_some_and(|u| is_claude(u.login.as_str())))
        .filter_map(|r| r.submitted_at.as_ref().map(|t| (t, r)))
        .max_by_key(|(t, _)| t.at());
    let latest_review_at = latest_review.map(|(t, _)| t.at());

    let latest_issue = issue_comments
        .iter()
        .filter(|c| is_claude(c.user.login.as_str()))
        .max_by_key(|c| c.created_at.at());
    let latest_issue_at = latest_issue.map(|c| c.created_at.at());

    let (latest_at_overall, body_at, latest_body, latest_url) = match (latest_review, latest_issue)
    {
        (Some((rt, rev)), Some(ic)) => {
            let combined_max = std::cmp::max(rt.at(), ic.created_at.at());
            // Body/URL/body-at follow the structured-review
            // submission per the priority invariant; drift witness
            // is the cross-surface max.
            (
                Some(combined_max),
                Some(rt.at()),
                Some(rev.body.clone()),
                Some(rev.html_url.clone()),
            )
        }
        (Some((rt, rev)), None) => (
            latest_review_at,
            Some(rt.at()),
            Some(rev.body.clone()),
            Some(rev.html_url.clone()),
        ),
        (None, Some(ic)) => (
            latest_issue_at,
            Some(ic.created_at.at()),
            Some(ic.body.clone()),
            Some(ic.html_url.clone()),
        ),
        (None, None) => (None, None, None, None),
    };

    let inline_thread_count = threads
        .data
        .repository
        .pull_request
        .review_threads
        .nodes
        .iter()
        .filter(|t| {
            t.comments.nodes.iter().any(|c| {
                c.author
                    .as_ref()
                    .is_some_and(|a| is_claude(a.login.as_str()))
            })
        })
        .count();

    ClaudeAggregate {
        latest_at: latest_at_overall,
        body_at,
        latest_body,
        latest_url,
        inline_thread_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitHubLogin, RepoSlug, Timestamp};
    use crate::observe::github::comments::{CommentUser, IssueComment};
    use crate::observe::github::review_threads::{
        CommentAuthor, PageInfo, ReviewRequestsPage, ReviewThread, ReviewThreadsData,
        ReviewThreadsPage, ReviewThreadsPr, ReviewThreadsRepo, ThreadComment, ThreadComments,
    };
    use crate::observe::github::reviews::{PullRequestReview, ReviewState, ReviewUser};
    use crate::orient::claude_review::{ClaudeReview, orient_claude_review};
    use ooda_core::attest::{CLAUDE_REVIEW_SCHEMA_VERSION, write_claude_review_atomic};
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn slug() -> RepoSlug {
        RepoSlug::parse("acme/widget").unwrap()
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(VALID_SHA).unwrap()
    }

    fn empty_threads() -> ReviewThreadsResponse {
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

    fn claude_review(at: &str, body: &str, url: &str) -> PullRequestReview {
        PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse("claude[bot]").unwrap(),
            }),
            state: ReviewState::Commented,
            commit_id: GitCommitSha::parse(VALID_SHA).unwrap(),
            submitted_at: Some(Timestamp::parse(at).unwrap()),
            body: body.into(),
            html_url: url.into(),
        }
    }

    fn claude_issue(at: &str, body: &str, url: &str) -> IssueComment {
        IssueComment {
            id: 1,
            user: CommentUser {
                login: GitHubLogin::parse("claude[bot]").unwrap(),
            },
            body: body.into(),
            created_at: Timestamp::parse(at).unwrap(),
            html_url: url.into(),
        }
    }

    fn other_issue(at: &str, login: &str) -> IssueComment {
        IssueComment {
            id: 2,
            user: CommentUser {
                login: GitHubLogin::parse(login).unwrap(),
            },
            body: "noise".into(),
            created_at: Timestamp::parse(at).unwrap(),
            html_url: String::new(),
        }
    }

    fn threads_with(authors: &[Vec<&str>]) -> ReviewThreadsResponse {
        let nodes: Vec<ReviewThread> = authors
            .iter()
            .map(|logins| ReviewThread {
                id: String::new(),
                is_resolved: false,
                is_outdated: false,
                path: String::new(),
                line: None,
                comments: ThreadComments {
                    page_info: PageInfo::default(),
                    nodes: logins
                        .iter()
                        .map(|l| ThreadComment {
                            database_id: None,
                            author: Some(CommentAuthor {
                                login: GitHubLogin::parse(l).unwrap(),
                            }),
                            created_at: Timestamp::parse("2026-05-02T10:00:00Z").unwrap(),
                            body: String::new(),
                        })
                        .collect(),
                },
            })
            .collect();
        ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo::default(),
                            nodes,
                        },
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        }
    }

    #[test]
    fn attest_path_joins_pull_request_id_and_filename() {
        let p = claude_review_attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(
            p,
            std::path::PathBuf::from("/state/753/claude_review_attest.json")
        );
    }

    #[test]
    fn missing_state_root_yields_no_attestation() {
        let obs = observe_claude_review(None, pr(), &head(), &[], &[], &empty_threads());
        assert!(obs.attestation.is_none());
        assert!(obs.attest_path.is_none());
        assert!(obs.latest_claude_at.is_none());
    }

    #[test]
    fn missing_attestation_file_yields_none() {
        let dir = tempdir().unwrap();
        let obs =
            observe_claude_review(Some(dir.path()), pr(), &head(), &[], &[], &empty_threads());
        assert!(obs.attestation.is_none());
        assert!(obs.attest_path.is_some());
    }

    #[test]
    fn malformed_attestation_file_degrades_to_none() {
        let dir = tempdir().unwrap();
        let path = claude_review_attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let obs =
            observe_claude_review(Some(dir.path()), pr(), &head(), &[], &[], &empty_threads());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn round_trip_attestation_reads_back() {
        let dir = tempdir().unwrap();
        let path = claude_review_attest_path(dir.path(), pr());
        let written = write_claude_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs =
            observe_claude_review(Some(dir.path()), pr(), &head(), &[], &[], &empty_threads());
        let att = obs.attestation.expect("attestation present");
        assert_eq!(att, written);
        assert_eq!(att.version, CLAUDE_REVIEW_SCHEMA_VERSION);
    }

    #[test]
    fn aggregates_claude_review_submission_when_only_review_exists() {
        let r = claude_review(
            "2026-05-02T10:00:00Z",
            "🔴 important",
            "https://example/r/1",
        );
        let obs = observe_claude_review(None, pr(), &head(), &[r], &[], &empty_threads());
        assert!(obs.latest_claude_at.is_some());
        assert_eq!(obs.latest_claude_body.as_deref(), Some("🔴 important"));
        assert_eq!(
            obs.latest_claude_url.as_deref(),
            Some("https://example/r/1"),
        );
    }

    #[test]
    fn aggregates_claude_issue_comment_when_only_issue_exists() {
        let c = claude_issue("2026-05-02T10:00:00Z", "review!", "https://example/c/1");
        let obs = observe_claude_review(None, pr(), &head(), &[], &[c], &empty_threads());
        assert!(obs.latest_claude_at.is_some());
        assert_eq!(obs.latest_claude_body.as_deref(), Some("review!"));
        assert_eq!(
            obs.latest_claude_url.as_deref(),
            Some("https://example/c/1")
        );
    }

    #[test]
    fn review_body_wins_when_both_review_and_issue_present_review_newer() {
        let r = claude_review(
            "2026-05-03T10:00:00Z",
            "structured review",
            "https://example/r/2",
        );
        let c = claude_issue("2026-05-02T10:00:00Z", "older issue", "https://example/c/3");
        let obs = observe_claude_review(None, pr(), &head(), &[r], &[c], &empty_threads());
        assert_eq!(obs.latest_claude_body.as_deref(), Some("structured review"));
        assert_eq!(
            obs.latest_claude_url.as_deref(),
            Some("https://example/r/2")
        );
    }

    #[test]
    fn review_body_wins_even_when_issue_is_newer() {
        let review_at_s = "2026-05-01T10:00:00Z";
        let issue_at_s = "2026-05-05T10:00:00Z";
        let r = claude_review(review_at_s, "structured review", "https://example/r/2");
        let c = claude_issue(issue_at_s, "newer issue", "https://example/c/4");
        let obs = observe_claude_review(None, pr(), &head(), &[r], &[c], &empty_threads());
        // Body priority is review-over-issue; the latest_at is still
        // the maximum across surfaces so freshness comparison sees
        // the newer issue timestamp.
        assert_eq!(obs.latest_claude_body.as_deref(), Some("structured review"));
        assert_eq!(
            obs.latest_claude_url.as_deref(),
            Some("https://example/r/2")
        );
        let review_at = chrono::DateTime::parse_from_rfc3339(review_at_s)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let issue_at = chrono::DateTime::parse_from_rfc3339(issue_at_s)
            .unwrap()
            .with_timezone(&chrono::Utc);
        // latest_claude_at is the drift signal: max across surfaces.
        assert_eq!(
            obs.latest_claude_at,
            Some(std::cmp::max(review_at, issue_at))
        );
        // body_at is the timestamp of the SELECTED body — the review.
        assert_eq!(obs.body_at, Some(review_at));

        // Confirm the Witness label uses body_at (review_at), not
        // latest_claude_at (issue_at).
        let oriented = orient_claude_review(&obs);
        let ClaudeReview::Fresh {
            body_at: fresh_body_at,
            latest_claude_at: fresh_latest,
            latest_claude_body,
            latest_claude_url,
            inline_thread_count,
            ..
        } = oriented
        else {
            panic!("expected Fresh");
        };
        assert_eq!(fresh_body_at, review_at);
        assert_eq!(fresh_latest, issue_at);
        let prompt = crate::act::address_claude_review::build_address_claude_review_prompt(
            pr(),
            fresh_body_at,
            &latest_claude_body,
            &latest_claude_url,
            inline_thread_count,
            None,
        );
        let s = prompt.to_string();
        assert!(
            s.contains(&review_at.to_string()),
            "label should contain body_at (review timestamp): {s}",
        );
        assert!(
            !s.contains(&issue_at.to_string()),
            "label should NOT contain latest_claude_at (issue timestamp): {s}",
        );
    }

    #[test]
    fn non_claude_reviews_and_comments_are_filtered_out() {
        let r = PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
            }),
            state: ReviewState::Commented,
            commit_id: GitCommitSha::parse(VALID_SHA).unwrap(),
            submitted_at: Some(Timestamp::parse("2026-05-02T10:00:00Z").unwrap()),
            body: "copilot".into(),
            html_url: String::new(),
        };
        let c = other_issue("2026-05-03T10:00:00Z", "alice");
        let obs = observe_claude_review(None, pr(), &head(), &[r], &[c], &empty_threads());
        assert!(obs.latest_claude_at.is_none());
        assert!(obs.latest_claude_body.is_none());
    }

    #[test]
    fn inline_threads_counted_only_when_claude_authored_a_comment() {
        let claude_thread = vec!["alice", "claude[bot]"];
        let other_thread = vec!["alice", "bob"];
        // Bare `claude` login is intentionally rejected: a plain-user
        // account can register it, so only the `[bot]`-suffixed form
        // is trusted as the reviewer's identity.
        let only_claude = vec!["claude[bot]"];
        let threads = threads_with(&[claude_thread, other_thread, only_claude]);
        let obs = observe_claude_review(None, pr(), &head(), &[], &[], &threads);
        assert_eq!(obs.inline_thread_count, 2);
    }

    #[test]
    fn slug_is_unused_helper_does_not_panic() {
        // Defensive: slug() is exposed as a fixture for parity with
        // sibling axes; pin it to avoid dead-code drift.
        let _ = slug();
    }
}
