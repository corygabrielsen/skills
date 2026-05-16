//! Reviews orient: join `reviews × HEAD`, count unresolved threads,
//! split pending reviewers into bots vs humans, and surface the
//! decision.
//!
//! First axis to perform real joins across observation sources
//! (PR head SHA × per-review commit SHA; thread/request data from
//! GraphQL).

use crate::ids::{GitCommitSha, GitHubLogin, Reviewer, TeamName, Timestamp};
use crate::observe::github::comments::IssueComment;
use crate::observe::github::pull_request_view::{PullRequestView, ReviewDecision};
use crate::observe::github::requested_reviewers::{RequestedReviewers, UserType};
use crate::observe::github::review_threads::{RequestedReviewer, ReviewThreadsResponse};
use crate::observe::github::reviews::{PullRequestReview, ReviewState};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    /// `None` means "no review policy on this branch" — distinct from
    /// `Some(ReviewRequired)` which means policy exists and is unmet.
    pub decision: Option<ReviewDecision>,
    pub threads_unresolved: usize,
    pub threads_total: usize,
    /// Issue-level comments authored by bots (`login` ends with `[bot]`).
    pub bot_comments: usize,
    pub approvals_on_head: usize,
    pub approvals_stale: usize,
    pub pending_reviews: PendingReviews,
    pub bot_reviews: Vec<BotReview>,
    /// Currently-required reviewers from the REST
    /// `pulls/{n}/requested_reviewers` endpoint, split into bots and
    /// humans (users + teams). Decide's `RequestApproval` prompt
    /// surfaces these so the human knows who must approve. Distinct
    /// from `pending_reviews`, which is the GraphQL-projected slice
    /// of `review_requests` and may include reviewers GitHub has not
    /// yet promoted into the REST view (eventual consistency window).
    pub requested_reviewers: RequestedReviewerSet,
    /// Latest `CHANGES_REQUESTED` review authored by a human (not a
    /// `[bot]`-suffixed login). `None` when there is no human-authored
    /// change request on this PR — bot change requests fall to
    /// `bot_reviews` instead. Drives the `AddressChangeRequest`
    /// prompt's inline witness so the agent does not need a
    /// `gh pr view --json reviews` round-trip to see what was asked.
    pub latest_human_changes_requested: Option<HumanReview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RequestedReviewerSet {
    pub bots: Vec<GitHubLogin>,
    pub humans: Vec<Reviewer>,
}

impl RequestedReviewerSet {
    /// All reviewers concatenated (bots first, then humans), useful
    /// for "required reviewers: a, b, c" rendering.
    pub fn all(&self) -> Vec<Reviewer> {
        let mut out: Vec<Reviewer> = self.bots.iter().cloned().map(Reviewer::User).collect();
        out.extend(self.humans.iter().cloned());
        out
    }

