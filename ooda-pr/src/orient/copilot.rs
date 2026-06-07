//! Project event timeline, review submissions, and threads into the
//! per-PR state of a request-driven reviewer.
//!
//! # Invariants
//!
//! - **Configuration distinguishes from engagement**: `None` is
//!   emitted iff no reviewer policy applies; an enabled-but-dormant
//!   reviewer emits `Some(Idle)`. Collapsing the two would lose the
//!   "policy exists, no engagement" signal that drives request
//!   actions.
//! - **Round assembly is single-pass**: the timeline merge walks
//!   events and reviews in chronological order, attaching each
//!   review to the most-recent open request when possible and
//!   emitting a synthetic round otherwise. Auto-review acks (no
//!   preceding request) read directly from the timeline; this is
//!   the only orphan-ack path.
//! - **Health requires HEAD anchoring**: in-flight health is
//!   computed against rounds anchored to the current HEAD. HEAD
//!   movement implicitly resets the budget via SHA-equality
//!   filtering; orphaned rounds at an old HEAD do not feed health.
//! - **Tier is total**: every round set + thread state classifies
//!   into exactly one tier; the rules are evaluated first-match-
//!   wins.

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

/// Canonical write-side identity. The host's write surface accepts
/// only this form; read-side surfaces emit several aliases.
pub(crate) const COPILOT_REVIEWER_LOGIN: &str = "copilot-pull-request-reviewer[bot]";

/// Read-side identity aliases. The host emits different forms on
/// different surfaces; the predicate accepts every variant so the
/// axis classifies the reviewer consistently regardless of surface.
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
    /// Latest round's review-commit equals current HEAD.
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

// Health attaches only to in-flight variants. Idle and Reviewed
// carry no health — the "health is meaningful only when in flight"
// invariant is encoded structurally in the variant set.
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

/// Project the activity into a dashboard signal. `Idle` returns
/// `None` so the dashboard does not emit a row for a dormant axis.
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

// Per-axis health lattice. Same Healthy/Degraded/Failed shape as
// the sibling check axis's in-flight health; held independently per
// the anti-DRY mirror rule until a third axis surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum InFlightHealth {
    /// In flight within the timing budget.
    Healthy,
    /// Threshold crossed once at the current HEAD; one remediation
    /// remains in budget before escalation.
    Degraded,
    /// Threshold crossed and remediation budget exhausted; further
    /// re-requests would only restart the same failure mode.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Symptom {
    /// Request received no acknowledgement within the start budget.
    StartTimeout,
    /// Acknowledgement received but no review submitted within the
    /// review budget.
    ReviewTimeout,
}

/// Maximum request-to-ack dwell before a round classifies as start-
/// degraded.
pub(crate) const THRESHOLD_START_TIMEOUT: chrono::Duration = chrono::Duration::minutes(10);

/// Maximum ack-to-review dwell before a round classifies as review-
/// degraded.
pub(crate) const THRESHOLD_REVIEW_TIMEOUT: chrono::Duration = chrono::Duration::minutes(30);

/// Per-HEAD remediation budget. Degraded rounds tolerated at the
/// current HEAD before promotion to Failed.
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
    /// Per-entry payload of the low-confidence findings — file, line,
    /// and body extracted from the `<details>` block at the tail of the
    /// review body. The host posts these as text only (not as inline
    /// review threads), so they need a separate surface to drive agent
    /// work. `comments_suppressed` carries the host's stated count;
    /// this vec carries the witnesses. The two may diverge if the
    /// host's prose drifts or the block fails to parse — both are kept
    /// rather than derived so the stated count survives parser failure.
    pub suppressed_comments: Vec<SuppressedComment>,
}

/// One entry inside Copilot's "Comments suppressed due to low
/// confidence" `<details>` block. Carries the witness (path + line +
/// body) needed to drive an `AddressThreads` action that the host
/// never posted as an inline review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SuppressedComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CopilotTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

impl CopilotTier {
    /// Stable lowercase slug per variant. Identity-bearing —
    /// distinct tiers map to distinct slugs and renaming a variant
    /// requires updating this impl in lockstep with downstream
    /// gate-key consumers.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Bronze => "bronze",
            Self::Silver => "silver",
            Self::Gold => "gold",
            Self::Platinum => "platinum",
        }
    }
}

