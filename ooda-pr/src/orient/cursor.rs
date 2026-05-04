//! Cursor orient: project Cursor Bugbot's reviews + check run +
//! threads into the per-PR state.
//!
//! Simpler state machine than Copilot: Cursor reviews atomically
//! (no separate request/ack/review timeline), so each Cursor review
//! is a round directly. The Bugbot check at HEAD is the platinum
//! signal.
//!
//! Activity-gated `Configured` semantics — returns `None` when no
//! cursor activity exists for this PR (no rounds and no check),
//! mirroring pr-fitness's contract. This is *the* asymmetry that
//! caused the false-stall bug class when Copilot used a different
//! contract; we hold the line here.

use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::checks::{CheckState, PullRequestCheck};
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use serde::Serialize;

use super::bot_threads::{BotThreadSummary, count_bot_threads};

// ── Identity ─────────────────────────────────────────────────────────

const CURSOR_LOGINS: &[&str] = &["cursor[bot]", "cursor"];
const CURSOR_CHECK_NAME: &str = "Cursor Bugbot";

pub fn is_cursor(login: &str) -> bool {
    CURSOR_LOGINS.contains(&login)
}

// ── Public types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorReport {
    pub activity: CursorActivity,
    /// All Cursor review rounds, oldest first.
    pub rounds: Vec<CursorReviewRound>,
    pub threads: BotThreadSummary,
    pub severity: CursorSeverityBreakdown,
    pub tier: CursorTier,
    /// Latest review observed at HEAD.
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum CursorActivity {
    Idle,
    Reviewing,
    Reviewed { latest: CursorReviewRound },
    Clean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorReviewRound {
    /// 1-indexed within this PR's Cursor review history.
    pub round: u32,
    pub reviewed_at: Timestamp,
    pub commit: GitCommitSha,
    pub findings_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct CursorSeverityBreakdown {
    pub high: u32,
    pub medium: u32,
    pub low: u32,
}

impl CursorSeverityBreakdown {
    /// `[\"2 high\", \"1 medium\"]` — only non-zero buckets, in order.
    /// Used to render compact severity summaries in prompts and
    /// PR comments.
    pub fn nonzero_parts(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if self.high > 0 {
            out.push(format!("{} high", self.high));
        }
        if self.medium > 0 {
            out.push(format!("{} medium", self.medium));
        }
        if self.low > 0 {
            out.push(format!("{} low", self.low));
        }
        out
    }
}

/// Same tier lattice as Copilot — kept as a separate type to prevent
/// accidental cross-bot comparison without explicit choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CursorTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

impl CursorTier {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Bronze => "bronze",
            Self::Silver => "silver",
            Self::Gold => "gold",
            Self::Platinum => "platinum",
        }
    }
}

// ── Public entry point ───────────────────────────────────────────────

/// Returns `None` when there is no Cursor activity for this PR (no
/// rounds and no Bugbot check). Activity-gated, not config-gated —
/// we don't have ruleset info for Cursor and don't need it: presence
/// of a check or review IS the per-PR engagement signal.
pub fn orient_cursor(
    reviews: &[PullRequestReview],
    threads: &ReviewThreadsResponse,
    checks: &[PullRequestCheck],
    head: &GitCommitSha,
) -> Option<CursorReport> {
    let rounds = correlate_rounds(reviews);
    let check = find_cursor_check(checks);

    if rounds.is_empty() && check.is_none() {
        return None;
    }

    let latest_reviewed_at = rounds.last().map(|r| r.reviewed_at);
    let thread_summary = count_bot_threads(threads, latest_reviewed_at.as_ref(), is_cursor);
    let severity = count_severity(threads);
    let activity = derive_activity(&rounds, check);
    let tier = score_tier(&rounds, &thread_summary, check);
    let fresh = is_fresh(&rounds, head);

    Some(CursorReport {
        activity,
        rounds,
        threads: thread_summary,
        severity,
        tier,
        fresh,
    })
}