    pub fn is_empty(&self) -> bool {
        self.bots.is_empty() && self.humans.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HumanReview {
    pub author: GitHubLogin,
    pub submitted_at: Option<Timestamp>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct PendingReviews {
    /// Bots always have logins (GraphQL Bot or User-with-`[bot]`-suffix).
    pub bots: Vec<GitHubLogin>,
    /// Humans may be users (logins) or teams (slugs).
    pub humans: Vec<Reviewer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotReview {
    pub user: GitHubLogin,
    pub state: ReviewState,
    pub submitted_at: Option<Timestamp>,
}

pub fn orient_reviews(
    pr: &PullRequestView,
    threads: &ReviewThreadsResponse,
    comments: &[IssueComment],
    reviews: &[PullRequestReview],
    requested: &RequestedReviewers,
) -> ReviewSummary {
    let pull = &threads.data.repository.pull_request;

    let threads_total = pull.review_threads.nodes.len();
    // Outdated threads (anchor line shifted away by rebase/amend) are
    // excluded from the actionable count: GitHub auto-collapses them
    // and the actor cannot meaningfully address them in code. They
    // remain `is_resolved=false` until someone clicks "Resolve" — that
    // hygiene is a separate concern from "should the actor act?"
    let threads_unresolved = pull
        .review_threads
        .nodes
        .iter()
        .filter(|t| !t.is_resolved && !t.is_outdated)
        .count();

    let bot_comments = comments.iter().filter(|c| c.user.login.is_bot()).count();

    let head = &pr.head_ref_oid;
    let (approvals_on_head, approvals_stale) = partition_approvals(reviews, head);

    let pending_reviews = pending_split(
        pull.review_requests
            .nodes
            .iter()
            .filter_map(|n| n.requested_reviewer.as_ref()),
    );

    let bot_reviews = reviews
        .iter()
        .filter_map(|r| {
            let login = &r.user.as_ref()?.login;
            login.is_bot().then(|| BotReview {
                user: login.clone(),
                state: r.state,
                submitted_at: r.submitted_at,
            })
        })
        .collect();

    // Latest human-authored CHANGES_REQUESTED. Walk reviews newest-
    // first (by submitted_at; reviews without a timestamp lose) and
    // pick the first non-bot ChangesRequested entry. Bots travel
    // through `bot_reviews` and are gated by the AddressThreads /
    // suppressed-finding paths instead.
    let latest_human_changes_requested = latest_human_change_request(reviews);

    let requested_reviewers = split_requested_reviewers(requested);

    ReviewSummary {
        decision: pr.review_decision,
        threads_unresolved,
        threads_total,
        bot_comments,
        approvals_on_head,
        approvals_stale,
        pending_reviews,
        bot_reviews,
        requested_reviewers,
        latest_human_changes_requested,
    }
}

fn latest_human_change_request(reviews: &[PullRequestReview]) -> Option<HumanReview> {
    reviews
        .iter()
        .filter(|r| r.state == ReviewState::ChangesRequested)
        .filter_map(|r| {
            let login = &r.user.as_ref()?.login;
            if login.is_bot() {
                return None;
            }
            Some(HumanReview {
                author: login.clone(),
                submitted_at: r.submitted_at,
                body: r.body.clone(),
            })
        })
        .max_by_key(|h| h.submitted_at)
}

/// Split the REST `requested_reviewers` payload into bots and humans.
/// `UserType::Bot` and `[bot]`-suffixed `User` logins both classify
/// as bots (matches `pending_split`'s legacy User-with-suffix path).
/// `UserType::Organization` is treated as a human team — GitHub
/// occasionally serves org identities here when a team request was
/// converted; surface as a `Reviewer::User` so the reviewer name
/// still renders.
fn split_requested_reviewers(requested: &RequestedReviewers) -> RequestedReviewerSet {
    let mut out = RequestedReviewerSet::default();
    for u in &requested.users {
        match u.user_type {
            UserType::Bot => out.bots.push(u.login.clone()),
            UserType::User | UserType::Mannequin | UserType::Organization => {
                if u.login.is_bot() {
                    out.bots.push(u.login.clone());
                } else {
                    out.humans.push(Reviewer::User(u.login.clone()));
                }
            }
        }
    }
    for t in &requested.teams {
        // Team slugs from the REST endpoint may include characters
        // (uppercase, dashes) that `TeamName::parse` accepts as-is;
        // when parsing fails the slug is surfaced as a raw user
        // login so the reviewer is still named in the prompt.
        match TeamName::parse(&t.slug) {
            Ok(name) => out.humans.push(Reviewer::Team(name)),
            Err(_) => {
                if let Ok(login) = GitHubLogin::parse(&t.slug) {
                    out.humans.push(Reviewer::User(login));
                }
            }
        }
    }
    out
}

fn partition_approvals(reviews: &[PullRequestReview], head: &GitCommitSha) -> (usize, usize) {
    let mut on_head = 0;
    let mut stale = 0;
    for r in reviews {
        if r.state != ReviewState::Approved {
            continue;
        }
        if &r.commit_id == head {
            on_head += 1;
        } else {
            stale += 1;
        }
    }
    (on_head, stale)
}

/// Split requested reviewers into bots and humans by typename:
///   - `Bot`: bot
///   - `User` / `Mannequin`: human (matches pr-fitness — Mannequin
///     is a placeholder identity for migrated humans)
///   - `Team`: human (team name carries a slug)
///
/// A `[bot]`-suffixed `User` login also counts as a bot so that
/// reviewers added before GraphQL knew about the Bot type still
/// classify correctly.
fn pending_split<'a>(reviewers: impl Iterator<Item = &'a RequestedReviewer>) -> PendingReviews {
    let mut out = PendingReviews::default();
    for r in reviewers {
        match r {
            RequestedReviewer::Bot { login } => out.bots.push(login.clone()),
            RequestedReviewer::User { login } => {
                if login.is_bot() {
                    out.bots.push(login.clone());
                } else {
                    out.humans.push(Reviewer::User(login.clone()));
                }
            }
            RequestedReviewer::Mannequin { login } => {
                out.humans.push(Reviewer::User(login.clone()));
            }
            RequestedReviewer::Team { name } => {
                out.humans.push(Reviewer::Team(name.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PullRequestNumber;
    use crate::observe::github::comments::{CommentUser, IssueComment};
    use crate::observe::github::pull_request_view::{
        MergeStateStatus, Mergeable, PullRequestState, PullRequestView,
    };
    use crate::observe::github::review_threads::{
        CommentAuthor, PageInfo, RequestedReviewer, ReviewRequestNode, ReviewRequestsPage,
        ReviewThread, ReviewThreadsData, ReviewThreadsPage, ReviewThreadsPr, ReviewThreadsRepo,
        ReviewThreadsResponse, ThreadComment, ThreadComments,
    };
    use crate::observe::github::reviews::{PullRequestReview, ReviewUser};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn pr() -> PullRequestView {
        PullRequestView {
            title: "x".into(),
            number: PullRequestNumber::new(1).unwrap(),
            url: "u".into(),
            body: None,
            state: PullRequestState::Open,
            is_draft: false,
            mergeable: Mergeable::Mergeable,
            merge_state_status: MergeStateStatus::Clean,
            head_ref_oid: GitCommitSha::parse(HEAD).unwrap(),
            base_ref_name: crate::ids::BranchName::parse("master").unwrap(),
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            closed_at: None,
            merged_at: None,
            review_decision: None,
            labels: vec![],
            assignees: vec![],
            review_requests: vec![],
            commits: vec![],
            author: None,
        }
    }

    fn threads(
        nodes: Vec<ReviewThread>,
        requests: Vec<ReviewRequestNode>,
    ) -> ReviewThreadsResponse {
        ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo {
                                has_next_page: false,
                                end_cursor: None,
                            },
                            nodes,
                        },
                        review_requests: ReviewRequestsPage { nodes: requests },
                    },
                },
            },
        }
    }

    fn thread(resolved: bool) -> ReviewThread {
        thread_full(resolved, false)
    }

    fn thread_full(resolved: bool, outdated: bool) -> ReviewThread {
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
                        login: GitHubLogin::parse("alice").unwrap(),
                    }),
                    created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                    body: "x".into(),
                }],
            },
        }
    }

    fn review(state: ReviewState, login: &str, sha: &str) -> PullRequestReview {
        PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse(login).unwrap(),
            }),
            state,
            commit_id: GitCommitSha::parse(sha).unwrap(),
            submitted_at: Some(Timestamp::parse("2026-04-23T10:00:00Z").unwrap()),
            body: String::new(),
            html_url: String::new(),
        }
    }

    fn comment(login: &str) -> IssueComment {
        IssueComment {
            id: 1,
            user: CommentUser {
                login: GitHubLogin::parse(login).unwrap(),
            },
            body: String::new(),
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            html_url: String::new(),
        }
    }

    fn req(rev: RequestedReviewer) -> ReviewRequestNode {
        ReviewRequestNode {
            requested_reviewer: Some(rev),
        }
    }

    #[test]
    fn empty_inputs_yield_empty_summary() {
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.decision, None);
        assert_eq!(s.threads_total, 0);
        assert_eq!(s.threads_unresolved, 0);
        assert_eq!(s.bot_comments, 0);
        assert_eq!(s.approvals_on_head, 0);
        assert_eq!(s.approvals_stale, 0);
        assert!(s.pending_reviews.bots.is_empty());
        assert!(s.pending_reviews.humans.is_empty());
        assert!(s.bot_reviews.is_empty());
        assert!(s.requested_reviewers.is_empty());
        assert!(s.latest_human_changes_requested.is_none());
    }

    #[test]
    fn unresolved_thread_count_excludes_resolved() {
        let nodes = vec![thread(true), thread(false), thread(false), thread(true)];
        let s = orient_reviews(
            &pr(),
            &threads(nodes, vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.threads_total, 4);
        assert_eq!(s.threads_unresolved, 2);
    }

    #[test]
    fn unresolved_thread_count_excludes_outdated() {
        // 5 open threads: 4 outdated (rebase shifted lines), 1 live.
        // Mirror of the smoke-test scenario on infrastructure#702.
        let nodes = vec![
            thread_full(false, true),  // outdated
            thread_full(false, true),  // outdated
            thread_full(false, false), // live
            thread_full(false, true),  // outdated
            thread_full(false, true),  // outdated
        ];
        let s = orient_reviews(
            &pr(),
            &threads(nodes, vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.threads_total, 5);
        assert_eq!(s.threads_unresolved, 1);
    }

    #[test]
    fn approvals_partitioned_by_head_sha() {
        let revs = vec![
            review(ReviewState::Approved, "alice", HEAD),
            review(ReviewState::Approved, "bob", OLD), // stale
            review(ReviewState::Approved, "carol", HEAD),
            review(ReviewState::Commented, "dave", HEAD), // not an approval
        ];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &[],
            &revs,
            &RequestedReviewers::default(),
        );
        assert_eq!(s.approvals_on_head, 2);
        assert_eq!(s.approvals_stale, 1);
    }

    #[test]
    fn bot_comments_count_by_login_suffix() {
        let cs = vec![
            comment("alice"),
            comment("copilot[bot]"),
            comment("dependabot[bot]"),
            comment("bob"),
        ];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &cs,
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.bot_comments, 2);
    }

    #[test]
    fn pending_reviews_split_by_typename() {
        let nodes = vec![
            req(RequestedReviewer::User {
                login: GitHubLogin::parse("alice").unwrap(),
            }),
            req(RequestedReviewer::Bot {
                login: GitHubLogin::parse("dependabot[bot]").unwrap(),
            }),
            req(RequestedReviewer::Team {
                name: crate::ids::TeamName::parse("backend").unwrap(),
            }),
            req(RequestedReviewer::Mannequin {
                login: GitHubLogin::parse("ghost").unwrap(),
            }),
            // User typename + bot suffix (legacy classification path)
            req(RequestedReviewer::User {
                login: GitHubLogin::parse("copilot[bot]").unwrap(),
            }),
        ];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], nodes),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        let bot_strs: Vec<String> = s
            .pending_reviews
            .bots
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(bot_strs, vec!["dependabot[bot]", "copilot[bot]"]);
        let human_strs: Vec<String> = s
            .pending_reviews
            .humans
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(human_strs, vec!["alice", "backend", "ghost"]);
    }

    #[test]
    fn null_requested_reviewer_skipped() {
        let nodes = vec![ReviewRequestNode {
            requested_reviewer: None,
        }];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], nodes),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert!(s.pending_reviews.bots.is_empty());
        assert!(s.pending_reviews.humans.is_empty());
    }

    #[test]
    fn bot_reviews_collect_only_bot_authored_reviews() {
        let revs = vec![
            review(ReviewState::Approved, "alice", HEAD),
            review(ReviewState::Commented, "copilot[bot]", HEAD),
            review(ReviewState::ChangesRequested, "cursor[bot]", OLD),
        ];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &[],
            &revs,
            &RequestedReviewers::default(),
        );
        assert_eq!(s.bot_reviews.len(), 2);
        assert_eq!(s.bot_reviews[0].user.as_str(), "copilot[bot]");
        assert_eq!(s.bot_reviews[1].state, ReviewState::ChangesRequested);
    }

    #[test]
    fn decision_passes_through_review_decision() {
        let mut p = pr();
        p.review_decision = Some(ReviewDecision::Approved);
        let s = orient_reviews(
            &p,
            &threads(vec![], vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.decision, Some(ReviewDecision::Approved));

        p.review_decision = Some(ReviewDecision::ChangesRequested);
        let s = orient_reviews(
            &p,
            &threads(vec![], vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.decision, Some(ReviewDecision::ChangesRequested));
    }

    #[test]
    fn decision_none_when_no_review_policy() {
        let p = pr(); // review_decision: None
        let s = orient_reviews(
            &p,
            &threads(vec![], vec![]),
            &[],
            &[],
            &RequestedReviewers::default(),
        );
        assert_eq!(s.decision, None);
    }

    // ── widening tests: requested_reviewers + latest_human_change_request ──

    fn req_user(
        login: &str,
        kind: UserType,
    ) -> crate::observe::github::requested_reviewers::RequestedUser {
        crate::observe::github::requested_reviewers::RequestedUser {
            login: GitHubLogin::parse(login).unwrap(),
            user_type: kind,
        }
    }

    fn req_team(slug: &str) -> crate::observe::github::requested_reviewers::RequestedTeam {
        crate::observe::github::requested_reviewers::RequestedTeam { slug: slug.into() }
    }

    #[test]
    fn requested_reviewers_split_by_user_type_and_login_suffix() {
        let requested = RequestedReviewers {
            users: vec![
                req_user("alice", UserType::User),
                req_user("dependabot[bot]", UserType::Bot),
                req_user("copilot[bot]", UserType::User), // suffix wins
                req_user("ghost", UserType::Mannequin),
            ],
            teams: vec![req_team("backend")],
        };
        let s = orient_reviews(&pr(), &threads(vec![], vec![]), &[], &[], &requested);
        let bot_strs: Vec<String> = s
            .requested_reviewers
            .bots
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(bot_strs, vec!["dependabot[bot]", "copilot[bot]"]);
        let human_strs: Vec<String> = s
            .requested_reviewers
            .humans
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(human_strs, vec!["alice", "ghost", "backend"]);
        assert!(!s.requested_reviewers.is_empty());
    }

    #[test]
    fn latest_human_change_request_picks_newest_by_submitted_at() {
        let revs = vec![
            // Old human change request.
            PullRequestReview {
                user: Some(ReviewUser {
                    login: GitHubLogin::parse("alice").unwrap(),
                }),
                state: ReviewState::ChangesRequested,
                commit_id: GitCommitSha::parse(HEAD).unwrap(),
                submitted_at: Some(Timestamp::parse("2026-04-20T10:00:00Z").unwrap()),
                body: "old feedback".into(),
                html_url: String::new(),
            },
            // Bot change request — must be ignored.
            PullRequestReview {
                user: Some(ReviewUser {
                    login: GitHubLogin::parse("copilot[bot]").unwrap(),
                }),
                state: ReviewState::ChangesRequested,
                commit_id: GitCommitSha::parse(HEAD).unwrap(),
                submitted_at: Some(Timestamp::parse("2026-04-23T10:00:00Z").unwrap()),
                body: "bot feedback".into(),
                html_url: String::new(),
            },
            // Latest human change request — must win.
            PullRequestReview {
                user: Some(ReviewUser {
                    login: GitHubLogin::parse("bob").unwrap(),
                }),
                state: ReviewState::ChangesRequested,
                commit_id: GitCommitSha::parse(HEAD).unwrap(),
                submitted_at: Some(Timestamp::parse("2026-04-22T10:00:00Z").unwrap()),
                body: "newer feedback".into(),
                html_url: String::new(),
            },
        ];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &[],
            &revs,
            &RequestedReviewers::default(),
        );
        let h = s
            .latest_human_changes_requested
            .as_ref()
            .expect("latest human CR must surface");
        assert_eq!(h.author.as_str(), "bob");
        assert_eq!(h.body, "newer feedback");
    }

    #[test]
    fn latest_human_change_request_none_when_only_bot_change_request() {
        let revs = vec![PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse("copilot[bot]").unwrap(),
            }),
            state: ReviewState::ChangesRequested,
            commit_id: GitCommitSha::parse(HEAD).unwrap(),
            submitted_at: Some(Timestamp::parse("2026-04-23T10:00:00Z").unwrap()),
            body: "bot feedback".into(),
            html_url: String::new(),
        }];
        let s = orient_reviews(
            &pr(),
            &threads(vec![], vec![]),
            &[],
            &revs,
            &RequestedReviewers::default(),
        );
        assert!(s.latest_human_changes_requested.is_none());
    }
}