impl std::fmt::Display for CopilotTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

// Finite, slug-stable enum: same variant → same slug; distinct
// variants → distinct slugs. Satisfies gate-identity.
impl ooda_core::GateIdentity for CopilotTier {}

// ── Public entry point ───────────────────────────────────────────────

/// Project per-PR reviewer signal into the orient state.
///
/// Returns `None` iff the reviewer is not configured for this repo;
/// otherwise always `Some`. Engagement is encoded in the activity
/// variant, not in the Option layer.
///
/// The parameter list is long by design: this axis joins more
/// observation sources than its siblings. A context struct would
/// shift the surface area without shrinking it; the explicit list
/// keeps call-site clarity high.
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

/// Filter the event stream to reviewer-axis events.
///
/// Class invariant: two distinct apps share an ambiguous display
/// identity but emit the same event names. Disambiguation must
/// happen at the axis boundary via the originating app slug;
/// downstream consumers never re-classify.
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

/// Extract the visible-count and suppressed-count integers from a
/// review body.
///
/// Class invariant: a digit-run immediately following the count
/// prefix is the authoritative terminator. Surrounding-token
/// matching admits false anchors when the body wording shifts; the
/// digit-run does not.
pub(crate) fn parse_copilot_review_body(body: &str) -> (u32, u32) {
    let visible = if body.contains("generated no new comments") {
        0
    } else {
        find_count(body, "generated ").unwrap_or(0)
    };
    let suppressed = find_count(body, "Comments suppressed due to low confidence (").unwrap_or(0);
    (visible, suppressed)
}

/// Parse the leading ASCII-digit run immediately after `prefix`.
/// Digit-run termination is robust to parenthesized clarifiers and
/// any tokens that follow the count region.
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

/// Extract per-comment witnesses from Copilot's "Comments suppressed
/// due to low confidence" `<details>` block. Each entry inside the
/// block is a `**path:line**` header followed by a `* ` bullet line
/// carrying the issue body.
///
/// Returns an empty vec when the prefix isn't present, the block
/// isn't well-formed, or every entry inside fails to parse. The
/// parser is lenient on whitespace and on multi-line bodies (a body
/// run continues until the next `**…**` header or the block close)
/// and strict on the header shape (must end in `:<digits>**`).
pub(crate) fn extract_suppressed_comments(body: &str) -> Vec<SuppressedComment> {
    let Some(prefix_at) = body.find("Comments suppressed due to low confidence (") else {
        return Vec::new();
    };
    let Some(summary_end_rel) = body[prefix_at..].find("</summary>") else {
        return Vec::new();
    };
    let block_start = prefix_at + summary_end_rel + "</summary>".len();
    let block_end = body[block_start..]
        .find("</details>")
        .map_or(body.len(), |i| block_start + i);
    parse_suppressed_block(&body[block_start..block_end])
}

