//! Project a push-driven reviewer's check-suite, check-run, and
//! review submissions into a per-PR activity state.
//!
//! # Invariants
//!
//! - **Push-driven, not request-driven**: this reviewer self-elects
//!   on push; there is no request-event in the lifecycle and no axis
//!   action remediates a stalled queue (the reviewer's own backend
//!   already holds the work).
//! - **Binary in-flight health**: no degraded intermediate. Healthy
//!   or Failed — without a remediation API, a degraded variant
//!   carries no actionable distinction from Failed.
//! - **First-class non-presence**: the reviewer explicitly declines
//!   some authors and some repos server-side, so declination is a
//!   typed lifecycle state, not an absence. Repo-level absence and
//!   per-PR declination are distinct variants so post-hoc analysis
//!   can tell them apart.
//! - **Domain-honest shape**: the activity lattice diverges from
//!   request-driven reviewer axes by design — when domains diverge,
//!   accept divergence rather than force a meta-structure.

use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::cursor_status::{
    CheckRunConclusion, CheckRunStatus, CheckSuiteStatus, CursorCheckSuite, CursorStatus,
};
use crate::observe::github::pull_request_view::PullRequestAuthor;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use serde::Serialize;

use super::bot_threads::{BotThreadSummary, count_bot_threads};

// ── Identity ─────────────────────────────────────────────────────────

const CURSOR_LOGINS: &[&str] = &["cursor[bot]", "cursor"];

pub(crate) fn is_cursor(login: &str) -> bool {
    CURSOR_LOGINS.contains(&login)
}

/// Login slugs the reviewer's server-side filter declines on author
/// class. Covers both the bare and bot-suffixed forms because the
/// host's GraphQL and REST surfaces emit different shapes for the
/// same identity.
const BOT_AUTHOR_SLUGS: &[&str] = &[
    "dependabot[bot]",
    "dependabot",
    "renovate[bot]",
    "renovate",
    "github-actions[bot]",
    "github-actions",
];

fn is_bot_author(author: &PullRequestAuthor) -> bool {
    author
        .login
        .as_ref()
        .is_some_and(|l| BOT_AUTHOR_SLUGS.contains(&l.as_str()))
}

// ── Stall threshold ──────────────────────────────────────────────────

/// Maximum dwell from suite creation (or run start) to a terminal
/// state before the in-flight stage classifies as Failed. Sized at
/// ~2× the observed p95 pickup latency over a multi-repo sample —
/// pads well above legitimate pickups while catching the canonical
/// stuck-suite pattern.
pub(crate) const STALL_TIMEOUT: chrono::Duration = chrono::Duration::minutes(15);

// ── Public types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CursorReport {
    pub activity: CursorActivity,
    /// All Cursor review rounds, oldest first. Empty when Cursor has
    /// not yet submitted a review at any HEAD on this PR.
    pub rounds: Vec<CursorReviewRound>,
    pub threads: BotThreadSummary,
    pub severity: CursorSeverityBreakdown,
    pub tier: CursorTier,
    /// Latest review observed at HEAD (`latest.commit == head`).
    pub fresh: bool,
    /// Suite-creation timestamp when a suite has been observed.
    /// Surfaced so the escalation prompt anchors the stall in
    /// absolute time rather than only naming the threshold.
    pub suite_created_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum CursorActivity {
    /// Repo-level absence — no suite ever observed and no per-PR
    /// declination signal.
    NotApplicable,
    /// Per-PR declination — distinct from repo-level absence so
    /// post-hoc analysis can identify which class of refusal fired.
    Skipped(SkipReason),
    /// Suite (and optionally run) is in flight. Health is binary;
    /// see [`InFlightHealth`].
    InFlight(InFlightHealth),
    /// Run reached a terminal state on this HEAD.
    Reviewed(ReviewedState),
}

