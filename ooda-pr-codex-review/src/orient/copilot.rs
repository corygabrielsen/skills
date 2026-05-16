//! Copilot orient: project events + reviews + threads into the
//! per-PR Copilot review state.
//!
//! This is the most complex orient axis so far — joins three
//! observation sources, runs a state machine (request → ack →
//! review) to assemble rounds, and parses Copilot's review bodies
//! for visible/suppressed counts.
//!
//! Returns `Option<CopilotReport>`: `None` iff the repo ruleset
//! has Copilot disabled. When enabled-but-never-engaged on this
//! PR, returns `Some(report)` with `activity = Idle` — letting
//! downstream code distinguish "no policy" from "policy but
//! dormant" rather than collapsing both, which was the source of
//! the false-stall bug in pr-fitness.

use crate::ids::{GitCommitSha, Timestamp};
use crate::observe::github::issue_events::IssueEvent;
use crate::observe::github::pull_request_view::Commit;
use crate::observe::github::requested_reviewers::RequestedReviewers;
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::observe::github::rulesets::CopilotCodeReviewParams;
use serde::Serialize;

use super::bot_threads::{BotThreadSummary, count_bot_threads};

// ── Identity ─────────────────────────────────────────────────────────

/// Canonical login string for adding Copilot as a reviewer (POST
/// `requested_reviewers`). The `[bot]` suffix variant is the only
/// form the write API accepts.
pub(crate) const COPILOT_REVIEWER_LOGIN: &str = "copilot-pull-request-reviewer[bot]";

/// Every known Copilot login variant. GitHub returns different
/// strings on different API surfaces (REST reviews vs GraphQL vs
/// `requested_reviewers`); we accept all of them on read.
const COPILOT_LOGINS: &[&str] = &[
    COPILOT_REVIEWER_LOGIN,
    "Copilot",
    "copilot-pull-request-reviewer",
];

pub(crate) fn is_copilot(login: &str) -> bool {
    COPILOT_LOGINS.contains(&login)
}

// ── Public types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CopilotReport {
    pub config: CopilotRepoConfig,
    pub activity: CopilotActivity,
    /// All review rounds, oldest first. Empty when Copilot has not
    /// been requested (or not yet acked).
    pub rounds: Vec<CopilotReviewRound>,
    pub threads: BotThreadSummary,
    pub tier: CopilotTier,
    /// Latest review observed at HEAD (`latest.commit == head`).
    pub fresh: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct CopilotRepoConfig {
    pub enabled: bool,
    pub review_on_push: bool,
    pub review_draft_pull_requests: bool,
}

impl From<CopilotCodeReviewParams> for CopilotRepoConfig {
    fn from(p: CopilotCodeReviewParams) -> Self {
        Self {
            enabled: true,
            review_on_push: p.review_on_push,
            review_draft_pull_requests: p.review_draft_pull_requests,
        }
    }
}

// Health embedded in in-flight variants. Idle and Reviewed carry no
// Health by design — the "health is meaningful only when in flight"
// invariant is type-structural here, not enforced at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum CopilotActivity {
    Idle,
    Requested {
        requested_at: Timestamp,
        health: InFlightHealth,
    },
    Working {
        requested_at: Timestamp,
        ack_at: Timestamp,
        health: InFlightHealth,
    },
    Reviewed {
        latest: CopilotReviewRound,
    },
}

/// Project Copilot activity onto a dashboard signal. `Idle` returns
/// `None` — the axis is dormant and should not emit a row. Every
/// other variant emits the icon+summary the spec table fixes.
pub(crate) fn copilot_signal(activity: &CopilotActivity) -> Option<crate::dashboard::AxisSignal> {
    use crate::dashboard::{AxisName, AxisSignal, SignalIcon};
    let (icon, summary) = match activity {
        CopilotActivity::Idle => return None,
        CopilotActivity::Requested {
            health: InFlightHealth::Healthy,
            ..
        } => (SignalIcon::InFlight, "review requested"),
        CopilotActivity::Requested {
            health: InFlightHealth::Degraded,
            ..
        } => (
            SignalIcon::Warn,
            "request received but no review yet (degraded)",
        ),
        CopilotActivity::Requested {
            health: InFlightHealth::Failed,
            ..
        } => (SignalIcon::Failed, "request not picked up — escalating"),
        CopilotActivity::Working {
            health: InFlightHealth::Healthy,
            ..
        } => (SignalIcon::InFlight, "reviewing"),
        CopilotActivity::Working {
            health: InFlightHealth::Degraded,
            ..
        } => (SignalIcon::Warn, "review running long (degraded)"),
        CopilotActivity::Working {
            health: InFlightHealth::Failed,
            ..
        } => (SignalIcon::Failed, "review stalled — escalating"),
        CopilotActivity::Reviewed { .. } => (SignalIcon::Ok, "review complete"),
    };
    Some(AxisSignal {
        axis: AxisName::Copilot,
        icon,
        summary: summary.to_string(),
    })
}

// Per-axis health projection. CI axis (queue stalls) and any
// subsequent reviewer axis will wear the same shape. On the third
// axis, lift to ooda_core::AxisHealth<S>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum InFlightHealth {
    /// In flight within the configured timeout window — keep waiting.
    Healthy,
    /// Timeout crossed once for this round at the current HEAD. One
    /// re-request remediation is in budget before escalation.
    Degraded,
    /// Timeout crossed and the per-HEAD budget is exhausted —
    /// re-requesting again would only restart the same failure mode.
    Failed,
}

// Axis-local. The future AxisHealth<S> parameterization carries this
// per axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Symptom {
    /// `copilot_work_started` never landed within the start window
    /// after `review_requested`.
    StartTimeout,
    /// Started but no review submitted within the review window
    /// after the start event.
    ReviewTimeout,
}

/// Time Copilot is allowed to ack a fresh review request before the
/// round is treated as `StartTimeout`-degraded.
pub(crate) const THRESHOLD_START_TIMEOUT: chrono::Duration = chrono::Duration::minutes(10);

/// Time Copilot is allowed between ack and review submission before
/// the round is treated as `ReviewTimeout`-degraded.
pub(crate) const THRESHOLD_REVIEW_TIMEOUT: chrono::Duration = chrono::Duration::minutes(30);