fn parse_suppressed_block(block: &str) -> Vec<SuppressedComment> {
    let mut out: Vec<SuppressedComment> = Vec::new();
    let mut current: Option<(String, u32, Vec<String>)> = None;
    for raw_line in block.lines() {
        let line = raw_line.trim();
        if let Some((path, line_no)) = parse_entry_header(line) {
            flush_entry(&mut current, &mut out);
            current = Some((path, line_no, Vec::new()));
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some((_, _, body_parts)) = current.as_mut() {
            let body_line = line.strip_prefix("* ").unwrap_or(line).to_string();
            body_parts.push(body_line);
        }
    }
    flush_entry(&mut current, &mut out);
    out
}

fn flush_entry(current: &mut Option<(String, u32, Vec<String>)>, out: &mut Vec<SuppressedComment>) {
    if let Some((path, line, body_parts)) = current.take() {
        let body = body_parts.join("\n").trim().to_string();
        if !body.is_empty() {
            out.push(SuppressedComment { path, line, body });
        }
    }
}

/// Parse a `**path:line**` header, returning `(path, line)` on
/// success. The line number is the trailing decimal run after the
/// last `:` inside the bolded span. Returns `None` for any other
/// line shape — the parser silently skips those rather than misclassify.
fn parse_entry_header(line: &str) -> Option<(String, u32)> {
    let inner = line.strip_prefix("**")?.strip_suffix("**")?;
    let colon = inner.rfind(':')?;
    let (path, line_part) = inner.split_at(colon);
    let line_str = &line_part[1..];
    if path.is_empty() || line_str.is_empty() {
        return None;
    }
    let line_no: u32 = line_str.parse().ok()?;
    Some((path.to_string(), line_no))
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

/// Assemble per-round state via a single chronological merge of
/// timeline points (request, ack) and review submissions.
///
/// Class invariant: every review either pairs with the open round
/// or emits a synthetic round. A single merge-walk enforces this
/// inline; multi-pass drains would distribute the invariant across
/// disjoint code paths and admit edge omissions.
///
/// Push-driven auto-review submissions, multiple reviews inside one
/// request window, and orphan acks are all covered uniformly by the
/// same walk. Orphan acks (no preceding request) generate no round
/// here — `derive_activity` reads them directly from the timeline.
fn correlate_rounds(
    timeline: &[TimelinePoint],
    reviews: &[&PullRequestReview],
) -> Vec<CopilotReviewRound> {
    let mut sorted_reviews: Vec<&PullRequestReview> = reviews.to_vec();
    sorted_reviews.sort_by_key(|a| a.submitted_at);

    // Round indices are assigned after a final sort by anchor
    // timestamp. The open round may anchor earlier than synthetic
    // rounds emitted while it was still being assembled; only a
    // post-sort renumber preserves chronological order — and
    // therefore the correctness of last-round selection and tier
    // scoring.
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
        round.suppressed_comments = extract_suppressed_comments(&rev.body);
    };

    for point in timeline {
        // Pre-point drain: every review strictly earlier than the
        // current timeline point lands now — either pairing with
        // the open round or emitting a synthetic round.
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
                    suppressed_comments: Vec::new(),
                });
                paired = false;
            }
            TimelineKind::Ack => {
                if let Some(round) = open.as_mut()
                    && round.ack_at.is_none()
                {
                    round.ack_at = Some(point.at);
                }
                // Orphan acks (no preceding request) generate no
                // round here; the activity classifier reads them
                // from the raw timeline.
            }
        }
    }

    // Post-timeline drain: reviews after the last event.
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

    // Final pass: stable-sort by anchor and renumber. Ties on
    // anchor (synthetic rounds with identical review timestamps)
    // preserve insertion order.
    rounds.sort_by_key(|r| r.requested_at);
    for (i, r) in rounds.iter_mut().enumerate() {
        r.round = u32::try_from(i).expect("review round index fits in u32") + 1;
    }
    rounds
}

/// Round for a review with no preceding request. The review's own
/// timestamp serves as the anchor; ack and request are absent by
/// construction.
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
        suppressed_comments: extract_suppressed_comments(&rev.body),
    }
}

// ── Tier scoring ─────────────────────────────────────────────────────

/// Tier classification, first-match-wins:
///   bronze:   no review, or in-flight, or actionable threads present
///   silver:   reviewed, no actionable threads, suppressed findings present
///   gold:     reviewed, no actionable, no suppressed, but stale or off-HEAD
///   platinum: reviewed, no actionable, no suppressed, no stale, latest at HEAD
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