// ── Body parsing ─────────────────────────────────────────────────────

/// Parse "found N potential issue(s)" from a Cursor review body.
pub(crate) fn parse_findings_count(body: &str) -> u32 {
    let prefix = "found ";
    let suffix = " potential issue";
    let Some(start) = body.find(prefix) else {
        return 0;
    };
    let rest = &body[start + prefix.len()..];
    let Some(end) = rest.find(suffix) else {
        return 0;
    };
    rest[..end].trim().parse().unwrap_or(0)
}

/// Parse the next severity tag — `**High Severity**` etc. — in a
/// thread comment body.
fn parse_severity(body: &str) -> Option<Severity> {
    let prefix = "**";
    let suffix = " Severity**";
    let start = body.find(prefix)? + prefix.len();
    let rest = &body[start..];
    let end = rest.find(suffix)?;
    match rest[..end].trim() {
        "High" => Some(Severity::High),
        "Medium" => Some(Severity::Medium),
        "Low" => Some(Severity::Low),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    High,
    Medium,
    Low,
}

// ── Round correlation ────────────────────────────────────────────────

fn correlate_rounds(reviews: &[PullRequestReview]) -> Vec<CursorReviewRound> {
    let mut sorted: Vec<&PullRequestReview> = reviews
        .iter()
        .filter(|r| r.user.as_ref().is_some_and(|u| is_cursor(u.login.as_str())))
        .collect();
    // Reviews without a submitted_at sort first (None < Some).
    sorted.sort_by_key(|a| a.submitted_at);

    sorted
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let reviewed_at = r.submitted_at?;
            Some(CursorReviewRound {
                round: i as u32 + 1,
                reviewed_at,
                commit: r.commit_id.clone(),
                findings_count: parse_findings_count(&r.body),
            })
        })
        .collect()
}

fn find_cursor_check(checks: &[PullRequestCheck]) -> Option<&PullRequestCheck> {
    checks.iter().find(|c| c.name.as_str() == CURSOR_CHECK_NAME)
}

// ── Severity counting ────────────────────────────────────────────────

/// Count severity tags across *unresolved* cursor-authored threads.
/// Resolved threads are excluded — the breakdown describes work left.
fn count_severity(threads: &ReviewThreadsResponse) -> CursorSeverityBreakdown {
    let mut s = CursorSeverityBreakdown::default();
    for t in &threads.data.repository.pull_request.review_threads.nodes {
        if t.is_resolved {
            continue;
        }
        let Some(first) = t.comments.nodes.first() else {
            continue;
        };
        let Some(author) = &first.author else {
            continue;
        };
        if !is_cursor(author.login.as_str()) {
            continue;
        }
        match parse_severity(&first.body) {
            Some(Severity::High) => s.high += 1,
            Some(Severity::Medium) => s.medium += 1,
            Some(Severity::Low) => s.low += 1,
            None => {}
        }
    }
    s
}

// ── Tier scoring ─────────────────────────────────────────────────────

/// Tier rules (first match wins):
///   bronze:   unresolved>0 OR (no rounds AND no check)
///   silver:   unresolved=0 AND check QUEUED/IN_PROGRESS AND prior round exists
///   gold:     unresolved=0 AND rounds present AND no in-flight check at HEAD
///   platinum: check at HEAD = SUCCESS
fn score_tier(
    rounds: &[CursorReviewRound],
    threads: &BotThreadSummary,
    check: Option<&PullRequestCheck>,
) -> CursorTier {
    if threads.unresolved > 0 {
        return CursorTier::Bronze;
    }
    match check.map(|c| c.state) {
        Some(CheckState::Success) => CursorTier::Platinum,
        // `Pending` joins Queued/InProgress as in-flight — Bugbot
        // hasn't completed yet, mirror the activity classification.
        Some(CheckState::Queued) | Some(CheckState::InProgress) | Some(CheckState::Pending) => {
            if rounds.is_empty() {
                CursorTier::Bronze
            } else {
                CursorTier::Silver
            }
        }
        _ => {
            if rounds.is_empty() {
                CursorTier::Bronze
            } else {
                CursorTier::Gold
            }
        }
    }
}