/// Per-HEAD remediation budget — number of Degraded rounds at the
/// current HEAD we will tolerate before promoting to `Failed` and
/// handing off to a human.
pub(crate) const HEALTH_REMEDIATION_BUDGET: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CopilotReviewRound {
    /// 1-indexed within this PR's Copilot review history.
    pub round: u32,
    pub requested_at: Timestamp,
    pub ack_at: Option<Timestamp>,
    pub reviewed_at: Option<Timestamp>,
    pub commit: Option<GitCommitSha>,
    /// Visible inline comment count from review body.
    pub comments_visible: u32,
    /// Suppressed low-confidence finding count from review body.
    pub comments_suppressed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CopilotTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

impl CopilotTier {
    /// Lowercase, stable slug for use in user-facing strings and
    /// blocker keys. Coupled to the variant *names* in the type
    /// contract — renaming a variant requires updating this impl.
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

/// Returns `None` iff the repo ruleset has no active
/// `copilot_code_review` rule (i.e. Copilot is not configured for
/// this repo at all). When configured, always returns `Some` even
/// if Copilot has never engaged on this PR.
///
/// The parameter list is long because the copilot axis joins more
/// observation sources than other axes (timeline, reviews, threads,
/// requested reviewers, commits, plus a clock). Bundling into a
/// "context" struct would only shift the surface area without
/// shrinking it; the long list pays for itself in call-site clarity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn orient_copilot(
    config: CopilotRepoConfig,
    events: &[IssueEvent],
    reviews: &[PullRequestReview],
    threads: &ReviewThreadsResponse,
    requested: &RequestedReviewers,
    head: &GitCommitSha,
    commits: &[Commit],
    now: Timestamp,
) -> Option<CopilotReport> {
    if !config.enabled {
        return None;
    }

    let copilot_reviews: Vec<&PullRequestReview> = reviews
        .iter()
        .filter(|r| {
            r.user
                .as_ref()
                .is_some_and(|u| is_copilot(u.login.as_str()))
        })
        .collect();

    let reviewer_events = copilot_reviewer_events(events);
    let timeline = copilot_timeline(&reviewer_events);
    let rounds = correlate_rounds(&timeline, &copilot_reviews);
    let latest_reviewed_at = rounds.last().and_then(|r| r.reviewed_at);
    let thread_summary = count_bot_threads(threads, latest_reviewed_at.as_ref(), is_copilot);
    let activity = derive_activity(&timeline, &rounds, requested, head, commits, now);
    let tier = score_tier(&rounds, &thread_summary, head);
    let fresh = is_fresh(&rounds, head);

    Some(CopilotReport {
        config,
        activity,
        rounds,
        threads: thread_summary,
        tier,
        fresh,
    })
}

/// Filter `events` to those that legitimately reflect the Copilot
/// **reviewer** axis.
//
// Two GitHub Apps share the "Copilot" identity:
// copilot-pull-request-reviewer (review path) and copilot-swe-agent
// (coding-agent path). They emit identical event types. Filter at
// the axis boundary; upstream consumers see only reviewer events.
fn copilot_reviewer_events(events: &[IssueEvent]) -> Vec<&IssueEvent> {
    const COPILOT_REVIEWER_APP: &str = "copilot-pull-request-reviewer";
    events
        .iter()
        .filter(|e| match e.event.as_str() {
            "copilot_work_started" | "copilot_work_finished" => e
                .performed_via_github_app
                .as_ref()
                .is_some_and(|a| a.slug == COPILOT_REVIEWER_APP),
            _ => true,
        })
        .collect()
}

// ── Body parsing ─────────────────────────────────────────────────────

/// Parse `(visible, suppressed)` counts from a Copilot review body.
///
/// Matches:
///   "generated N comment(s)" → visible = N
///   "generated no new comments" → visible = 0
///   "Comments suppressed due to low confidence (N…)" → suppressed = N
pub(crate) fn parse_copilot_review_body(body: &str) -> (u32, u32) {
    let visible = if body.contains("generated no new comments") {
        0
    } else {
        find_count(body, "generated ").unwrap_or(0)
    };
    let suppressed = find_count(body, "Comments suppressed due to low confidence (").unwrap_or(0);
    (visible, suppressed)
}

/// Find the leading run of ASCII digits immediately after `prefix`
/// in `body`. Digit-run is the authoritative terminator — previous
/// versions used `find(suffix)` which truncated mid-token if the
/// body added a parenthesized clarifier (e.g. "(N of M)" → first
/// `)` lands inside the count region).
fn find_count(body: &str, prefix: &str) -> Option<u32> {
    let start = body.find(prefix)? + prefix.len();
    let rest = &body[start..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

// ── Timeline + rounds ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimelinePoint {
    kind: TimelineKind,
    at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineKind {
    Requested,
    Ack,
}

fn copilot_timeline(events: &[&IssueEvent]) -> Vec<TimelinePoint> {
    let mut points: Vec<TimelinePoint> = Vec::new();
    for e in events {
        let Some(at) = e.created_at else { continue };
        match e.event.as_str() {
            "review_requested" => {
                if let Some(rr) = &e.requested_reviewer
                    && is_copilot(rr.login.as_str())
                {
                    points.push(TimelinePoint {
                        kind: TimelineKind::Requested,
                        at,
                    });
                }
            }
            "copilot_work_started" => {
                points.push(TimelinePoint {
                    kind: TimelineKind::Ack,
                    at,
                });
            }
            _ => {}
        }
    }
    points.sort_by_key(|p| p.at);
    points
}

/// Build per-round Copilot review state by chronologically merging
/// the timeline (Requested + Ack events) with the review submissions.
///
/// Single invariant: every review either pairs with the open round
/// (anchored on the most recent Requested event) or becomes a
/// synthetic round on its own. Replaces the prior 3-drain
/// implementation (pre-request, same-window, post-loop) with one
/// merge-walk — the invariant is enforced inline at every step
/// instead of bolted on at three separate "didn't we forget this
/// edge?" points.
///
/// Three review-on-push edge cases the merge-walk covers naturally:
///   - Reviews submitted before any Requested event → no open round
///     when the review arrives → synthetic round.
///   - Multiple reviews inside one request window → first pairs
///     with the round; subsequent ones land while `paired` is true
///     → synthetic rounds.
///   - Ack arriving without a preceding Requested event (auto-review
///     `copilot_work_started` only) → no open round → Ack ignored
///     here. `derive_activity` reads the timeline directly for
///     orphan Acks and reports `Working`, so the signal isn't lost.
fn correlate_rounds(
    timeline: &[TimelinePoint],
    reviews: &[&PullRequestReview],
) -> Vec<CopilotReviewRound> {
    let mut sorted_reviews: Vec<&PullRequestReview> = reviews.to_vec();
    sorted_reviews.sort_by_key(|a| a.submitted_at);

    // Round numbers are assigned at the very end after a final sort
    // by `requested_at`. Without this, the open round (anchored on
    // an earlier Requested event) would land AFTER a synthetic round
    // that was emitted while open was still being assembled — wrong
    // chronological order, wrong `rounds.last()`, wrong tier scoring.
    let mut rounds: Vec<CopilotReviewRound> = Vec::new();
    let mut open: Option<CopilotReviewRound> = None;
    let mut paired = false;
    let mut review_idx = 0;

    let consume_review = |rev: &PullRequestReview, round: &mut CopilotReviewRound| {
        round.reviewed_at = rev.submitted_at;
        round.commit = Some(rev.commit_id.clone());
        let (visible, suppressed) = parse_copilot_review_body(&rev.body);
        round.comments_visible = visible;
        round.comments_suppressed = suppressed;
    };

    for point in timeline {
        // Before processing this timeline point, drain every review
        // strictly earlier than it. Each review either pairs with
        // the open round (if any and not yet paired) or becomes a
        // synthetic round.
        while let Some(rev) = sorted_reviews.get(review_idx)
            && rev.submitted_at.as_ref().is_some_and(|t| t < &point.at)
        {
            match (&mut open, paired) {
                (Some(round), false) => {
                    consume_review(rev, round);
                    paired = true;
                }
                _ => {
                    rounds.push(synthetic_round(rev, 0));
                }
            }
            review_idx += 1;
        }

        match point.kind {
            TimelineKind::Requested => {
                if let Some(round) = open.take() {
                    rounds.push(round);
                }
                open = Some(CopilotReviewRound {
                    round: 0, // assigned in the final sort pass
                    requested_at: point.at,
                    ack_at: None,
                    reviewed_at: None,
                    commit: None,
                    comments_visible: 0,
                    comments_suppressed: 0,
                });
                paired = false;
            }
            TimelineKind::Ack => {
                if let Some(round) = open.as_mut()
                    && round.ack_at.is_none()
                {
                    round.ack_at = Some(point.at);
                }
                // Orphan Acks (auto-review with no Requested event)
                // don't land here. derive_activity reads them from
                // the raw timeline and reports Working.
            }
        }
    }

    // Drain remaining reviews after the last timeline event.
    while let Some(rev) = sorted_reviews.get(review_idx) {
        match (&mut open, paired) {
            (Some(round), false) => {
                consume_review(rev, round);
                paired = true;
            }
            _ => {
                rounds.push(synthetic_round(rev, 0));
            }
        }
        review_idx += 1;
    }
    if let Some(round) = open.take() {
        rounds.push(round);
    }

    // Final pass: sort by anchor and renumber. Stable sort keeps
    // the relative order of rounds with the same `requested_at`
    // (only happens for synthetic rounds with identical review
    // timestamps — preserve insertion order).
    rounds.sort_by_key(|r| r.requested_at);
    for (i, r) in rounds.iter_mut().enumerate() {
        // Review-round count fits in u32 by construction: bounded by
        // GitHub's per-PR review history (orders of magnitude < 4B).
        r.round = u32::try_from(i).expect("review round index fits in u32") + 1;
    }
    rounds
}

/// Synthetic round for a review with no preceding Requested event.
/// The review's own timestamp serves as the round anchor — there is
/// no real request, ack, or window to derive.
fn synthetic_round(rev: &PullRequestReview, round_no: u32) -> CopilotReviewRound {
    let counts = parse_copilot_review_body(&rev.body);
    let anchor = rev
        .submitted_at
        .unwrap_or_else(|| Timestamp::parse("1970-01-01T00:00:00Z").unwrap());
    CopilotReviewRound {
        round: round_no,
        requested_at: anchor,
        ack_at: None,
        reviewed_at: rev.submitted_at,
        commit: Some(rev.commit_id.clone()),
        comments_visible: counts.0,
        comments_suppressed: counts.1,
    }
}

// ── Tier scoring ─────────────────────────────────────────────────────

/// Tier rules (first match wins):
///   bronze:   no review yet OR latest still in flight OR unresolved>0
///   silver:   reviewed, no unresolved, suppressed>0
///   gold:     reviewed, no unresolved, no suppressed, (stale>0 OR latest!=HEAD)
///   platinum: reviewed, no unresolved, no suppressed, no stale, latest=HEAD
fn score_tier(
    rounds: &[CopilotReviewRound],
    threads: &BotThreadSummary,
    head: &GitCommitSha,
) -> CopilotTier {
    let Some(latest) = rounds.last() else {
        return CopilotTier::Bronze;
    };
    if latest.reviewed_at.is_none() {
        return CopilotTier::Bronze;
    }
    if threads.unresolved > 0 {
        return CopilotTier::Bronze;
    }
    if latest.comments_suppressed > 0 {
        return CopilotTier::Silver;
    }
    if threads.stale > 0 {
        return CopilotTier::Gold;
    }
    if latest.commit.as_ref() == Some(head) {
        CopilotTier::Platinum
    } else {
        CopilotTier::Gold
    }
}

fn is_fresh(rounds: &[CopilotReviewRound], head: &GitCommitSha) -> bool {
    rounds
        .last()
        .is_some_and(|r| r.reviewed_at.is_some() && r.commit.as_ref() == Some(head))
}

// ── Activity derivation ──────────────────────────────────────────────

/// Bare in-flight stage — what kind of pending work, divorced from
/// health. `derive_activity` first computes this from the existing
/// timeline/rounds/requested signals, then wraps in-flight stages
/// with [`InFlightHealth`] derived from per-round timing.
enum BareStage {
    Idle,
    Requested {
        requested_at: Timestamp,
    },
    Working {
        requested_at: Timestamp,
        ack_at: Timestamp,
    },
    Reviewed {
        latest: CopilotReviewRound,
    },
}

fn derive_activity(
    timeline: &[TimelinePoint],
    rounds: &[CopilotReviewRound],
    requested: &RequestedReviewers,
    head: &GitCommitSha,
    commits: &[Commit],
    now: Timestamp,
) -> CopilotActivity {
    let stage = bare_stage(timeline, rounds, requested);
    match stage {
        BareStage::Idle => CopilotActivity::Idle,
        BareStage::Reviewed { latest } => CopilotActivity::Reviewed { latest },
        BareStage::Requested { requested_at } => {
            let health = compute_in_flight_health(rounds, head, commits, now);
            CopilotActivity::Requested {
                requested_at,
                health,
            }
        }
        BareStage::Working {
            requested_at,
            ack_at,
        } => {
            let health = compute_in_flight_health(rounds, head, commits, now);
            CopilotActivity::Working {
                requested_at,
                ack_at,
                health,
            }
        }
    }
}

fn bare_stage(
    timeline: &[TimelinePoint],
    rounds: &[CopilotReviewRound],
    requested: &RequestedReviewers,
) -> BareStage {
    let latest_review_ts: Option<&Timestamp> =
        rounds.iter().filter_map(|r| r.reviewed_at.as_ref()).max();
    let latest_ack: Option<&TimelinePoint> = timeline
        .iter()
        .filter(|p| p.kind == TimelineKind::Ack)
        .max_by_key(|p| p.at);
    let latest_request: Option<&TimelinePoint> = timeline
        .iter()
        .filter(|p| p.kind == TimelineKind::Requested)
        .max_by_key(|p| p.at);

    // Phase 1: Working signal — an Ack newer than the latest review
    // means Copilot is actively reviewing right now. The guard must
    // accept the right cases without false-firing on the
    // request-withdrawn-after-ack case:
    //   - currently_pending(requested) catches re-request mid-flight
    //     (new Requested + Ack landed; issue_events propagating).
    //   - latest_ack_is_orphan catches review_on_push auto-review:
    //     a new copilot_work_started fires WITHOUT a Requested event
    //     (so it doesn't appear as ack_at on any round). Works both
    //     when rounds is empty AND when prior rounds exist (a NEW
    //     auto-review after a previous Reviewed round) — a guard of
    //     "rounds.is_empty()" missed the latter case and let decide
    //     re-request Copilot mid-flight.
    // Class invariant: an Ack is orphan iff its timestamp does NOT
    // appear as ack_at on any existing round. The withdrawn-after-
    // ack case has its Ack already paired (via correlate_rounds),
    // so it's not orphan and Phase 1 doesn't fire — Idle remains.
    let ack_after_review = match (latest_ack, latest_review_ts) {
        (Some(ack), Some(rev)) => &ack.at > rev,
        (Some(_), None) => true,
        _ => false,
    };
    let latest_ack_is_orphan =
        latest_ack.is_some_and(|ack| !rounds.iter().any(|r| r.ack_at.as_ref() == Some(&ack.at)));
    let work_genuinely_in_flight = currently_pending(requested) || latest_ack_is_orphan;
    if let Some(ack) = latest_ack
        && ack_after_review
        && work_genuinely_in_flight
    {
        let req_at = latest_request
            .filter(|r| r.at <= ack.at)
            .map_or_else(|| ack.at, |r| r.at);
        return BareStage::Working {
            requested_at: req_at,
            ack_at: ack.at,
        };
    }

    if let Some(latest) = rounds.last() {
        if latest.reviewed_at.is_some() {
            // Reviewed branch: if Copilot is currently in
            // requested_reviewers but no Ack has landed yet, this is
            // a re-request that hasn't propagated to issue_events yet.
            // Pre-fix this returned Reviewed → decide() emitted
            // RerequestCopilot (Full) again → runner's repeated-Full
            // guard halted Stalled. Treat as Requested (waiting for
            // ack) so the loop emits WaitForCopilotAck instead.
            if currently_pending(requested) {
                let req_at = latest_request.map_or_else(|| latest.requested_at, |r| r.at);
                return BareStage::Requested {
                    requested_at: req_at,
                };
            }
            return BareStage::Reviewed {
                latest: latest.clone(),
            };
        }
        if let Some(ack) = &latest.ack_at {
            // Stale-round case: an Ack landed but the review request
            // was withdrawn before the review. Without the
            // currently_pending guard, decide emits
            // WaitForCopilotReview indefinitely (Wait actions are
            // stall-exempt). Class invariant: every "still working"
            // Copilot activity requires Copilot to actually be in
            // the current requested_reviewers set.
            if currently_pending(requested) {
                return BareStage::Working {
                    requested_at: latest.requested_at,
                    ack_at: *ack,
                };
            }
            return BareStage::Idle;
        }
        // No ack on the latest round. Distinguish "still pending"
        // from "request withdrawn before ack".
        if currently_pending(requested) {
            return BareStage::Requested {
                requested_at: latest.requested_at,
            };
        }
        return BareStage::Idle;
    }
    let pending = currently_pending(requested);
    if pending {
        // Eventual-consistency window: requested_reviewers shows
        // Copilot but issue_events hasn't surfaced the
        // review_requested/copilot_work_started event yet. Fall
        // back to a synthetic "epoch" timestamp so decide() still
        // emits WaitForCopilotAck instead of letting the PR halt
        // Success while Copilot review is pending. The Requested
        // activity is what matters; downstream consumers don't
        // anchor on the requested_at timestamp.
        let requested_at = timeline.last().map_or_else(
            || Timestamp::parse("1970-01-01T00:00:00Z").unwrap(),
            |p| p.at,
        );
        return BareStage::Requested { requested_at };
    }
    BareStage::Idle
}

fn currently_pending(requested: &RequestedReviewers) -> bool {
    requested.users.iter().any(|u| is_copilot(u.login.as_str()))
}

// ── Health computation ───────────────────────────────────────────────

/// Per-round seal: classify a round's terminal state given the
/// chronological horizon `tau` (either the next round's request
/// timestamp, for non-tail rounds, or `now` for the tail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sealed {
    Resolved,
    Healthy,
    Degraded(Symptom),
    /// A non-tail round superseded by a later request before any
    /// terminal signal (resolution or timeout) landed. Only emitted
    /// for non-tail rounds — the tail has no next request by zip
    /// construction.
    Preempted,
}

fn seal(round: &CopilotReviewRound, next_req_at: Option<Timestamp>, tau: Timestamp) -> Sealed {
    if round.reviewed_at.is_some() {
        return Sealed::Resolved;
    }
    if let Some(ack) = round.ack_at {
        if tau.at() - ack.at() >= THRESHOLD_REVIEW_TIMEOUT {
            return Sealed::Degraded(Symptom::ReviewTimeout);
        }
    } else if tau.at() - round.requested_at.at() >= THRESHOLD_START_TIMEOUT {
        return Sealed::Degraded(Symptom::StartTimeout);
    }
    if next_req_at.is_some() {
        Sealed::Preempted
    } else {
        Sealed::Healthy
    }
}

// SHA-equality partition; head_ref_force_pushed events are
// unreliable (GitHub drops them under comment-burst conditions). The
// stored HEAD SHA is the source of truth.
fn filter_to_current_head<'a>(
    rounds: &'a [CopilotReviewRound],
    commits: &[Commit],
    head: &GitCommitSha,
) -> Vec<&'a CopilotReviewRound> {
    rounds
        .iter()
        .filter(|r| head_at(commits, r.requested_at).as_ref() == Some(head))
        .collect()
}