// Variants are emitted selectively — `AuthorClass` is the only one
// the present classifier produces. `RepoConfig` and `Unknown` are
// reserved for paths that gain evidence (config probe, additional
// disambiguators); their presence locks the wire schema against
// silent rename when those paths land.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum SkipReason {
    /// Author belongs to the reviewer's bot-class filter. Detected at
    /// the observe boundary; the activity classifier never sees a
    /// missing-check it cannot explain on such PRs.
    AuthorClass,
    /// Repo-level opt-out via reviewer-side configuration.
    RepoConfig,
    /// Suite absent and no positive signal disambiguates between
    /// opt-out, seat-coverage gap, or silent backend failure. Catch-
    /// all that prevents false-positive in-flight classification.
    Unknown,
}

// Binary by design: with no remediation API, a degraded intermediate
// carries no actionable distinction from Failed — threshold trip goes
// straight to escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum InFlightHealth {
    /// Within `STALL_TIMEOUT` of the most-specific available anchor:
    /// run-start when present, else suite-creation.
    Healthy,
    /// `STALL_TIMEOUT` elapsed. Decide escalates to human handoff.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum ReviewedState {
    /// Run completed without findings posted as a review on this
    /// commit.
    Clean,
    /// Run completed with findings on this commit. Resolved either
    /// by a positive conclusion plus a co-located review row, or by
    /// disambiguating an overloaded conclusion via the same cross-
    /// ref. Remediation is handled by the generic threads axis.
    HasFindings,
}

/// Project the activity into a dashboard signal. `NotApplicable`
/// returns `None` so the dashboard does not emit a row when the
/// reviewer is silent on this PR.
pub(crate) fn cursor_signal(activity: &CursorActivity) -> Option<crate::dashboard::AxisSignal> {
    use crate::dashboard::{AxisName, AxisSignal, SignalIcon};
    let (icon, summary) = match activity {
        CursorActivity::NotApplicable => return None,
        CursorActivity::Skipped(reason) => (
            SignalIcon::NotApplicable,
            format!("skipped ({})", skip_reason_label(*reason)),
        ),
        CursorActivity::InFlight(InFlightHealth::Healthy) => {
            (SignalIcon::InFlight, "reviewing".to_string())
        }
        CursorActivity::InFlight(InFlightHealth::Failed) => (
            SignalIcon::Failed,
            "check_suite stalled — escalating".to_string(),
        ),
        CursorActivity::Reviewed(ReviewedState::Clean) => {
            (SignalIcon::Ok, "no findings".to_string())
        }
        CursorActivity::Reviewed(ReviewedState::HasFindings) => {
            (SignalIcon::Warn, "findings to address".to_string())
        }
    };
    Some(AxisSignal {
        axis: AxisName::Cursor,
        icon,
        summary,
    })
}