fn is_fresh(rounds: &[CursorReviewRound], head: &GitCommitSha) -> bool {
    rounds.last().is_some_and(|r| &r.commit == head)
}

// ── Activity derivation ──────────────────────────────────────────────

fn derive_activity(
    rounds: &[CursorReviewRound],
    check: Option<&PullRequestCheck>,
) -> CursorActivity {
    if let Some(c) = check {
        match c.state {
            // `Pending` is the same in-flight semantic as Queued/
            // InProgress for our purposes — the bot has work
            // outstanding. Without it, a PR with the Cursor check
            // sitting at PENDING and no prior Cursor round would
            // emit no WaitForCursorReview and let decide() halt
            // Success while Bugbot is still queued.
            CheckState::Queued | CheckState::InProgress | CheckState::Pending => {
                return CursorActivity::Reviewing;
            }
            CheckState::Success => return CursorActivity::Clean,
            _ => {}
        }
    }
    if let Some(latest) = rounds.last() {
        return CursorActivity::Reviewed {
            latest: latest.clone(),
        };
    }
    CursorActivity::Idle
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
    use crate::observe::github::reviews::{ReviewState, ReviewUser};

    const HEAD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }
    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }
    fn cursor_review(sha: &str, at: &str, body: &str) -> PullRequestReview {
        PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse("cursor[bot]").unwrap(),
            }),
            state: ReviewState::Commented,
            commit_id: GitCommitSha::parse(sha).unwrap(),
            submitted_at: Some(ts(at)),
            body: body.into(),
        }
    }
    fn cursor_check(state: CheckState) -> PullRequestCheck {
        PullRequestCheck {
            name: crate::ids::CheckName::parse(CURSOR_CHECK_NAME).unwrap(),
            state,
            description: String::new(),
            link: String::new(),
            completed_at: None,
        }
    }
    fn empty_threads() -> ReviewThreadsResponse {
        ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo {
                                has_next_page: false,
                                end_cursor: None,
                            },
                            nodes: vec![],
                        },
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        }
    }
    fn cursor_thread(resolved: bool, body: &str) -> ReviewThread {
        ReviewThread {
            id: String::new(),
            is_resolved: resolved,
            is_outdated: false,
            path: String::new(),
            line: None,
            comments: ThreadComments {
                page_info: Default::default(),
                nodes: vec![ThreadComment {
                    author: Some(CommentAuthor {
                        login: GitHubLogin::parse("cursor[bot]").unwrap(),
                    }),
                    created_at: ts("2026-04-23T10:00:00Z"),
                    body: body.into(),
                }],
            },
        }
    }
    fn threads_with(nodes: Vec<ReviewThread>) -> ReviewThreadsResponse {
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
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        }
    }

    // ── identity ──

    #[test]
    fn identity_recognizes_known_logins() {
        assert!(is_cursor("cursor[bot]"));
        assert!(is_cursor("cursor"));
        assert!(!is_cursor("Cursor"));
        assert!(!is_cursor("alice"));
    }

    // ── activity-gated configuration ──

    #[test]
    fn returns_none_when_no_rounds_and_no_check() {
        let r = orient_cursor(&[], &empty_threads(), &[], &head());
        assert!(r.is_none());
    }

    #[test]
    fn returns_some_when_check_exists_even_without_rounds() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &[cursor_check(CheckState::Queued)],
            &head(),
        );
        assert!(r.is_some());
    }

    #[test]
    fn returns_some_when_round_exists_even_without_check() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head());
        assert!(r.is_some());
    }

    // ── body parsing ──

    #[test]
    fn parses_singular_potential_issue() {
        assert_eq!(parse_findings_count("Cursor found 1 potential issue."), 1);
    }

    #[test]
    fn parses_plural_potential_issues() {
        assert_eq!(parse_findings_count("found 4 potential issues here"), 4);
    }

    #[test]
    fn returns_zero_when_pattern_absent() {
        assert_eq!(parse_findings_count("everything looks great"), 0);
    }

    // ── activity transitions ──

    #[test]
    fn activity_reviewing_when_check_in_progress() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &[cursor_check(CheckState::InProgress)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::Reviewing);
    }

    #[test]
    fn activity_clean_when_check_success() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &[cursor_check(CheckState::Success)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::Clean);
    }

    #[test]
    fn activity_reviewed_when_round_exists_no_active_check() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head()).unwrap();
        assert!(matches!(r.activity, CursorActivity::Reviewed { .. }));
    }

    // ── tier transitions ──

    #[test]
    fn tier_platinum_when_check_success_at_head() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &[cursor_check(CheckState::Success)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Platinum);
    }

    #[test]
    fn tier_silver_when_check_in_progress_with_prior_round() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &[cursor_check(CheckState::InProgress)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Silver);
    }

    #[test]
    fn tier_bronze_when_check_in_progress_with_no_round() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &[cursor_check(CheckState::InProgress)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Bronze);
    }

    #[test]
    fn tier_gold_when_round_exists_but_no_active_check() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head()).unwrap();
        assert_eq!(r.tier, CursorTier::Gold);
    }

    #[test]
    fn tier_bronze_when_unresolved_threads_present() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let threads = threads_with(vec![cursor_thread(false, "**High Severity** issue")]);
        let r = orient_cursor(
            &revs,
            &threads,
            &[cursor_check(CheckState::Success)],
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Bronze);
        assert_eq!(r.threads.unresolved, 1);
    }

    // ── severity counting ──

    #[test]
    fn severity_count_partitions_high_medium_low() {
        let threads = threads_with(vec![
            cursor_thread(false, "**High Severity** thing"),
            cursor_thread(false, "**Medium Severity** thing"),
            cursor_thread(false, "**Medium Severity** other"),
            cursor_thread(false, "**Low Severity** thing"),
            cursor_thread(true, "**High Severity** but resolved"), // excluded
        ]);
        let r =
            orient_cursor(&[], &threads, &[cursor_check(CheckState::Success)], &head()).unwrap();
        assert_eq!(r.severity.high, 1);
        assert_eq!(r.severity.medium, 2);
        assert_eq!(r.severity.low, 1);
    }

    #[test]
    fn severity_skips_threads_without_severity_tag() {
        let threads = threads_with(vec![cursor_thread(false, "no tag here")]);
        let r =
            orient_cursor(&[], &threads, &[cursor_check(CheckState::Success)], &head()).unwrap();
        assert_eq!(r.severity.high, 0);
        assert_eq!(r.severity.medium, 0);
        assert_eq!(r.severity.low, 0);
    }

    // ── round correlation ──

    #[test]
    fn rounds_sorted_by_submitted_at_with_indexing() {
        let revs = vec![
            cursor_review(OLD_SHA, "2026-04-23T11:00:00Z", "found 2 potential issues"),
            cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "found 0 potential issues"),
        ];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head()).unwrap();
        assert_eq!(r.rounds.len(), 2);
        assert_eq!(r.rounds[0].round, 1);
        assert_eq!(r.rounds[0].commit.as_str(), HEAD_SHA);
        assert_eq!(r.rounds[1].round, 2);
        assert_eq!(r.rounds[1].findings_count, 2);
    }

    #[test]
    fn fresh_true_when_latest_review_at_head() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head()).unwrap();
        assert!(r.fresh);
    }

    #[test]
    fn fresh_false_when_latest_review_at_old_sha() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(&revs, &empty_threads(), &[], &head()).unwrap();
        assert!(!r.fresh);
    }
}