/// In-flight stage divorced from health. The activity derivation
/// computes the bare stage first, then wraps in-flight stages with
/// timing-derived health.
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

    // Phase 1: working detector. An ack newer than the latest
    // review witnesses active review work — but only when the work
    // is genuinely in flight, not when the request was withdrawn
    // after ack.
    //
    // Class invariant: an ack is orphan iff its timestamp does not
    // pair with any round's ack. Genuine-in-flight is witnessed
    // either by a currently-pending request (re-request mid-flight)
    // or by an orphan ack (auto-review from push). Withdrawn-after-
    // ack does not qualify — its ack is already paired and the
    // detector falls through.
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
            // Class invariant: a reviewer in the currently-
            // requested set overrides a prior Reviewed state — the
            // re-request has not yet propagated to the event
            // stream. Collapsing to Reviewed would trigger a
            // redundant request action and the repeated-action
            // guard would halt the loop.
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
            // Class invariant: in-flight working state requires the
            // reviewer to be in the currently-requested set. Acked-
            // then-withdrawn does not qualify; otherwise the wait
            // action runs unbounded (Wait is stall-exempt).
            if currently_pending(requested) {
                return BareStage::Working {
                    requested_at: latest.requested_at,
                    ack_at: *ack,
                };
            }
            return BareStage::Idle;
        }
        // No ack on the latest round — distinguish pending-request
        // from request-withdrawn-before-ack.
        if currently_pending(requested) {
            return BareStage::Requested {
                requested_at: latest.requested_at,
            };
        }
        return BareStage::Idle;
    }
    let pending = currently_pending(requested);
    if pending {
        // Eventual-consistency window: the reviewer is in the
        // currently-requested set but no timeline event has
        // surfaced yet. Synthetic anchor preserves the Requested
        // stage so the wait action keeps the loop alive instead of
        // halting Success while the review is genuinely pending.
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

/// Per-round terminal classification against horizon `tau`. For
/// non-tail rounds `tau` is the next round's request timestamp; for
/// the tail it is `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sealed {
    Resolved,
    Healthy,
    Degraded(Symptom),
    /// Non-tail round superseded by a later request before any
    /// terminal signal landed. Tail rounds cannot reach this by
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

// Filter by SHA-equality against the current HEAD. Push-event
// signals are unreliable under load; the stored HEAD SHA is the
// authoritative anchor.
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

/// HEAD-at-time projection: the most-recent commit on or before `t`.
/// Absent when `commits` is empty or every commit is strictly after
/// `t` (pre-history request).
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
        // Two surface-identical sub-cases get distinct treatment:
        //   - No rounds at all: no timing signal; Healthy.
        //   - Rounds exist but all filtered to past HEADs (post-
        //     force-push). If enough time has elapsed since the new
        //     HEAD landed for a fresh request to surface and none
        //     has, the request slot is orphaned at the new HEAD and
        //     remediation cannot recover — escalate to Failed
        //     directly, skipping the budget dance.
        if rounds.is_empty() {
            return InFlightHealth::Healthy;
        }
        let head_committed_at = commits
            .iter()
            .find(|c| c.oid == *head)
            .map(|c| c.committed_date);
        return match head_committed_at {
            Some(t) if now.at() - t.at() > THRESHOLD_START_TIMEOUT => InFlightHealth::Failed,
            _ => InFlightHealth::Healthy,
        };
    }
    let tail_idx = rounds_h.len() - 1;
    // Walk tail-first; for each round, tau is the next round's
    // request anchor (non-tail) or `now` (tail). Counting halts at
    // the first non-Degraded round.
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
    // Resolved and Healthy collapse to Healthy at the tail; the
    // arms are kept distinct for parity with the per-round seal
    // classifier.
    #[allow(clippy::match_same_arms)]
    match seal(tail, None, now) {
        Sealed::Resolved => InFlightHealth::Healthy,
        Sealed::Healthy => InFlightHealth::Healthy,
        Sealed::Degraded(_) if degraded_run >= HEALTH_REMEDIATION_BUDGET => InFlightHealth::Failed,
        Sealed::Degraded(_) => InFlightHealth::Degraded,
        // Tail never has a successor: `next_req_at` passed in is
        // None, and the seal classifier only emits Preempted when
        // next_req_at is Some. Genuinely uninhabitable.
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

    // ── suppressed-entry extraction ──

    const SAMPLE_DETAILS: &str = "\
generated 0 comments.

<details>
<summary>Comments suppressed due to low confidence (2)</summary>

**src/core/config/src/impls/consensus.rs:16**
* This doc comment still says \"workflow consensus protocol\".

**src/core/config/src/impls/consensus.rs:19**
* `ConsensusConfig` is now a public type in `w3_config::impls::consensus`.

</details>
";

    #[test]
    fn extract_suppressed_returns_empty_when_marker_absent() {
        assert!(extract_suppressed_comments("generated 3 comments.").is_empty());
    }

    #[test]
    fn extract_suppressed_parses_two_entries_from_sample_block() {
        let entries = extract_suppressed_comments(SAMPLE_DETAILS);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "src/core/config/src/impls/consensus.rs");
        assert_eq!(entries[0].line, 16);
        assert_eq!(
            entries[0].body,
            "This doc comment still says \"workflow consensus protocol\"."
        );
        assert_eq!(entries[1].line, 19);
        assert!(entries[1].body.starts_with("`ConsensusConfig`"));
    }

    #[test]
    fn extract_suppressed_drops_header_with_non_numeric_line() {
        let body = "<summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:bogus**\n* should be skipped\n\
                    **src/y.rs:7**\n* this one is kept\n";
        let entries = extract_suppressed_comments(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/y.rs");
        assert_eq!(entries[0].line, 7);
    }

    #[test]
    fn extract_suppressed_skips_empty_bodies() {
        let body = "<summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n\n\
                    **src/y.rs:2**\n* real body\n";
        let entries = extract_suppressed_comments(body);
        assert_eq!(entries.len(), 1, "header-only entry must not survive");
        assert_eq!(entries[0].path, "src/y.rs");
    }

    #[test]
    fn extract_suppressed_concatenates_multi_line_body() {
        let body = "<summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n\
                    * first line\n\
                    continuation\n";
        let entries = extract_suppressed_comments(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "first line\ncontinuation");
    }

    #[test]
    fn extract_suppressed_stops_at_details_close() {
        let body = "<summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n* before close\n\
                    </details>\n\
                    **src/y.rs:2**\n* outside block — must be ignored\n";
        let entries = extract_suppressed_comments(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/x.rs");
    }

    #[test]
    fn round_populated_with_suppressed_entries_from_review_body() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "Copilot"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            SAMPLE_DETAILS,
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
        assert_eq!(r.rounds.len(), 1);
        let round = &r.rounds[0];
        assert_eq!(round.comments_suppressed, 2);
        assert_eq!(round.suppressed_comments.len(), 2);
        assert_eq!(round.suppressed_comments[0].line, 16);
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
    fn requested_failed_when_orphaned_after_force_push() {
        // Force-push scenario: req_event at 09:15 was at pre-push
        // HEAD (sha PRE). Force-push at 09:30 advanced HEAD to
        // HEAD_SHA. No new req_event arrived. By 09:45 (15min after
        // the new HEAD was pushed, past the 10min start threshold),
        // the events-feed lag explanation is exhausted —
        // orphaned-pending confirmed. RerequestCopilot is empirically
        // a no-op against pending state, so escalate directly to
        // Failed (→ EscalateCopilotFailed → HandoffHuman).
        const PRE_PUSH_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let events = vec![req_event("2026-04-23T09:15:00Z", "Copilot")];
        let commits = vec![
            commit_at(PRE_PUSH_SHA, "2026-04-23T09:00:00Z"),
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
            ts("2026-04-23T09:45:00Z"),
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
            "expected Requested(Failed), got {:?}",
            r.activity
        );
    }

    #[test]
    fn requested_healthy_within_window_after_force_push() {
        // Same force-push setup as the orphaned test, but only 5min
        // after the new HEAD push — under the 10min threshold. The
        // events feed may still deliver a fresh request event;
        // respect the ack window.
        const PRE_PUSH_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let events = vec![req_event("2026-04-23T09:15:00Z", "Copilot")];
        let commits = vec![
            commit_at(PRE_PUSH_SHA, "2026-04-23T09:00:00Z"),
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
            ts("2026-04-23T09:35:00Z"),
        )
        .unwrap();
        assert!(
            matches!(
                r.activity,
                CopilotActivity::Requested {
                    health: InFlightHealth::Healthy,
                    ..
                }
            ),
            "expected Requested(Healthy), got {:?}",
            r.activity
        );
    }

    #[test]
    fn requested_healthy_when_head_not_in_commits() {
        // Defensive: HEAD SHA is absent from the commits list (an
        // observation-invariant violation). Don't escalate spuriously;
        // fall back to Healthy.
        const PRE_PUSH_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let events = vec![req_event("2026-04-23T09:15:00Z", "Copilot")];
        let commits = vec![commit_at(PRE_PUSH_SHA, "2026-04-23T09:00:00Z")];
        let r = orient_copilot(
            enabled(),
            &events,
            &[],
            &empty_threads(),
            &pending_copilot(),
            &head(),
            &commits,
            ts("2026-04-23T09:45:00Z"),
        )
        .unwrap();
        assert!(
            matches!(
                r.activity,
                CopilotActivity::Requested {
                    health: InFlightHealth::Healthy,
                    ..
                }
            ),
            "expected Requested(Healthy), got {:?}",
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