fn skip_reason_label(r: SkipReason) -> &'static str {
    match r {
        SkipReason::AuthorClass => "author class",
        SkipReason::RepoConfig => "repo config",
        SkipReason::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CursorReviewRound {
    /// 1-indexed within this PR's Cursor review history.
    pub round: u32,
    pub reviewed_at: Timestamp,
    pub commit: GitCommitSha,
    pub findings_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub(crate) struct CursorSeverityBreakdown {
    pub high: u32,
    pub medium: u32,
    pub low: u32,
}

impl CursorSeverityBreakdown {
    /// Compact severity rendering — non-zero buckets only, in
    /// descending-severity order. Used by prompt and PR-comment
    /// renderers.
    pub(crate) fn nonzero_parts(&self) -> Vec<String> {
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

/// Reviewer-specific tier lattice. Distinct type from sibling
/// reviewer tiers so cross-reviewer comparison is an explicit
/// choice at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CursorTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

impl CursorTier {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Bronze => "bronze",
            Self::Silver => "silver",
            Self::Gold => "gold",
            Self::Platinum => "platinum",
        }
    }
}

// ── Public entry point ───────────────────────────────────────────────

/// Project per-HEAD reviewer signal into the orient state.
///
/// Returns `Some` once any signal exists (suite, round, or author);
/// returns `None` when no observation source contributes anything.
/// The Option layer is preserved so downstream consumers can
/// distinguish "no observation at all" from "observation says
/// `NotApplicable`."
#[allow(clippy::too_many_arguments)]
pub(crate) fn orient_cursor(
    reviews: &[PullRequestReview],
    threads: &ReviewThreadsResponse,
    cursor_status: &CursorStatus,
    author: Option<&PullRequestAuthor>,
    head: &GitCommitSha,
    now: Timestamp,
) -> Option<CursorReport> {
    let rounds = correlate_rounds(reviews);

    // No signal → no report; preserves the "no observation" vs
    // "NotApplicable observation" distinction for downstream
    // consumers.
    if rounds.is_empty() && cursor_status.suite.is_none() && author.is_none() {
        return None;
    }

    let latest_reviewed_at = rounds.last().map(|r| r.reviewed_at);
    let thread_summary = count_bot_threads(threads, latest_reviewed_at.as_ref(), is_cursor);
    let severity = count_severity(threads);
    let has_review_row_at_head = reviews_at_head(reviews, head);
    let activity = derive_activity(cursor_status, author, has_review_row_at_head, now);
    let tier = score_tier(&rounds, &thread_summary, cursor_status);
    let fresh = is_fresh(&rounds, head);
    let suite_created_at = cursor_status.suite.as_ref().map(|s| s.created_at);

    Some(CursorReport {
        activity,
        rounds,
        threads: thread_summary,
        severity,
        tier,
        fresh,
        suite_created_at,
    })
}

// ── Body parsing ─────────────────────────────────────────────────────

/// Extract the integer finding count from a review body.
///
/// Class invariant: the digit-run immediately following the count
/// prefix is the authoritative terminator. Surrounding-noun matching
/// is brittle to wording drift; the digit run is not.
pub(crate) fn parse_findings_count(body: &str) -> u32 {
    let Some(start) = body.find("found ") else {
        return 0;
    };
    let rest = &body[start + "found ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

/// Extract the leading severity tag from a thread body.
///
/// Class invariant: positional-earliest literal wins. Delimiter-
/// shape matching (find an opener, expect a literal between
/// closers) admits false anchors from any other emphasis span;
/// positional-min over literal scans is robust to that.
fn parse_severity(body: &str) -> Option<Severity> {
    [
        ("**High Severity**", Severity::High),
        ("**Medium Severity**", Severity::Medium),
        ("**Low Severity**", Severity::Low),
    ]
    .iter()
    .filter_map(|(needle, sev)| body.find(needle).map(|pos| (pos, *sev)))
    .min_by_key(|&(pos, _)| pos)
    .map(|(_, sev)| sev)
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
                // Review-round index fits in u32: bounded by GitHub's
                // per-PR review history (orders of magnitude < 4B).
                round: u32::try_from(i).expect("review round index fits in u32") + 1,
                reviewed_at,
                commit: r.commit_id.clone(),
                findings_count: parse_findings_count(&r.body),
            })
        })
        .collect()
}

/// Witness predicate: a co-located review row anchored to HEAD.
/// Disambiguates the overloaded conclusion arm and witnesses
/// findings-present on the success arm.
fn reviews_at_head(reviews: &[PullRequestReview], head: &GitCommitSha) -> bool {
    reviews.iter().any(|r| {
        r.commit_id == *head && r.user.as_ref().is_some_and(|u| is_cursor(u.login.as_str()))
    })
}

// ── Severity counting ────────────────────────────────────────────────

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

