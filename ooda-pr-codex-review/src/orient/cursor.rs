//! Cursor orient: project Cursor's check_suite + check_run + reviews
//! into the per-PR Cursor activity state.
//!
//! ## Why this axis diverges
//!
//! Cursor's state machine is deliberately divergent from Copilot's
//! (Idle/Requested(Health)/Working(Health)/Reviewed) and CI's
//! (Idle/InFlight(Vec<PendingCheck>)/Resolved). Cursor is push-driven
//! (no `review_requested` event), has no remediation API (cannot
//! re-poke a stalled suite — posting a `cursor review` comment
//! doesn't unstick Cursor's own backend queue), and has first-class
//! non-presence states (NotApplicable, Skipped) because Cursor
//! explicitly declines some PRs server-side (Dependabot author
//! class, repo opt-out). See feedback-domain-shapes-design memory:
//! when domains diverge, don't force a meta-structure across axes.
//!
//! Concretely, that means no `AxisHealth<S>` lift here — Cursor's
//! health is binary (Healthy or Failed), the InFlight payload is
//! nullary, the Skipped variant carries a SkipReason for diagnostic
//! distinction, and there is no Symptom enum because Cursor has a
//! single failure mode (stalled check_suite on Cursor's servers).

use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::cursor_status::{
    CheckRunConclusion, CheckRunStatus, CheckSuiteStatus, CursorCheckSuite, CursorStatus,
};
use crate::observe::github::pr_view::PullRequestAuthor;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use serde::Serialize;

use super::bot_threads::{BotThreadSummary, count_bot_threads};

// ── Identity ─────────────────────────────────────────────────────────

const CURSOR_LOGINS: &[&str] = &["cursor[bot]", "cursor"];

pub fn is_cursor(login: &str) -> bool {
    CURSOR_LOGINS.contains(&login)
}

/// Login slugs Cursor's server-side filter declines automatically.
/// Hardcoded short list is fine for v1; extend when a new automation
/// vendor enters the org's PR mix. Matches the bare login AND the
/// `[bot]`-suffixed form because the GraphQL and REST surfaces emit
/// different shapes.
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

/// Time Cursor is allowed between check_suite creation (or check_run
/// start) and a terminal state before the in-flight stage is treated
/// as Failed.
//
// ~2× p95 pickup of 7.3m (n=126 across protocol/infrastructure/
// explorer, ~30d sample). Below the observed max of 2.6h (outliers
// at p99=14.6m), pads enough that healthy pickups don't trip the
// detector while still catching the canonical stuck-suite pattern
// (suite stuck queued > 1h).
pub const STALL_TIMEOUT: chrono::Duration = chrono::Duration::minutes(15);

// ── Public types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorReport {
    pub activity: CursorActivity,
    /// All Cursor review rounds, oldest first. Empty when Cursor has
    /// not yet submitted a review at any HEAD on this PR.
    pub rounds: Vec<CursorReviewRound>,
    pub threads: BotThreadSummary,
    pub severity: CursorSeverityBreakdown,
    pub tier: CursorTier,
    /// Latest review observed at HEAD (`latest.commit == head`).
    pub fresh: bool,
    /// `created_at` from the Cursor check_suite when present.
    /// `None` when no suite has been observed for this PR
    /// (NotApplicable, Skipped, or a round-only history). Decide's
    /// `EscalateCursorStalled` prompt surfaces the suite's age so
    /// the human sees "stalled since <ts>, ~N minutes ago" rather
    /// than just a generic STALL_TIMEOUT mention.
    pub suite_created_at: Option<Timestamp>,
}