/// The HEAD SHA in flight at `t`: the most-recent commit whose
/// `committed_date <= t`. `None` when `commits` is empty or every
/// commit was authored after `t` (a pre-history request — treat as
/// not-at-current-head).
fn head_at(commits: &[Commit], t: Timestamp) -> Option<GitCommitSha> {
    commits
        .iter()
        .filter(|c| c.committed_date <= t)
        .max_by_key(|c| c.committed_date)
        .map(|c| c.oid.clone())
}

fn compute_in_flight_health(
    rounds: &[CopilotReviewRound],
    head: &GitCommitSha,
    commits: &[Commit],
    now: Timestamp,
) -> InFlightHealth {
    let rounds_h = filter_to_current_head(rounds, commits, head);
    if rounds_h.is_empty() {
        // No round at current HEAD yet (e.g. eventual-consistency
        // synthetic Requested). The in-flight stage exists but no
        // round backs it — treat as Healthy; no timing signal to
        // degrade on.
        return InFlightHealth::Healthy;
    }
    let tail_idx = rounds_h.len() - 1;
    // Walk back from the tail; for each round, tau is the next
    // round's requested_at (non-tail) or `now` (tail). Stop at the
    // first non-Degraded.
    let mut degraded_run = 0usize;
    for rev_idx in 0..rounds_h.len() {
        let abs_idx = tail_idx - rev_idx;
        let next_req_at = if abs_idx == tail_idx {
            None
        } else {
            Some(rounds_h[abs_idx + 1].requested_at)
        };
        let tau = next_req_at.unwrap_or(now);
        if matches!(
            seal(rounds_h[abs_idx], next_req_at, tau),
            Sealed::Degraded(_)
        ) {
            degraded_run += 1;
        } else {
            break;
        }
    }

    let tail: &CopilotReviewRound = rounds_h[tail_idx];
    // Intentional exhaustive match per axis pattern; Resolved and
    // Healthy are kept distinct for spec clarity even though both
    // collapse to `Healthy` at the tail.
    #[allow(clippy::match_same_arms)]
    match seal(tail, None, now) {
        Sealed::Resolved => InFlightHealth::Healthy,
        Sealed::Healthy => InFlightHealth::Healthy,
        Sealed::Degraded(_) if degraded_run >= HEALTH_REMEDIATION_BUDGET => InFlightHealth::Failed,
        Sealed::Degraded(_) => InFlightHealth::Degraded,
        // Tail has no successor by construction — `next_req_at` we
        // passed above is None, and `seal` only returns Preempted
        // when `next_req_at.is_some()`. This arm is genuinely
        // uninhabitable.
        Sealed::Preempted => unreachable!("tail round has no next request by construction"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitHubLogin;
    use crate::observe::github::issue_events::{Actor, IssueEvent, UserRef};
    use crate::observe::github::requested_reviewers::{
        RequestedReviewers, RequestedUser, UserType,
    };
    use crate::observe::github::review_threads::{
        CommentAuthor, PageInfo, ReviewRequestsPage, ReviewThread, ReviewThreadsData,
        ReviewThreadsPage, ReviewThreadsPr, ReviewThreadsRepo, ReviewThreadsResponse,
        ThreadComment, ThreadComments,
    };
    use crate::observe::github::reviews::{PullRequestReview, ReviewState, ReviewUser};

    const HEAD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }
    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }
    fn enabled() -> CopilotRepoConfig {
        CopilotRepoConfig {
            enabled: true,
            review_on_push: false,
            review_draft_pull_requests: false,
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
    fn empty_reqs() -> RequestedReviewers {
        RequestedReviewers {
            users: vec![],
            teams: vec![],
        }
    }
    fn copilot_review(sha: &str, at: &str, body: &str) -> PullRequestReview {
        PullRequestReview {
            user: Some(ReviewUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
            }),
            state: ReviewState::Commented,
            commit_id: GitCommitSha::parse(sha).unwrap(),
            submitted_at: Some(ts(at)),
            body: body.into(),
            html_url: String::new(),
        }
    }
    fn req_event(at: &str, login: &str) -> IssueEvent {
        IssueEvent {
            event: "review_requested".into(),
            actor: Some(Actor {
                login: GitHubLogin::parse("alice").unwrap(),
            }),
            created_at: Some(ts(at)),
            requested_reviewer: Some(UserRef {
                login: GitHubLogin::parse(login).unwrap(),
            }),
            requested_team: None,
            // review_requested events are NOT app-performed; only
            // the copilot_work_* events are. Defaulting to None
            // matches what GitHub returns on the wire.
            performed_via_github_app: None,
        }
    }
    fn ack_event(at: &str) -> IssueEvent {
        IssueEvent {
            event: "copilot_work_started".into(),
            actor: None,
            created_at: Some(ts(at)),
            requested_reviewer: None,
            requested_team: None,
            performed_via_github_app: Some(crate::observe::github::issue_events::GitHubAppRef {
                slug: "copilot-pull-request-reviewer".into(),
            }),
        }
    }

    /// Test-only fixed clock. All baseline tests place events around
    /// 2026-04-23T10:00:00–10:05:00; this clock is later than every
    /// in-flight event but well under the start/review timeouts, so
    /// health computation reports Healthy unless a test deliberately
    /// engineers a degraded scenario.
    fn fixed_now() -> Timestamp {
        ts("2026-04-23T10:02:00Z")
    }

    /// Test wrapper: pre-existing tests were written before clock and
    /// commit injection landed. They construct events/rounds without
    /// commits, so `head_at()` returns `None` and the per-HEAD round
    /// list is empty — `compute_in_flight_health` returns Healthy by
    /// default, preserving the original behavior. New scenario tests
    /// pass `commits` and `now` directly to `orient_copilot`.
    fn orient_copilot_test(
        config: CopilotRepoConfig,
        events: &[IssueEvent],
        reviews: &[PullRequestReview],
        threads: &ReviewThreadsResponse,
        requested: &RequestedReviewers,
        head: &GitCommitSha,
    ) -> Option<CopilotReport> {
        orient_copilot(
            config,
            events,
            reviews,
            threads,
            requested,
            head,
            &[],
            fixed_now(),
        )
    }
    // ── identity ──

    #[test]
    fn is_copilot_recognizes_all_known_variants() {
        assert!(is_copilot("copilot-pull-request-reviewer[bot]"));
        assert!(is_copilot("Copilot"));
        assert!(is_copilot("copilot-pull-request-reviewer"));
        assert!(!is_copilot("Cursor"));
        assert!(!is_copilot("alice"));
    }

    // ── body parsing ──

    #[test]
    fn parse_visible_count_from_review_body() {
        let (v, _) = parse_copilot_review_body("Copilot reviewed and generated 3 comments. End.");
        assert_eq!(v, 3);
    }
    #[test]
    fn parse_visible_count_singular() {
        let (v, _) = parse_copilot_review_body("generated 1 comment.");
        assert_eq!(v, 1);
    }
    #[test]
    fn parse_no_new_comments_zero_case() {
        let (v, _) = parse_copilot_review_body("Copilot generated no new comments.");
        assert_eq!(v, 0);
    }
    #[test]
    fn parse_suppressed_count() {
        let (v, s) = parse_copilot_review_body(
            "generated 2 comments. Comments suppressed due to low confidence (5)",
        );
        assert_eq!(v, 2);
        assert_eq!(s, 5);
    }
    #[test]
    fn parse_returns_zero_when_neither_pattern_present() {
        let (v, s) = parse_copilot_review_body("nothing matches here");
        assert_eq!(v, 0);
        assert_eq!(s, 0);
    }
    #[test]
    fn parse_suppressed_with_nested_paren_still_extracts_count() {
        // Regression: old find(suffix) returned at the inner `)`
        // and parsed "5 of 12" → 0. Digit-run terminator avoids it.
        let (_, s) = parse_copilot_review_body(
            "generated 2 comments. Comments suppressed due to low confidence (5 of 12 hidden)",
        );
        assert_eq!(s, 5);
    }

    // ── orient_copilot returns None when disabled ──

    #[test]
    fn returns_none_when_config_disabled() {
        let cfg = CopilotRepoConfig {
            enabled: false,
            review_on_push: false,
            review_draft_pull_requests: false,
        };
        let r = orient_copilot_test(cfg, &[], &[], &empty_threads(), &empty_reqs(), &head());
        assert!(r.is_none());
    }

    // ── activity transitions ──

    #[test]
    fn idle_when_no_rounds_and_no_pending() {
        let r = orient_copilot_test(
            enabled(),
            &[],
            &[],
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.activity, CopilotActivity::Idle);
        assert_eq!(r.tier, CopilotTier::Bronze);
        assert!(!r.fresh);
    }

    #[test]
    fn requested_when_pending_in_requested_reviewers_and_no_rounds() {
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let events = vec![req_event("2026-04-23T10:00:00Z", "Copilot")];
        let r =
            orient_copilot_test(enabled(), &events, &[], &empty_threads(), &reqs, &head()).unwrap();
        assert!(matches!(r.activity, CopilotActivity::Requested { .. }));
    }

    #[test]
    fn working_when_acked_but_no_review_yet() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        // Copilot must still be in requested_reviewers for Working
        // to fire — represents a live in-flight review.
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let r =
            orient_copilot_test(enabled(), &events, &[], &empty_threads(), &reqs, &head()).unwrap();
        assert!(matches!(r.activity, CopilotActivity::Working { .. }));
        assert_eq!(r.rounds.len(), 1);
        assert!(r.rounds[0].ack_at.is_some());
        assert!(r.rounds[0].reviewed_at.is_none());
    }

    #[test]
    fn working_when_review_on_push_acks_after_prior_round() {
        // review_on_push fires a new copilot_work_started AFTER a
        // previous Copilot Reviewed round, with no review_requested
        // event and Copilot NOT in requested_reviewers. Pre-fix the
        // guard `currently_pending || rounds.is_empty()` rejected
        // this orphan Ack because a prior round existed; decide()
        // then re-requested Copilot mid-flight (or worse, halted
        // Success). Now: the orphan Ack (timestamp not paired to
        // any round) triggers Working.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
            // Auto-review starts later, with no preceding request.
            ack_event("2026-04-23T11:00:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated 0 comments.",
        )];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert!(
            matches!(r.activity, CopilotActivity::Working { .. }),
            "review_on_push ack after prior round must be Working, got {:?}",
            r.activity
        );
    }

    #[test]
    fn requested_when_pending_reviewer_but_no_timeline_events() {
        // Eventual-consistency window: requested_reviewers already
        // contains Copilot but issue_events hasn't surfaced any
        // Copilot timeline event yet. Pre-fix the lack of last_event
        // routed to Idle → no candidate → Halt::Success while
        // Copilot review was pending. Now: synthetic Requested.
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let r = orient_copilot_test(enabled(), &[], &[], &empty_threads(), &reqs, &head()).unwrap();
        assert!(
            matches!(r.activity, CopilotActivity::Requested { .. }),
            "pending reviewer with no events must be Requested, got {:?}",
            r.activity
        );
    }

    #[test]
    fn working_when_auto_review_ack_without_request_event() {
        // review_on_push case: copilot_work_started fires without a
        // preceding review_requested event. correlate_rounds creates
        // no round (it iterates Requested events), so Phase 1 must
        // detect the orphan Ack from the timeline directly. Without
        // this, rounds is empty and currently_pending is false → Idle,
        // and decide() halts Success while Copilot is still reviewing.
        let events = vec![ack_event("2026-04-23T10:00:00Z")];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert!(
            matches!(r.activity, CopilotActivity::Working { .. }),
            "auto-review ack without request must be Working, got {:?}",
            r.activity
        );
    }

    #[test]
    fn requested_when_re_requested_after_review_event_not_yet_visible() {
        // PR has a prior Reviewed round. Copilot was just re-requested
        // (Full RerequestCopilot fired); requested_reviewers shows
        // Copilot but the new review_requested event hasn't propagated
        // to issue_events yet. Pre-fix this returned Reviewed → decide
        // emitted RerequestCopilot AGAIN → runner halted Stalled
        // (repeated Full action). Now: returns Requested so decide
        // emits WaitForCopilotAck instead.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let reviews = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated 0 comments.",
        )];
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let r = orient_copilot_test(
            enabled(),
            &events,
            &reviews,
            &empty_threads(),
            &reqs,
            &head(),
        )
        .unwrap();
        assert!(
            matches!(r.activity, CopilotActivity::Requested { .. }),
            "Reviewed + currently_pending must be Requested (re-request just fired), got {:?}",
            r.activity
        );
    }

    #[test]
    fn idle_when_acked_but_request_removed_before_review() {
        // Regression: pre-fix, this returned Working solely on
        // ack_at presence, and decide() then emitted
        // WaitForCopilotReview indefinitely (Wait actions are
        // exempt from stall detection) until the iteration cap.
        // With the currently_pending guard, a removed request
        // collapses to Idle so the loop can advance other axes.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.activity, CopilotActivity::Idle);
        // The round itself is still preserved — observation shows
        // the work happened, even though the request was withdrawn.
        assert_eq!(r.rounds.len(), 1);
        assert!(r.rounds[0].ack_at.is_some());
    }

    #[test]
    fn reviewed_when_review_submitted_in_window() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated 0 comments.",
        )];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert!(matches!(r.activity, CopilotActivity::Reviewed { .. }));
        assert_eq!(r.rounds.len(), 1);
        assert_eq!(r.rounds[0].commit.as_ref().unwrap().as_str(), HEAD_SHA);
    }

    #[test]
    fn synthetic_round_for_review_without_request_event() {
        // review_on_push case: Copilot submitted a review without
        // any matching review_requested timeline event. Pre-fix,
        // the cursor advance would silently consume this review and
        // the report would look Idle, letting decide() halt
        // Success while Copilot's findings went unaddressed.
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated 0 comments.",
        )];
        let r = orient_copilot_test(
            enabled(),
            &[],
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert!(
            matches!(r.activity, CopilotActivity::Reviewed { .. }),
            "review with no preceding request must produce a round, got {:?}",
            r.activity
        );
        assert_eq!(r.rounds.len(), 1);
        assert_eq!(r.rounds[0].commit.as_ref().unwrap().as_str(), HEAD_SHA);
        assert!(r.rounds[0].reviewed_at.is_some());
        assert!(r.rounds[0].ack_at.is_none(), "no ack without a request");
    }

    #[test]
    fn multiple_reviews_in_same_window_drain_to_synthetic_rounds() {
        // Manual request at t=10, ack at t=11, two reviews in
        // window: t=15 (paired) and t=20 (review_on_push fired
        // after the manual one). Pre-fix: t=20 was silently lost
        // because the inner while broke after pairing t=15. Now:
        // 2 rounds — paired (with ack) and synthetic.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![
            copilot_review(HEAD_SHA, "2026-04-23T10:05:00Z", "generated 1 comment."),
            copilot_review(
                HEAD_SHA,
                "2026-04-23T10:10:00Z",
                "generated no new comments.",
            ),
        ];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.rounds.len(), 2, "got rounds: {:?}", r.rounds);
        // Paired round first (ack present).
        assert!(r.rounds[0].ack_at.is_some());
        assert_eq!(r.rounds[0].comments_visible, 1);
        // Synthetic second (no ack).
        assert!(r.rounds[1].ack_at.is_none());
        assert_eq!(r.rounds[1].comments_visible, 0);
        // rounds.last() must be the actual latest review for tier
        // scoring. Pre-fix it was the stale t=15 one.
        assert_eq!(
            r.rounds
                .last()
                .unwrap()
                .reviewed_at
                .as_ref()
                .unwrap()
                .to_string(),
            "2026-04-23T10:10:00+00:00"
        );
    }

    #[test]
    fn extra_review_after_last_window_emitted_as_synthetic() {
        // Single request at t=10, paired review at t=15, then a
        // second review at t=20 (review_on_push after the only
        // manual request, no further request event). Post-loop
        // drain catches it.
        let events = vec![req_event("2026-04-23T10:00:00Z", "Copilot")];
        let revs = vec![
            copilot_review(HEAD_SHA, "2026-04-23T10:05:00Z", "generated 0 comments."),
            copilot_review(HEAD_SHA, "2026-04-23T10:10:00Z", "generated 0 comments."),
        ];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.rounds.len(), 2);
        assert_eq!(
            r.rounds
                .last()
                .unwrap()
                .reviewed_at
                .as_ref()
                .unwrap()
                .to_string(),
            "2026-04-23T10:10:00+00:00"
        );
    }

    #[test]
    fn pre_request_review_gets_own_round_then_real_request_round() {
        // Mixed case: review_on_push fired at t=10, then the user
        // manually re-requested at t=20 and Copilot acked at t=21.
        // Two rounds expected: the synthetic pre-request round
        // (with reviewed_at) and the real request round (with ack
        // but no review yet).
        let reviews = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:00:00Z",
            "generated 1 comment.",
        )];
        let events = vec![
            req_event("2026-04-23T11:00:00Z", "Copilot"),
            ack_event("2026-04-23T11:01:00Z"),
        ];
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let r = orient_copilot_test(
            enabled(),
            &events,
            &reviews,
            &empty_threads(),
            &reqs,
            &head(),
        )
        .unwrap();
        assert_eq!(r.rounds.len(), 2);
        assert!(r.rounds[0].reviewed_at.is_some());
        assert!(r.rounds[0].ack_at.is_none());
        assert!(r.rounds[1].reviewed_at.is_none());
        assert!(r.rounds[1].ack_at.is_some());
    }

    // ── tier transitions ──

    #[test]
    fn tier_platinum_when_reviewed_at_head_clean() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated no new comments.",
        )];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CopilotTier::Platinum);
        assert!(r.fresh);
    }

    #[test]
    fn tier_gold_when_reviewed_at_non_head() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            OLD_SHA,
            "2026-04-23T10:05:00Z",
            "generated no new comments.",
        )];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CopilotTier::Gold);
        assert!(!r.fresh);
    }

    #[test]
    fn tier_silver_when_low_confidence_findings_present() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated 2 comments. Comments suppressed due to low confidence (3)",
        )];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.tier, CopilotTier::Silver);
    }

    #[test]
    fn tier_bronze_when_unresolved_threads_exist() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "generated no new comments.",
        )];
        let threads = ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo {
                                has_next_page: false,
                                end_cursor: None,
                            },
                            nodes: vec![ReviewThread {
                                id: String::new(),
                                is_resolved: false,
                                is_outdated: false,
                                path: String::new(),
                                line: None,
                                comments: ThreadComments {
                                    page_info: PageInfo::default(),
                                    nodes: vec![ThreadComment {
                                        author: Some(CommentAuthor {
                                            login: GitHubLogin::parse(
                                                "copilot-pull-request-reviewer",
                                            )
                                            .unwrap(),
                                        }),
                                        created_at: ts("2026-04-23T10:04:00Z"),
                                        body: "issue".into(),
                                    }],
                                },
                            }],
                        },
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        };
        let r = orient_copilot_test(enabled(), &events, &revs, &threads, &empty_reqs(), &head())
            .unwrap();
        assert_eq!(r.tier, CopilotTier::Bronze);
        assert_eq!(r.threads.unresolved, 1);
    }

    // ── round correlation ──

    #[test]
    fn two_rounds_correlate_correctly_to_two_reviews() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
            req_event("2026-04-23T11:00:00Z", "Copilot"),
            ack_event("2026-04-23T11:01:00Z"),
        ];
        let revs = vec![
            copilot_review(OLD_SHA, "2026-04-23T10:05:00Z", "generated 1 comment."),
            copilot_review(
                HEAD_SHA,
                "2026-04-23T11:05:00Z",
                "generated no new comments.",
            ),
        ];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &revs,
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert_eq!(r.rounds.len(), 2);
        assert_eq!(r.rounds[0].round, 1);
        assert_eq!(r.rounds[0].comments_visible, 1);
        assert_eq!(r.rounds[1].round, 2);
        assert_eq!(r.rounds[1].commit.as_ref().unwrap().as_str(), HEAD_SHA);
    }

    #[test]
    fn non_copilot_review_requests_ignored_by_timeline() {
        let events = vec![req_event("2026-04-23T10:00:00Z", "alice")];
        let r = orient_copilot_test(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &empty_reqs(),
            &head(),
        )
        .unwrap();
        assert!(r.rounds.is_empty());
        assert_eq!(r.activity, CopilotActivity::Idle);
    }

    // ── health computation ──

    fn commit_at(sha: &str, at: &str) -> Commit {
        Commit {
            oid: GitCommitSha::parse(sha).unwrap(),
            committed_date: ts(at),
        }
    }

    fn pending_copilot() -> RequestedReviewers {
        RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("Copilot").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        }
    }

    #[test]
    fn requested_healthy_within_start_window() {
        let events = vec![req_event("2026-04-23T10:00:00Z", "Copilot")];
        let commits = vec![commit_at(HEAD_SHA, "2026-04-23T09:50:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:05:00Z"),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CopilotActivity::Requested {
                requested_at: ts("2026-04-23T10:00:00Z"),
                health: InFlightHealth::Healthy,
            }
        );
    }

    #[test]
    fn requested_degraded_after_start_timeout() {
        // Request placed at 10:00, no ack landed; now is 10:15 — 15
        // minutes elapsed, past the 10-minute start threshold. Only
        // one round at HEAD → degraded_run = 1 < BUDGET = 2 →
        // Degraded, not Failed.
        let events = vec![req_event("2026-04-23T10:00:00Z", "Copilot")];
        let commits = vec![commit_at(HEAD_SHA, "2026-04-23T09:50:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:15:00Z"),
        )
        .unwrap();
        assert_eq!(
            r.activity,
            CopilotActivity::Requested {
                requested_at: ts("2026-04-23T10:00:00Z"),
                health: InFlightHealth::Degraded,
            }
        );
    }

    #[test]
    fn requested_failed_when_budget_exhausted() {
        // Two consecutive Degraded rounds at HEAD: first request at
        // 10:00, second at 10:20 (after the start-timeout cutoff for
        // the first round). Now is 10:35 — 15 minutes past the
        // second request, also past the threshold. degraded_run = 2,
        // BUDGET = 2 → Failed.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            req_event("2026-04-23T10:20:00Z", "Copilot"),
        ];
        let commits = vec![commit_at(HEAD_SHA, "2026-04-23T09:50:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:35:00Z"),
        )
        .unwrap();
        assert!(
            matches!(
                r.activity,
                CopilotActivity::Requested {
                    health: InFlightHealth::Failed,
                    ..
                }
            ),
            "expected Failed, got {:?}",
            r.activity
        );
    }

    #[test]
    fn working_degraded_after_review_timeout() {
        // Ack landed at 10:01; no review by 10:35 — 34 minutes past
        // ack, over the 30-minute review threshold.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let commits = vec![commit_at(HEAD_SHA, "2026-04-23T09:50:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:35:00Z"),
        )
        .unwrap();
        assert!(
            matches!(
                r.activity,
                CopilotActivity::Working {
                    health: InFlightHealth::Degraded,
                    ..
                }
            ),
            "expected Working(Degraded), got {:?}",
            r.activity
        );
    }

    #[test]
    fn swe_agent_events_filtered_at_axis_boundary() {
        // `copilot_work_started` performed by the coding-agent app
        // (slug = copilot-swe-agent) must NOT be treated as an ack
        // on the reviewer axis. Without the filter, a coding-agent
        // event would flip the reviewer state to Working.
        use crate::observe::github::issue_events::GitHubAppRef;
        let req = req_event("2026-04-23T10:00:00Z", "Copilot");
        let swe_ack = IssueEvent {
            event: "copilot_work_started".into(),
            actor: None,
            created_at: Some(ts("2026-04-23T10:01:00Z")),
            requested_reviewer: None,
            requested_team: None,
            performed_via_github_app: Some(GitHubAppRef {
                slug: "copilot-swe-agent".into(),
            }),
        };
        let events = vec![req, swe_ack];
        let commits = vec![commit_at(HEAD_SHA, "2026-04-23T09:50:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:05:00Z"),
        )
        .unwrap();
        // Reviewer-axis state must remain Requested (no reviewer-app
        // ack landed); the SWE-agent ack was filtered out.
        assert!(
            matches!(r.activity, CopilotActivity::Requested { .. }),
            "SWE-agent ack must not flip reviewer axis to Working, got {:?}",
            r.activity
        );
    }

    #[test]
    fn head_partition_excludes_pre_force_push_rounds() {
        // Two rounds: one against OLD_SHA, one against HEAD_SHA. The
        // OLD round should NOT count toward degraded_run for the
        // HEAD round, so a single timed-out HEAD round stays
        // Degraded (not Failed) even if the OLD round was also
        // timed out — the budget is per-HEAD.
        let events = vec![
            req_event("2026-04-23T09:00:00Z", "Copilot"),
            req_event("2026-04-23T10:00:00Z", "Copilot"),
        ];
        let commits = vec![
            commit_at(OLD_SHA, "2026-04-23T08:50:00Z"),
            // New HEAD landed between the two requests.
            commit_at(HEAD_SHA, "2026-04-23T09:30:00Z"),
        ];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T10:15:00Z"),
        )
        .unwrap();
        // Only the second round is at HEAD; degraded_run = 1 →
        // Degraded, not Failed.
        assert!(
            matches!(
                r.activity,
                CopilotActivity::Requested {
                    health: InFlightHealth::Degraded,
                    ..
                }
            ),
            "expected Degraded (per-HEAD budget), got {:?}",
            r.activity
        );
    }
}