/// Tier classification, first-match-wins:
///   bronze:   actionable threads present OR no rounds and no suite
///   silver:   no actionable threads, suite still in flight, prior round
///   gold:     no actionable threads, rounds present, no in-flight suite
///   platinum: suite completed with a success-conclusion run at HEAD
fn score_tier(
    rounds: &[CursorReviewRound],
    threads: &BotThreadSummary,
    cursor_status: &CursorStatus,
) -> CursorTier {
    if threads.unresolved > 0 {
        return CursorTier::Bronze;
    }
    let suite_status = cursor_status.suite.as_ref().map(|s| s.status);
    let run_conclusion = cursor_status
        .run
        .as_ref()
        .and_then(|r| r.conclusion)
        .filter(|_| {
            cursor_status
                .run
                .as_ref()
                .is_some_and(|r| r.status == CheckRunStatus::Completed)
        });
    match (suite_status, run_conclusion) {
        // Completed with a success run at HEAD — platinum.
        (Some(CheckSuiteStatus::Completed), Some(CheckRunConclusion::Success)) => {
            CursorTier::Platinum
        }
        // In-flight (queued / in-progress at suite level OR run level).
        (Some(CheckSuiteStatus::Queued | CheckSuiteStatus::InProgress), _) => {
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
    cursor_status: &CursorStatus,
    author: Option<&PullRequestAuthor>,
    has_review_row_at_head: bool,
    now: Timestamp,
) -> CursorActivity {
    // The canonical stall has no child run — it is suite-only and
    // therefore invisible to name-filtered check observation. The
    // suite source is the only one that can witness it.
    let Some(suite) = cursor_status.suite.as_ref() else {
        return classify_no_suite(author);
    };

    match cursor_status.run.as_ref() {
        // No child run yet — the canonical stuck-suite signature.
        None => classify_suite_without_run(suite, now),
        Some(run) => match (run.status, run.conclusion) {
            (CheckRunStatus::Completed, Some(concl)) => {
                classify_completed(concl, has_review_row_at_head)
            }
            // Eventual-consistency window: status reaches a terminal
            // value before conclusion populates. Treated as still-
            // running so the next observation tick reconciles.
            (CheckRunStatus::Completed, None) => {
                in_flight_health_from_run_anchor(run.started_at, suite.created_at, now)
            }
            // Queued, InProgress, Pending — apply the stall timeout.
            (
                CheckRunStatus::Queued
                | CheckRunStatus::InProgress
                | CheckRunStatus::Pending
                | CheckRunStatus::Unknown,
                _,
            ) => in_flight_health_from_run_anchor(run.started_at, suite.created_at, now),
        },
    }
}

// No-suite branch: classify the absence rather than collapse it to
// the in-flight lattice. Bot-class authorship witnesses a per-PR
// declination; everything else (no author, non-bot author with no
// suite) routes to repo-level absence — the classifier has no
// positive evidence the reviewer is active here.
fn classify_no_suite(author: Option<&PullRequestAuthor>) -> CursorActivity {
    match author {
        Some(a) if is_bot_author(a) => CursorActivity::Skipped(SkipReason::AuthorClass),
        Some(_) | None => CursorActivity::NotApplicable,
    }
}

fn classify_suite_without_run(suite: &CursorCheckSuite, now: Timestamp) -> CursorActivity {
    match suite.status {
        // Suite terminal without ever spawning a run is a backend-
        // cancellation signature — no run means no possible review
        // on this HEAD. Failed by construction.
        CheckSuiteStatus::Completed => CursorActivity::InFlight(InFlightHealth::Failed),
        // Still-pending suite: classify by the stall threshold from
        // suite creation.
        CheckSuiteStatus::Queued | CheckSuiteStatus::InProgress | CheckSuiteStatus::Unknown => {
            if now.at() - suite.created_at.at() >= STALL_TIMEOUT {
                CursorActivity::InFlight(InFlightHealth::Failed)
            } else {
                CursorActivity::InFlight(InFlightHealth::Healthy)
            }
        }
    }
}

// The neutral arm is overloaded — findings-posted vs backend-cancel
// share the same wire value. Disambiguate via the co-located-review-
// row predicate: presence witnesses findings, absence routes to
// failure.
fn classify_completed(
    conclusion: CheckRunConclusion,
    has_review_row_at_head: bool,
) -> CursorActivity {
    match conclusion {
        CheckRunConclusion::Success => {
            if has_review_row_at_head {
                CursorActivity::Reviewed(ReviewedState::HasFindings)
            } else {
                CursorActivity::Reviewed(ReviewedState::Clean)
            }
        }
        CheckRunConclusion::Neutral => {
            if has_review_row_at_head {
                CursorActivity::Reviewed(ReviewedState::HasFindings)
            } else {
                CursorActivity::InFlight(InFlightHealth::Failed)
            }
        }
        CheckRunConclusion::Failure
        | CheckRunConclusion::TimedOut
        | CheckRunConclusion::Cancelled
        | CheckRunConclusion::StartupFailure => CursorActivity::InFlight(InFlightHealth::Failed),
        // Reviewer-declined-to-act conclusions. Collapse to Clean so
        // the loop does not loop on a non-actionable signal.
        CheckRunConclusion::Skipped
        | CheckRunConclusion::ActionRequired
        | CheckRunConclusion::Stale
        | CheckRunConclusion::Unknown => CursorActivity::Reviewed(ReviewedState::Clean),
    }
}

/// Classify in-flight health against the most-specific timing
/// anchor available: run-start when populated, otherwise suite-
/// creation. Threshold-cross routes to Failed; under-threshold
/// routes to Healthy.
fn in_flight_health_from_run_anchor(
    started_at: Option<Timestamp>,
    suite_created_at: Timestamp,
    now: Timestamp,
) -> CursorActivity {
    let anchor = started_at.unwrap_or(suite_created_at);
    if now.at() - anchor.at() >= STALL_TIMEOUT {
        CursorActivity::InFlight(InFlightHealth::Failed)
    } else {
        CursorActivity::InFlight(InFlightHealth::Healthy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitHubLogin;
    use crate::observe::github::cursor_status::{CursorCheckRun, CursorCheckSuite, CursorStatus};
    use crate::observe::github::pull_request_view::PullRequestAuthor;
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
    fn now() -> Timestamp {
        ts("2026-04-23T10:05:00Z")
    }
    fn human_author() -> PullRequestAuthor {
        PullRequestAuthor {
            login: Some(GitHubLogin::parse("alice").unwrap()),
        }
    }
    fn bot_author() -> PullRequestAuthor {
        PullRequestAuthor {
            login: Some(GitHubLogin::parse("dependabot[bot]").unwrap()),
        }
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
            html_url: String::new(),
        }
    }
    fn empty_status() -> CursorStatus {
        CursorStatus {
            suite: None,
            run: None,
        }
    }
    fn status_suite_only(status: CheckSuiteStatus, created_at: &str) -> CursorStatus {
        CursorStatus {
            suite: Some(CursorCheckSuite {
                status,
                created_at: ts(created_at),
            }),
            run: None,
        }
    }
    fn status_with_run(
        suite_status: CheckSuiteStatus,
        suite_created_at: &str,
        run_status: CheckRunStatus,
        conclusion: Option<CheckRunConclusion>,
        started_at: Option<&str>,
    ) -> CursorStatus {
        CursorStatus {
            suite: Some(CursorCheckSuite {
                status: suite_status,
                created_at: ts(suite_created_at),
            }),
            run: Some(CursorCheckRun {
                status: run_status,
                conclusion,
                started_at: started_at.map(ts),
            }),
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
                page_info: PageInfo::default(),
                nodes: vec![ThreadComment {
                    database_id: None,
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

    #[test]
    fn bot_author_recognises_dependabot_renovate_actions() {
        for login in [
            "dependabot[bot]",
            "renovate[bot]",
            "github-actions[bot]",
            "dependabot",
        ] {
            let a = PullRequestAuthor {
                login: Some(GitHubLogin::parse(login).unwrap()),
            };
            assert!(is_bot_author(&a), "should match: {login}");
        }
        let alice = PullRequestAuthor {
            login: Some(GitHubLogin::parse("alice").unwrap()),
        };
        assert!(!is_bot_author(&alice));
    }

    // ── early-return Option ──

    #[test]
    fn returns_none_when_no_signal_at_all() {
        let r = orient_cursor(&[], &empty_threads(), &empty_status(), None, &head(), now());
        assert!(r.is_none());
    }

    #[test]
    fn returns_some_when_suite_exists() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_suite_only(CheckSuiteStatus::Queued, "2026-04-23T10:00:00Z"),
            None,
            &head(),
            now(),
        );
        assert!(r.is_some());
    }

    #[test]
    fn returns_some_when_round_exists() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &empty_status(),
            None,
            &head(),
            now(),
        );
        assert!(r.is_some());
    }

    // ── NotApplicable / Skipped classification ──

    #[test]
    fn no_suite_with_human_author_is_not_applicable() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &empty_status(),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::NotApplicable);
    }

    #[test]
    fn no_suite_with_bot_author_is_skipped_author_class() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &empty_status(),
            Some(&bot_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::Skipped(SkipReason::AuthorClass),);
    }

    // ── InFlight × Healthy/Failed ──

    #[test]
    fn suite_queued_within_threshold_is_in_flight_healthy() {
        // Suite created 5min ago, now=10:05 → 5min < 15min threshold.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_suite_only(CheckSuiteStatus::Queued, "2026-04-23T10:00:00Z"),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CursorActivity::InFlight(InFlightHealth::Healthy),
        );
    }

    #[test]
    fn suite_queued_past_threshold_is_in_flight_failed() {
        // Suite created 30min before now → past 15min threshold.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_suite_only(CheckSuiteStatus::Queued, "2026-04-23T09:35:00Z"),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    #[test]
    fn run_in_progress_within_threshold_is_healthy() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::InProgress,
                "2026-04-23T09:55:00Z",
                CheckRunStatus::InProgress,
                None,
                Some("2026-04-23T09:58:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CursorActivity::InFlight(InFlightHealth::Healthy),
        );
    }

    #[test]
    fn run_in_progress_past_run_threshold_is_failed() {
        // Run started 30min ago → past threshold from per-run anchor.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::InProgress,
                "2026-04-23T09:00:00Z",
                CheckRunStatus::InProgress,
                None,
                Some("2026-04-23T09:35:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    #[test]
    fn run_pending_without_started_at_falls_back_to_suite_created_at() {
        // No started_at; suite created 20min ago → past threshold.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Queued,
                "2026-04-23T09:45:00Z",
                CheckRunStatus::Queued,
                None,
                None,
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    // ── Reviewed × Clean/HasFindings ──

    #[test]
    fn completed_success_with_no_review_row_is_clean() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::Reviewed(ReviewedState::Clean));
    }

    #[test]
    fn completed_success_with_review_row_at_head_is_has_findings() {
        let revs = vec![cursor_review(
            HEAD_SHA,
            "2026-04-23T10:02:00Z",
            "Cursor found 2 potential issues",
        )];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CursorActivity::Reviewed(ReviewedState::HasFindings),
        );
    }

    #[test]
    fn completed_neutral_with_review_row_disambiguates_to_has_findings() {
        let revs = vec![cursor_review(
            HEAD_SHA,
            "2026-04-23T10:02:00Z",
            "found 1 potential issue",
        )];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Neutral),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CursorActivity::Reviewed(ReviewedState::HasFindings),
        );
    }

    #[test]
    fn completed_neutral_without_review_row_is_in_flight_failed() {
        // Backend cancel signature: completed neutral with no review
        // row on this commit. Disambiguation says Failed, not Clean.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Neutral),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    #[test]
    fn completed_neutral_with_review_row_only_at_old_sha_is_failed() {
        // Review row exists but on an old SHA — not the current HEAD.
        // The disambiguator must reject it.
        let revs = vec![cursor_review(OLD_SHA, "2026-04-22T10:00:00Z", "old")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Neutral),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    #[test]
    fn completed_failure_is_in_flight_failed() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Failure),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
    }

    #[test]
    fn completed_skipped_collapses_to_clean() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Skipped),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::Reviewed(ReviewedState::Clean));
    }

    #[test]
    fn suite_completed_without_child_run_is_failed() {
        // Suite reached `completed` with no child run ever spawned —
        // backend cancellation signature.
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_suite_only(CheckSuiteStatus::Completed, "2026-04-23T10:00:00Z"),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.activity, CursorActivity::InFlight(InFlightHealth::Failed),);
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

    #[test]
    fn parse_severity_finds_tag_after_leading_bold() {
        // Regression: old parser took the first ** and required
        // the closing ** to delimit the severity literal; a
        // preceding bold span (e.g. "**Note:**") broke it.
        assert_eq!(
            parse_severity("**Note:** **High Severity** message"),
            Some(Severity::High),
        );
    }

    #[test]
    fn parse_severity_picks_first_positionally() {
        assert_eq!(
            parse_severity("**Medium Severity** then **High Severity**"),
            Some(Severity::Medium),
        );
    }

    // ── tier transitions ──

    #[test]
    fn tier_platinum_when_completed_success() {
        let r = orient_cursor(
            &[],
            &empty_threads(),
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Platinum);
    }

    #[test]
    fn tier_silver_when_suite_queued_with_prior_round() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T09:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &status_suite_only(CheckSuiteStatus::Queued, "2026-04-23T10:00:00Z"),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Silver);
    }

    #[test]
    fn tier_bronze_when_unresolved_threads_present() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let threads = threads_with(vec![cursor_thread(false, "**High Severity** issue")]);
        let r = orient_cursor(
            &revs,
            &threads,
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Bronze);
        assert_eq!(r.threads.unresolved, 1);
    }

    #[test]
    fn tier_gold_when_round_exists_but_no_active_suite() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T09:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &empty_status(),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.tier, CursorTier::Gold);
    }

    // ── severity counting ──

    #[test]
    fn severity_count_partitions_high_medium_low() {
        let threads = threads_with(vec![
            cursor_thread(false, "**High Severity** thing"),
            cursor_thread(false, "**Medium Severity** thing"),
            cursor_thread(false, "**Medium Severity** other"),
            cursor_thread(false, "**Low Severity** thing"),
            cursor_thread(true, "**High Severity** but resolved"),
        ]);
        let r = orient_cursor(
            &[],
            &threads,
            &status_with_run(
                CheckSuiteStatus::Completed,
                "2026-04-23T10:00:00Z",
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
                Some("2026-04-23T10:01:00Z"),
            ),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.severity.high, 1);
        assert_eq!(r.severity.medium, 2);
        assert_eq!(r.severity.low, 1);
    }

    // ── round correlation ──

    #[test]
    fn rounds_sorted_by_submitted_at_with_indexing() {
        let revs = vec![
            cursor_review(OLD_SHA, "2026-04-23T11:00:00Z", "found 2 potential issues"),
            cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "found 0 potential issues"),
        ];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &empty_status(),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert_eq!(r.rounds.len(), 2);
        assert_eq!(r.rounds[0].round, 1);
        assert_eq!(r.rounds[0].commit.as_str(), HEAD_SHA);
        assert_eq!(r.rounds[1].round, 2);
        assert_eq!(r.rounds[1].findings_count, 2);
    }

    #[test]
    fn fresh_true_when_latest_review_at_head() {
        let revs = vec![cursor_review(HEAD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &empty_status(),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert!(r.fresh);
    }

    #[test]
    fn fresh_false_when_latest_review_at_old_sha() {
        let revs = vec![cursor_review(OLD_SHA, "2026-04-23T10:00:00Z", "")];
        let r = orient_cursor(
            &revs,
            &empty_threads(),
            &empty_status(),
            Some(&human_author()),
            &head(),
            now(),
        )
        .unwrap();
        assert!(!r.fresh);
    }

    // ── reserved SkipReason variants ──
    //
    // `RepoConfig` and `Unknown` are part of the public contract but
    // the current classifier never emits them. Construct each variant
    // and assert its serialized wire name to lock the JSONL shape
    // against silent rename drift.

    #[test]
    fn skip_reason_repo_config_serializes_to_pascal_case() {
        let json = serde_json::to_string(&SkipReason::RepoConfig).unwrap();
        assert_eq!(json, "\"RepoConfig\"");
    }

    #[test]
    fn skip_reason_unknown_serializes_to_pascal_case() {
        let json = serde_json::to_string(&SkipReason::Unknown).unwrap();
        assert_eq!(json, "\"Unknown\"");
    }

    #[test]
    fn skip_reason_label_covers_all_variants() {
        assert_eq!(skip_reason_label(SkipReason::AuthorClass), "author class");
        assert_eq!(skip_reason_label(SkipReason::RepoConfig), "repo config");
        assert_eq!(skip_reason_label(SkipReason::Unknown), "unknown");
    }
}