// Cursor's state machine is deliberately divergent from Copilot's
// (Idle/Requested(Health)/Working(Health)/Reviewed) and CI's
// (Idle/InFlight(Vec<PendingCheck>)/Resolved). Cursor is push-driven
// (no request event), has no remediation API (cannot poke), and has
// first-class non-presence states (NotApplicable, Skipped). See
// feedback-domain-shapes-design memory — don't force a meta-structure
// across axes when domains diverge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum CursorActivity {
    /// Cursor not active in this repo — no Cursor check_suite ever
    /// observed for this PR's HEAD AND the author isn't on the
    /// bot-class skip list. Repo-level absence.
    NotApplicable,
    /// Cursor declined this PR. Per-PR refusal — distinct from
    /// repo-level NotApplicable for diagnostic purposes (the JSONL
    /// record carries the reason).
    Skipped(SkipReason),
    /// Cursor's check_suite (and maybe child check_run) is pending.
    /// Health is binary — see [`InFlightHealth`].
    InFlight(InFlightHealth),
    /// Cursor's check_run reached a terminal state on this HEAD.
    Reviewed(ReviewedState),
}

// First-class distinction from Copilot/CI: Cursor explicitly declines
// some PRs (e.g., Dependabot author class). NotApplicable is
// repo-level absence; Skipped is per-PR refusal. Distinguishing them
// helps post-hoc analysis even though both delegate to no action.
//
// `RepoConfig` and `Unknown` are part of the contract but v1's
// classifier only emits `AuthorClass` — no Cursor config-fetch path
// exists yet, and the classifier falls back to `NotApplicable` rather
// than `Skipped(Unknown)` to avoid noise. The variants stay first-
// class so the JSONL schema is stable when those paths land.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SkipReason {
    /// Author is in Cursor's bot-class filter (Dependabot, Renovate,
    /// GitHub Actions). Detected at observe boundary; the activity
    /// classifier never sees a "missing check" it can't explain for
    /// these PRs.
    AuthorClass,
    /// Repo opted out of Cursor (configurable on cursor.com). v1 does
    /// not have a config probe; this variant is reserved for the
    /// future config-fetch path. Today the classifier emits Unknown
    /// instead.
    RepoConfig,
    /// Suite absent but author isn't bot-class — could be repo
    /// opt-out, seat coverage gap, or a silent Cursor backend failure
    /// we cannot disambiguate from outside. Catch-all rather than a
    /// false-positive InFlight(Failed).
    Unknown,
}

// Two states only: Healthy or Failed. No Degraded intermediate.
// Rationale: when Cursor's check_suite is stalled on Cursor's servers,
// posting a 'cursor review' comment doesn't unstick the queue (Cursor
// already knows about the PR). No remediation → no retry budget →
// straight to escalation on threshold trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum InFlightHealth {
    /// Within STALL_TIMEOUT of either the suite's creation (when no
    /// child run exists yet) or the run's `started_at` (or the
    /// suite's `created_at` as fallback when the run has no
    /// `started_at`).
    Healthy,
    /// STALL_TIMEOUT elapsed. No remediation; the decide layer
    /// escalates directly to a human handoff.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReviewedState {
    /// Run completed with no findings posted as a review on this
    /// commit. Cursor's `success` conclusion is normal-and-quiet.
    Clean,
    /// Run completed with findings. Either `success` + a cursor[bot]
    /// review row exists on this commit, or `neutral` disambiguated
    /// to "issues found" via the cross-ref join on `/pulls/{n}/reviews`.
    /// Existing thread-addressing path (the generic AddressThreads
    /// from the reviews axis) handles the actual remediation.
    HasFindings,
}

/// Project Cursor activity onto a dashboard signal. `NotApplicable`
/// returns `None` — repo/PR has no Cursor signal at all.
pub fn cursor_signal(activity: &CursorActivity) -> Option<crate::dashboard::AxisSignal> {
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
    /// `["2 high", "1 medium"]` — only non-zero buckets, in order.
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

/// Project Cursor's per-HEAD signal into the orient state. Always
/// returns `Some` once any signal exists; emits `Some(NotApplicable)`
/// only when both Cursor and the per-PR Skip detector agree there's
/// nothing here.
///
/// Returns `None` purely to preserve the existing
/// `Option<CursorReport>` field shape — `None` happens when neither
/// the suite, prior rounds, nor an author signal give the classifier
/// anything to project. In practice this collapses with NotApplicable;
/// the Option layer is kept so the rendering and JSONL paths can
/// distinguish "no observation at all" from "observation says
/// NotApplicable".
#[allow(clippy::too_many_arguments)]
pub fn orient_cursor(
    reviews: &[PullRequestReview],
    threads: &ReviewThreadsResponse,
    cursor_status: &CursorStatus,
    author: Option<&PullRequestAuthor>,
    head: &GitCommitSha,
    now: Timestamp,
) -> Option<CursorReport> {
    let rounds = correlate_rounds(reviews);

    // No signal at all → no report. The Option carries this
    // distinction even though NotApplicable would render identically;
    // a downstream JSONL consumer can tell the two apart.
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

/// Cross-ref join: does cursor[bot] have a review row anchored to
/// `head`? This is the disambiguator for the `neutral` conclusion AND
/// the witness for `success` + findings-present.
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

/// Tier rules (first match wins):
///   bronze:   unresolved>0 OR (no rounds AND no suite)
///   silver:   unresolved=0 AND suite queued/in_progress AND prior round
///   gold:     unresolved=0 AND rounds present AND no in-flight suite
///   platinum: suite completed with success-conclusion run at HEAD
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
    // Must query check_suites endpoint, not just check_runs. The
    // canonical Cursor stall (~7% of PRs in a 30d sample) is a stuck
    // check_suite (queued, no child check_run ever created).
    // Invisible to `check_name=Cursor Bugbot` filtering, which is
    // what `PullRequestCheck` ultimately sees.
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
            // Completed without a conclusion is an undocumented but
            // observed eventual-consistency state (the API has
            // marked the run completed before the conclusion field
            // populates). Treat as still-running for the threshold
            // calculation — re-observe on next tick.
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

// Cursor declines Dependabot-class PRs by author policy (their
// server-side filter). Detect at observe boundary; surface as
// Skipped(AuthorClass) so the health detector doesn't false-positive
// on the 'no check at all' signal.
fn classify_no_suite(author: Option<&PullRequestAuthor>) -> CursorActivity {
    match author {
        Some(a) if is_bot_author(a) => CursorActivity::Skipped(SkipReason::AuthorClass),
        // No author available — treat as repo-level absence to avoid
        // a noisy Skipped(Unknown) when the PR view fetch race-
        // conditioned the author field. Same fallback as a known
        // non-bot author on a repo that's never seen a Cursor run:
        // the activity classifier has no positive evidence Cursor
        // is active here.
        Some(_) | None => CursorActivity::NotApplicable,
    }
}

fn classify_suite_without_run(suite: &CursorCheckSuite, now: Timestamp) -> CursorActivity {
    match suite.status {
        CheckSuiteStatus::Completed => {
            // Suite reported `completed` without ever spawning a
            // child run — Cursor cancelled before producing output.
            // No review on this HEAD by construction (the run is
            // where the review would have come from). Treat as a
            // backend cancellation: Failed.
            CursorActivity::InFlight(InFlightHealth::Failed)
        }
        CheckSuiteStatus::Queued | CheckSuiteStatus::InProgress | CheckSuiteStatus::Unknown => {
            if now.at() - suite.created_at.at() >= STALL_TIMEOUT {
                CursorActivity::InFlight(InFlightHealth::Failed)
            } else {
                CursorActivity::InFlight(InFlightHealth::Healthy)
            }
        }
    }
}

// Cursor's neutral conclusion is overloaded: could mean 'issues found
// and posted as review comments' OR 'Cursor backend cancelled /
// internal error'. Disambiguate via cross-ref join on
// /pulls/{n}/reviews — if cursor[bot] review row exists on this
// commit, treat as HasFindings; otherwise treat as backend-cancel →
// InFlight(Failed).
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
        // Skipped / ActionRequired / Stale / Unknown: treat as
        // "Cursor decided not to act on this run". No actionable
        // remediation; collapse to Clean so the loop doesn't loop.
        CheckRunConclusion::Skipped
        | CheckRunConclusion::ActionRequired
        | CheckRunConclusion::Stale
        | CheckRunConclusion::Unknown => CursorActivity::Reviewed(ReviewedState::Clean),
    }
}

/// Health from the per-run timing anchor. Uses `started_at` when
/// present, else falls back to the suite's `created_at` (a run that
/// has not yet started has no per-run anchor of its own).
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
    use crate::observe::github::pr_view::PullRequestAuthor;
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
}
