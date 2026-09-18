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

use std::collections::HashMap;

use crate::ids::{GitCommitSha, GitHubLogin, Timestamp};
use crate::observe::github::issue_events::IssueEvent;
use crate::observe::github::pull_request_view::Commit;
use crate::observe::github::requested_reviewers::{RequestedReviewers, UserType};
use crate::observe::github::review_threads::ReviewThreadsResponse;
use crate::observe::github::reviews::PullRequestReview;
use crate::observe::github::rulesets::CopilotCodeReviewParams;
use serde::Serialize;

use super::bot_threads::{BotThreadSummary, count_bot_threads};

// ── Identity ─────────────────────────────────────────────────────────

/// Canonical write-side identity. The host's write surface accepts
/// only this form; read-side surfaces emit several aliases.
pub(crate) const COPILOT_REVIEWER_LOGIN: &str = "copilot-pull-request-reviewer[bot]";

/// Recognized bot-form logins for the Copilot reviewer. Bare-string
/// surface forms (`"Copilot"`, `"copilot-pull-request-reviewer"`) are
/// intentionally absent: the predicate gates on the structural
/// [`GitHubLogin::is_bot`] check first, so a bare login (which any
/// plain-user account can register) cannot impersonate the reviewer
/// regardless of allowlist contents. Surfaces that previously emitted
/// the bare forms must now emit the `[bot]`-suffixed or `app/`-prefixed
/// canonical form to be classified as Copilot.
const COPILOT_LOGINS: &[&str] = &[COPILOT_REVIEWER_LOGIN, "app/copilot-pull-request-reviewer"];

pub(crate) fn is_copilot(login: &str) -> bool {
    GitHubLogin::parse(login).is_ok_and(|l| l.is_bot()) && COPILOT_LOGINS.contains(&login)
}

/// Bare display login the host emits for the Copilot reviewer on
/// type-attested surfaces (timeline `requested_reviewer`, requested
/// reviewers, review authors).
const COPILOT_DISPLAY_LOGIN: &str = "Copilot";

/// Classifies a `(login, host-attested account type)` pair as the
/// Copilot reviewer. The bare display login is accepted only under
/// the host's `Bot` attestation: any plain-user account can register
/// a bare name, but cannot carry the `Bot` type. Bot-form logins
/// classify regardless of attestation ([`is_copilot`]).
pub(crate) fn is_copilot_ref(login: &str, user_type: Option<UserType>) -> bool {
    is_copilot(login) || (user_type == Some(UserType::Bot) && login == COPILOT_DISPLAY_LOGIN)
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
    /// Count of findings the review renders as inline review threads:
    /// `max(stated, structural)` — the count stated on the body's
    /// findings label joined with the count of review threads whose
    /// first comment belongs to this review. Two channels with
    /// disjoint failure domains, same design as
    /// `comments_suppressed`.
    pub comments_visible: u32,
    /// Count of findings the review renders in the body rather than
    /// as inline threads: `max(|suppressed_comments|, stated count)`
    /// per [`parse_copilot_review_body`]. Never less than what either
    /// extraction channel attests.
    pub comments_suppressed: u32,
    /// Per-entry witnesses of the body-rendered findings — file,
    /// line, and body extracted from the review body's `<details>`
    /// blocks. The host posts these as text only (not as inline
    /// review threads), so they need a separate surface to drive
    /// agent work. Extraction is structural (entry shape), while
    /// `comments_suppressed` also reads the host's stated count via
    /// a lexical stem; the disjoint failure domains keep the count
    /// alive when the witness parse fails, and vice versa.
    pub suppressed_comments: Vec<SuppressedComment>,
}

/// One finding rendered in the review body's `<details>` block
/// instead of as an inline review thread. Carries the witness (path +
/// line + body) needed to drive an `AddressThreads` action for
/// feedback the host never posted as a thread.
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
                .is_some_and(|u| is_copilot_ref(u.login.as_str(), u.user_type))
        })
        .collect();

    let reviewer_events = copilot_reviewer_events(events);
    let timeline = copilot_timeline(&reviewer_events);
    let visible_by_review = thread_counts_by_review(threads);
    let rounds = correlate_rounds(&timeline, &copilot_reviews, &visible_by_review);
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

/// Signals parsed from one Copilot review body. One parse per body;
/// `suppressed` coheres with `suppressed_comments` by construction
/// (see [`parse_copilot_review_body`]).
pub(crate) struct ReviewBodySignals {
    /// Stated channel only. The structural channel (thread linkage)
    /// joins at round assembly in [`correlate_rounds`], which sees
    /// the thread data this parser does not.
    pub visible: u32,
    pub suppressed: u32,
    pub suppressed_comments: Vec<SuppressedComment>,
}

/// Parse a review body into its signals.
///
/// # Host grammars
///
/// The host has rewritten this body's markup while keeping the same
/// information, and both revisions remain readable on any PR whose
/// history spans the change. Every anchor here is therefore a *shape*
/// — element nesting, line delimiters, digit runs — never a sentence.
/// The two shapes observed in the wild:
///
/// - **flat** — one `<details>` element holding a markdown heading per
///   section, with `**path:line**`-headed entries under the heading
///   that governs body-rendered findings.
/// - **nested** — one `<details>` element per section; the section of
///   body-rendered findings holds one nested `<details>` per finding,
///   whose content opens with a `` `path:line` `` locator.
///
/// One walk covers both: [`details_blocks`] flattens the element tree
/// and any block whose content carries locator lines yields entries.
///
/// # Channels
///
/// Suppressed findings have two extraction channels with disjoint
/// failure domains:
///
/// - **witnesses** — structural: locator-headed entries inside any
///   `<details>` block ([`block_entries`]). Immune to
///   section-title rewording.
/// - **stated count** — lexical stem ([`stated_suppressed_count`]).
///   Immune to entry-grammar drift.
///
/// `suppressed = max(|witnesses|, stated)`: never less than what
/// either channel attests, so a single-channel parse failure degrades
/// the payload (count without witnesses, or vice versa) instead of
/// silently reporting zero. The two channels share no anchor; only
/// simultaneous drift of both surfaces can zero the signal.
pub(crate) fn parse_copilot_review_body(body: &str) -> ReviewBodySignals {
    let blocks = details_blocks(body);
    let suppressed_comments = block_entries(&blocks);
    let witnesses = u32::try_from(suppressed_comments.len()).unwrap_or(u32::MAX);
    ReviewBodySignals {
        visible: stated_visible_count(body),
        suppressed: witnesses.max(stated_suppressed_count(&blocks)),
        suppressed_comments,
    }
}

// ── Markup reduction ─────────────────────────────────────────────────

/// Invisible formatting code points. The host renders paths with
/// zero-width break opportunities between segments; they carry no path
/// semantics and make the path unusable as a filesystem key, so every
/// parsed path is filtered through [`strip_invisible`].
fn is_invisible_format(c: char) -> bool {
    matches!(c,
        '\u{00ad}' | '\u{200b}'..='\u{200f}' | '\u{2060}'..='\u{2064}' | '\u{feff}')
}

fn strip_invisible(s: &str) -> String {
    s.chars().filter(|c| !is_invisible_format(*c)).collect()
}

/// Reduce markup-bearing text to the text it renders by dropping every
/// `<…>` span. Total — an unterminated `<` consumes the rest of the
/// input. Attribute values are dropped with their tag, so tag geometry
/// contributes no digits to a count read off the reduced text.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// A markdown ATX heading line.
fn is_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes) && line[hashes..].starts_with(' ')
}

// ── Element tree ─────────────────────────────────────────────────────

/// One `<details>` element, flattened out of the body's element tree.
struct DetailsBlock {
    /// `<summary>` text with markup removed; empty when absent.
    summary: String,
    /// Content between `</summary>` and the matching `</details>`,
    /// with nested `<details>` elements elided — each nested element
    /// is flattened into its own [`DetailsBlock`].
    content: String,
    /// The block renders an in-page anchor link of its own. Findings
    /// rendered as links are inline review threads, which the visible
    /// channel counts; body-rendered findings never carry one.
    linked: bool,
}

const DETAILS_CLOSE: &str = "</details>";

/// Flatten the body's `<details>` elements. Total — a body with no
/// elements yields an empty list, and an unclosed `<details>` governs
/// to the end of the body.
fn details_blocks(body: &str) -> Vec<DetailsBlock> {
    let mut opens: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = body[cursor..].find('<') {
        let at = cursor + rel;
        if body[at..].starts_with(DETAILS_CLOSE) {
            if let Some(start) = opens.pop() {
                out.push(details_block(&body[start..at]));
            }
            cursor = at + DETAILS_CLOSE.len();
        } else if let Some(after) = open_details_end(&body[at..]) {
            opens.push(at + after);
            cursor = at + after;
        } else {
            cursor = at + 1;
        }
    }
    for start in opens.into_iter().rev() {
        out.push(details_block(&body[start..]));
    }
    out
}

/// Offset just past the `>` of a `<details …>` open tag at the head of
/// `s`, or `None` when `s` does not open one.
fn open_details_end(s: &str) -> Option<usize> {
    const OPEN: &str = "<details";
    let rest = s.strip_prefix(OPEN)?;
    if !rest.starts_with('>') && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(OPEN.len() + rest.find('>')? + 1)
}

fn details_block(inner: &str) -> DetailsBlock {
    let (summary, rest) = split_summary(inner);
    let content = elide_nested_details(rest);
    let linked = summary.contains("](#") || content.contains("](#");
    DetailsBlock {
        summary: strip_tags(summary).trim().to_string(),
        content,
        linked,
    }
}

/// Split an element's own `<summary>` off its content. The summary is
/// the element's first child; a `<summary>` appearing only after a
/// nested `<details>` belongs to that nested element, not this one.
fn split_summary(inner: &str) -> (&str, &str) {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    let nested_at = inner.find("<details").unwrap_or(inner.len());
    let Some(open_at) = inner.find(OPEN).filter(|at| *at < nested_at) else {
        return ("", inner);
    };
    let from = open_at + OPEN.len();
    let Some(rel) = inner[from..].find(CLOSE) else {
        return (&inner[from..], "");
    };
    (&inner[from..from + rel], &inner[from + rel + CLOSE.len()..])
}

/// Drop nested `<details>` elements from an element's content, so a
/// block's content is the text it renders itself.
fn elide_nested_details(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut cursor = 0;
    while let Some(rel) = s[cursor..].find('<') {
        let at = cursor + rel;
        if depth == 0 {
            out.push_str(&s[cursor..at]);
        }
        if s[at..].starts_with(DETAILS_CLOSE) {
            depth = depth.saturating_sub(1);
            cursor = at + DETAILS_CLOSE.len();
        } else if let Some(after) = open_details_end(&s[at..]) {
            depth += 1;
            cursor = at + after;
        } else {
            if depth == 0 {
                out.push('<');
            }
            cursor = at + 1;
        }
    }
    if depth == 0 {
        out.push_str(&s[cursor..]);
    }
    out
}

// ── Entry witnesses ──────────────────────────────────────────────────

/// Extract per-comment witnesses from the body's flattened blocks.
///
/// Identification is structural, not lexical: a finding rendered in
/// the body is by definition not materialized as an inline review
/// thread, so body-rendered entries are the suppressed set. The host
/// owns the section titles and has reworded them; the entry shape is
/// the stable anchor, so blocks are classified by whether their
/// content parses, never by title.
///
/// Returns an empty vec when no block content parses.
fn block_entries(blocks: &[DetailsBlock]) -> Vec<SuppressedComment> {
    blocks.iter().flat_map(parse_block_entries).collect()
}

/// Parse one block's entries.
///
/// An entry opens at a locator line and runs to the next locator, to
/// the next block-structure marker (a markdown heading or a bold
/// label line), or to the block's end. The parser is lenient on
/// whitespace and multi-line bodies and strict on the locator shape,
/// so a non-entry line is skipped rather than misclassified. Entries
/// left with an empty body are dropped.
///
/// A block whose content *opens* with a locator and whose summary
/// states no count renders a single finding: the summary titles that
/// finding, and joins its body, which is otherwise the only place the
/// finding's headline would be lost. A summary that states a count
/// titles a set — a section of findings — rather than any one entry,
/// and stays out of every entry's body.
fn parse_block_entries(block: &DetailsBlock) -> Vec<SuppressedComment> {
    let mut out: Vec<SuppressedComment> = Vec::new();
    let mut current: Option<(String, u32, Vec<String>)> = None;
    let mut opens_with_locator: Option<bool> = None;
    for raw_line in block.content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((path, line_no)) = parse_entry_locator(line) {
            flush_entry(&mut current, &mut out);
            current = Some((path, line_no, Vec::new()));
            opens_with_locator.get_or_insert(true);
            continue;
        }
        opens_with_locator.get_or_insert(false);
        // Block-structure markers close the open entry: a heading or
        // a bold label line starts the block's own scaffolding (its
        // section titles and its per-review totals), which is not
        // finding prose and must not reach an agent as such.
        if is_heading(line) || bold_label(line).is_some() {
            flush_entry(&mut current, &mut out);
            continue;
        }
        if let Some((_, _, body_parts)) = current.as_mut() {
            body_parts.push(line.strip_prefix("* ").unwrap_or(line).to_string());
        }
    }
    flush_entry(&mut current, &mut out);
    if opens_with_locator == Some(true)
        && out.len() == 1
        && !block.summary.is_empty()
        && paren_count(&block.summary).is_none()
        && let Some(entry) = out.first_mut()
    {
        entry.body = if entry.body.is_empty() {
            block.summary.clone()
        } else {
            format!("{}\n\n{}", block.summary, entry.body)
        };
    }
    out.retain(|c| !c.body.is_empty());
    out
}

fn flush_entry(current: &mut Option<(String, u32, Vec<String>)>, out: &mut Vec<SuppressedComment>) {
    if let Some((path, line, body_parts)) = current.take() {
        out.push(SuppressedComment {
            path,
            line,
            body: body_parts.join("\n").trim().to_string(),
        });
    }
}

/// Parse a standalone entry locator, returning `(path, line)`.
///
/// Two host grammars, one shape: the locator occupies the whole line,
/// is delimited (bold `**…**` or code `` `…` ``), and ends in a
/// trailing decimal run after the last `:`. Returns `None` for any
/// other line shape.
fn parse_entry_locator(line: &str) -> Option<(String, u32)> {
    let inner = delimited(line, "**").or_else(|| delimited(line, "`"))?;
    let colon = inner.rfind(':')?;
    let (path, line_part) = inner.split_at(colon);
    let line_no: u32 = line_part[1..].parse().ok()?;
    let path = strip_invisible(path);
    if path.is_empty() {
        return None;
    }
    Some((path, line_no))
}

/// The span a line wraps in `delim`, when the line is exactly one such
/// span.
fn delimited<'a>(line: &'a str, delim: &str) -> Option<&'a str> {
    let inner = line.strip_prefix(delim)?.strip_suffix(delim)?;
    (!inner.is_empty() && !inner.contains(delim)).then_some(inner)
}

// ── Stated counts ────────────────────────────────────────────────────

/// The host's stated count of body-rendered findings.
///
/// Degraded-signal channel: when entry extraction fails wholesale, the
/// stated count still drives `comments_suppressed > 0` gates. Reading
/// it needs a lexical anchor — no markup distinguishes a section of
/// withheld findings from a section of skipped files — so this is the
/// one place a host wording is matched, and the stem set is this
/// channel's known failure surface. Both stems are observed in the
/// wild: `suppress` under the flat grammar, `miss` under the nested
/// one. Each block's summary and its own markdown headings are
/// matched, because the host has stated the count in both positions.
///
/// Sections are disjoint, so qualifying blocks sum; within one block
/// the positions restate a single count, so they max.
///
/// Blocks rendering in-page anchor links are excluded: those findings
/// are inline review threads, counted by the visible channel.
fn stated_suppressed_count(blocks: &[DetailsBlock]) -> u32 {
    blocks
        .iter()
        .filter(|b| !b.linked)
        .filter_map(block_stated_count)
        .fold(0u32, u32::saturating_add)
}

/// Bold label lines are deliberately not matched: under the flat
/// grammar the host partitions a section's total across bold labels
/// inside the same block, so reading them would double-count against
/// the heading that already states the total.
fn block_stated_count(block: &DetailsBlock) -> Option<u32> {
    const STEMS: &[&str] = &["suppress", "miss"];
    std::iter::once(block.summary.as_str())
        .chain(
            block
                .content
                .lines()
                .map(str::trim)
                .filter(|l| is_heading(l)),
        )
        .filter(|text| {
            let lower = text.to_ascii_lowercase();
            STEMS.iter().any(|stem| lower.contains(stem))
        })
        .filter_map(paren_count)
        .max()
}

/// Digit-run immediately after the first `(` that opens onto digits.
/// Digit-run termination is robust to trailing clarifiers
/// (`(5 of 12 hidden)` → 5); digit-free parens are skipped.
fn paren_count(summary: &str) -> Option<u32> {
    let mut rest = summary;
    while let Some(at) = rest.find('(') {
        let digits: String = rest[at + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
        rest = &rest[at + 1..];
    }
    None
}

/// The host's stated count of findings rendered as inline review
/// threads.
///
/// The weaker of this axis's two visible-count channels: the
/// structural one joined in [`correlate_rounds`] counts threads by
/// originating review id and is exact, where this one reads prose.
/// It carries the signal when thread data is unavailable.
///
/// The anchor is the line shape — a bold label whose value carries
/// digit runs — rather than any one sentence; the host has reworded
/// the label across grammars while keeping the shape, so the stems
/// `generated` (flat) and `finding` (nested) select the label and the
/// shape does the rest. A severity breakdown renders the total as
/// several runs on one line (`1 <high> · 1 <medium>`), so a line's
/// runs sum; markup is stripped first so tag geometry contributes no
/// digits.
fn stated_visible_count(body: &str) -> u32 {
    const STEMS: &[&str] = &["generated", "finding"];
    body.lines()
        .filter_map(bold_label)
        .filter(|(label, _)| {
            let lower = label.to_ascii_lowercase();
            STEMS.iter().any(|stem| lower.contains(stem))
        })
        .map(|(_, value)| sum_digit_runs(&strip_tags(value)))
        .max()
        .unwrap_or(0)
}

/// Split a `**label:** value` line into its label and value. Tolerates
/// a leading list-item bullet.
fn bold_label(line: &str) -> Option<(&str, &str)> {
    let text = line.trim();
    let text = text.strip_prefix("- ").unwrap_or(text);
    let rest = text.strip_prefix("**")?;
    let end = rest.find("**")?;
    Some((rest[..end].trim_end_matches(':'), &rest[end + 2..]))
}

/// Sum the ASCII digit runs in `s`.
fn sum_digit_runs(s: &str) -> u32 {
    let mut total = 0u32;
    let mut run = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else if !run.is_empty() {
            total = total.saturating_add(run.parse().unwrap_or(0));
            run.clear();
        }
    }
    if run.is_empty() {
        total
    } else {
        total.saturating_add(run.parse().unwrap_or(0))
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
                    && is_copilot_ref(rr.login.as_str(), rr.user_type)
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

/// Threads grouped by originating review: for each numeric review
/// id, the count of threads whose first comment was submitted under
/// it. The first comment determines the thread's originating review
/// — replies belong to their own (implicit) reviews and must not
/// attribute the thread. Threads without linkage (older fixtures,
/// deleted reviews) contribute nothing.
fn thread_counts_by_review(threads: &ReviewThreadsResponse) -> HashMap<u64, u32> {
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for t in &threads.data.repository.pull_request.review_threads.nodes {
        let Some(first) = t.comments.nodes.first() else {
            continue;
        };
        let Some(id) = first
            .pull_request_review
            .as_ref()
            .and_then(|r| r.database_id)
        else {
            continue;
        };
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
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
    visible_by_review: &HashMap<u64, u32>,
) -> Vec<CopilotReviewRound> {
    // Visible-count structural channel: threads originated by this
    // review, joined on the numeric review id. Max'd with the body's
    // stated "generated N" so either channel alone carries the
    // signal — same disjoint-failure-domain design as the suppressed
    // channels in `parse_copilot_review_body`.
    let structural_visible = |rev: &PullRequestReview| {
        rev.id
            .and_then(|id| visible_by_review.get(&id))
            .copied()
            .unwrap_or(0)
    };
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
        let signals = parse_copilot_review_body(&rev.body);
        round.comments_visible = signals.visible.max(structural_visible(rev));
        round.comments_suppressed = signals.suppressed;
        round.suppressed_comments = signals.suppressed_comments;
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
                    rounds.push(synthetic_round(rev, 0, structural_visible(rev)));
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
                rounds.push(synthetic_round(rev, 0, structural_visible(rev)));
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
fn synthetic_round(
    rev: &PullRequestReview,
    round_no: u32,
    structural_visible: u32,
) -> CopilotReviewRound {
    let signals = parse_copilot_review_body(&rev.body);
    let anchor = rev
        .submitted_at
        .unwrap_or_else(|| Timestamp::parse("1970-01-01T00:00:00Z").unwrap());
    CopilotReviewRound {
        round: round_no,
        requested_at: anchor,
        ack_at: None,
        reviewed_at: rev.submitted_at,
        commit: Some(rev.commit_id.clone()),
        comments_visible: signals.visible.max(structural_visible),
        comments_suppressed: signals.suppressed,
        suppressed_comments: signals.suppressed_comments,
    }
}

// ── Tier scoring ─────────────────────────────────────────────────────

/// Tier classification, first-match-wins:
///   bronze:   no review, or in-flight, or actionable threads present
///   silver:   reviewed, no actionable threads, suppressed findings present
///   gold:     reviewed, no actionable, no suppressed, and either
///             (a) stale-or-off-HEAD, or
///             (b) latest review at HEAD found comments — resolution
///                 without a fresh re-review does not confirm the
///                 fixes hold
///   platinum: reviewed at HEAD, no actionable, no suppressed, no
///             stale, and the latest review reported zero comments
///             — a fresh Copilot re-review at HEAD came back clean
///
/// The Gold-vs-Platinum discriminator is the latest round's
/// `comments_visible`. Marking threads resolved on a review that
/// found findings is human/agent judgment; Platinum requires
/// upstream (Copilot) to have re-inspected the code post-fix and
/// declared it clean. Decide's Copilot axis emits `RerequestCopilot`
/// while at Gold so the loop drives toward the zero-comment
/// re-review.
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
    let at_head = latest.commit.as_ref() == Some(head);
    if !at_head {
        return CopilotTier::Gold;
    }
    if latest.comments_visible > 0 {
        return CopilotTier::Gold;
    }
    CopilotTier::Platinum
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
    requested
        .users
        .iter()
        .any(|u| is_copilot_ref(u.login.as_str(), Some(u.user_type)))
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
            // `>=` matches the convention used by `seal()` for the
            // same threshold — "X has timed out" iff elapsed ≥ X.
            // Mismatched comparison (this site used `>`) produced
            // divergent verdicts at the exact-boundary case.
            Some(t) if now.at() - t.at() >= THRESHOLD_START_TIMEOUT => InFlightHealth::Failed,
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
            id: None,
            user: Some(ReviewUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
                user_type: None,
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
                user_type: None,
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
    /// Review body stating no inline findings, in the host's current
    /// label grammar.
    const NO_FINDINGS: &str = "**Findings:** None";

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
    fn is_copilot_requires_bot_form_login() {
        // Proper bot forms: pass structural is_bot() and allowlist.
        assert!(is_copilot("copilot-pull-request-reviewer[bot]"));
        assert!(is_copilot("app/copilot-pull-request-reviewer"));
        // Bare logins: rejected by is_bot() structural gate. A plain-
        // user account registering these handles cannot impersonate.
        assert!(!is_copilot("Copilot"));
        assert!(!is_copilot("copilot-pull-request-reviewer"));
        assert!(!is_copilot("Cursor"));
        assert!(!is_copilot("alice"));
        // Wrong bot: passes is_bot() but allowlist rejects.
        assert!(!is_copilot("cursor[bot]"));
        assert!(!is_copilot("random[bot]"));
    }

    #[test]
    fn is_copilot_rejects_malformed_and_empty() {
        assert!(!is_copilot(""));
        assert!(!is_copilot("[bot]"));
        assert!(!is_copilot("copilot\n"));
        assert!(!is_copilot("copilot bot"));
    }

    #[test]
    fn is_copilot_ref_accepts_bare_login_only_with_bot_attestation() {
        // The 2026-08 host migration: `requested_reviewer` carries the
        // bare display login with a host-attested account type.
        assert!(is_copilot_ref("Copilot", Some(UserType::Bot)));
        // No attestation (or non-Bot): a user-registrable bare name.
        assert!(!is_copilot_ref("Copilot", None));
        assert!(!is_copilot_ref("Copilot", Some(UserType::User)));
        assert!(!is_copilot_ref("Copilot", Some(UserType::Unknown)));
        // Attestation does not admit other identities.
        assert!(!is_copilot_ref("Cursor", Some(UserType::Bot)));
        assert!(!is_copilot_ref("alice", Some(UserType::Bot)));
        // Bot-form logins classify with or without attestation.
        assert!(is_copilot_ref("copilot-pull-request-reviewer[bot]", None));
        assert!(is_copilot_ref(
            "copilot-pull-request-reviewer[bot]",
            Some(UserType::Bot)
        ));
    }

    #[test]
    fn bare_copilot_request_event_opens_a_round() {
        // Regression: the host migration emits
        // `{login: "Copilot", type: "Bot"}` on `review_requested`.
        // Pre-fix this event was unclassified, no round opened, and
        // decide re-emitted RerequestCopilot until StuckRepeated.
        let mut e = req_event("2026-08-19T19:35:06Z", "alice");
        e.requested_reviewer = Some(UserRef {
            login: GitHubLogin::parse("Copilot").unwrap(),
            user_type: Some(UserType::Bot),
        });
        let events = [&e];
        let timeline = copilot_timeline(&events[..]);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, TimelineKind::Requested);
    }

    // ── body parsing ──

    /// Nested grammar, as the host emits it (observed 2026-09-18).
    /// One `<details>` per section; the section of body-rendered
    /// findings holds a nested `<details>` per finding, whose content
    /// opens with a backticked locator. Rendered paths carry
    /// zero-width break opportunities. One severity image is kept
    /// intact: its geometry attributes are the false-digit hazard the
    /// count channels must not read.
    const NESTED_BODY: &str = "\
## Copilot review overview

### Needs a closer look

Runtime validation is missing for bare units and rounding modes.

**Review effort:** Balanced
**Findings:** None

<details>
<summary><strong>Resolved since last review (2)</strong></summary>

- [Validate Money units are bigint](#discussion_r4051316354)
- [Validate rate ratio is bigint](#discussion_r4051316369)
</details>

<details>
<summary><strong>Previously missed (2)</strong></summary>

In code that hasn't changed since last review

<details>
<summary><img src=\"medium-v2.png\" alt=\"Medium severity\" width=\"62\" height=\"18\"> Validate units as bigint before formatting</summary>

`services/\u{200b}shared/\u{200b}src/\u{200b}money.ts:531`

This exported escape hatch bypasses the `bigint` validation used for `Money`.
</details>

<details>
<summary><img src=\"medium-v2.png\" alt=\"Medium severity\" width=\"62\" height=\"18\"> Reject invalid rounding modes before fast-path return</summary>

`services/\u{200b}shared/\u{200b}src/\u{200b}money.ts:949`

An invalid runtime rounding value silently falls through as `half-even` on ties.
</details>
</details>
";

    /// Nested grammar, the round where findings landed as inline
    /// threads: the severity breakdown splits the total across runs on
    /// the findings label, and the section renders each finding as an
    /// anchor link rather than as text.
    const NESTED_LINKED_BODY: &str = "\
## Copilot review overview

### Changes recommended

Structural inputs can bypass primitive validation.

**Review effort:** Balanced
**Findings:** 1 <picture><source srcset=\"high-v2-dark.svg\"><img src=\"high-v2.png\" alt=\"High severity\" width=\"62\" height=\"18\"></picture> \u{b7} 1 <picture><img src=\"medium-v2.png\" alt=\"Medium severity\" width=\"62\" height=\"18\"></picture>

<details open>
<summary><strong>Open (2)</strong></summary>

- [Validate Money units are bigint](#discussion_r4051316354) \u{b7} New
- [Validate rate ratio is bigint](#discussion_r4051316369) \u{b7} New
</details>
";

    /// Flat grammar, as the host emitted it through 2026-09-18: one
    /// `<details>` holding a markdown heading per section, bold-header
    /// entries, and the per-review totals as bold label lines.
    const FLAT_BODY: &str = "\
### Changes recommended

Checksum validation and implicit rounding need correction.

<details>
<summary>Review details</summary>

### Suppressed comments (2)

**services/shared/src/asset-id.ts:190**
* Validate mixed-case EVM references before canonicalizing them.

**services/shared/src/money.ts:446**
* Normalize the bigint operands before converting to `number`.

- **Files reviewed:** 5/6 changed files
- **Comments generated:** 3
- **Review effort level:** Balanced
</details>
";

    #[test]
    fn nested_grammar_extracts_body_rendered_findings() {
        let s = parse_copilot_review_body(NESTED_BODY);
        assert_eq!(s.visible, 0);
        assert_eq!(s.suppressed, 2);
        assert_eq!(s.suppressed_comments.len(), 2);
    }

    #[test]
    fn nested_grammar_strips_invisible_formatting_from_paths() {
        // The host renders the path with zero-width break
        // opportunities between segments. A witness carrying them is
        // not a filesystem key and never matches a real file.
        let s = parse_copilot_review_body(NESTED_BODY);
        assert_eq!(
            s.suppressed_comments[0].path,
            "services/shared/src/money.ts"
        );
        assert_eq!(s.suppressed_comments[0].line, 531);
        assert_eq!(s.suppressed_comments[1].line, 949);
    }

    #[test]
    fn nested_grammar_entry_body_carries_the_finding_title() {
        // The finding's headline lives in the per-finding block's
        // summary; without it the witness reaches an agent as
        // rationale with no prescription.
        let s = parse_copilot_review_body(NESTED_BODY);
        assert_eq!(
            s.suppressed_comments[0].body,
            "Validate units as bigint before formatting\n\n\
             This exported escape hatch bypasses the `bigint` validation used for `Money`."
        );
    }

    #[test]
    fn nested_grammar_sums_severity_runs_on_the_findings_label() {
        // `1 <high> \u{b7} 1 <medium>` totals 2. Image geometry
        // (width=62, height=18) sits inside tags and must not reach
        // the digit scan.
        let s = parse_copilot_review_body(NESTED_LINKED_BODY);
        assert_eq!(s.visible, 2);
    }

    #[test]
    fn link_rendered_findings_are_not_counted_as_body_rendered() {
        // A section whose findings render as anchor links is a set of
        // inline review threads; the visible channel owns it.
        let s = parse_copilot_review_body(NESTED_LINKED_BODY);
        assert_eq!(s.suppressed, 0);
        assert!(s.suppressed_comments.is_empty());
    }

    #[test]
    fn stem_matched_summary_over_links_does_not_state_a_suppressed_count() {
        // Guard against double-counting: even a stem-matched section
        // title contributes nothing while its findings render as
        // links.
        let body = "<details>\n<summary><strong>Previously missed (2)</strong></summary>\n\n\
             - [First](#discussion_r1)\n- [Second](#discussion_r2)\n</details>";
        assert_eq!(parse_copilot_review_body(body).suppressed, 0);
    }

    #[test]
    fn flat_grammar_extracts_body_rendered_findings() {
        let s = parse_copilot_review_body(FLAT_BODY);
        assert_eq!(s.visible, 3);
        assert_eq!(s.suppressed, 2);
        assert_eq!(s.suppressed_comments.len(), 2);
        assert_eq!(
            s.suppressed_comments[0].path,
            "services/shared/src/asset-id.ts"
        );
        assert_eq!(s.suppressed_comments[0].line, 190);
    }

    #[test]
    fn entry_body_stops_at_the_blocks_own_scaffolding() {
        // The per-review totals follow the last entry inside the same
        // block. Appending them to that entry's body ships the
        // block's bookkeeping to an agent as if it were the finding.
        let s = parse_copilot_review_body(FLAT_BODY);
        assert_eq!(
            s.suppressed_comments[1].body,
            "Normalize the bigint operands before converting to `number`."
        );
    }

    #[test]
    fn unrelated_bold_labels_do_not_state_a_visible_count() {
        // `Files reviewed: 5/6` is a bold label line carrying digit
        // runs; only a findings label states the count.
        let body = "- **Files reviewed:** 5/6 changed files\n- **Review effort level:** Balanced";
        assert_eq!(parse_copilot_review_body(body).visible, 0);
    }

    #[test]
    fn parse_returns_zero_when_neither_pattern_present() {
        let s = parse_copilot_review_body("nothing matches here");
        assert_eq!(s.visible, 0);
        assert_eq!(s.suppressed, 0);
        assert!(s.suppressed_comments.is_empty());
    }

    #[test]
    fn parse_suppressed_count_old_wording() {
        // Wording observed before 2026-08-07. Bodies predating a host
        // rewording stay readable: a PR's round history spans them.
        let s = parse_copilot_review_body(
            "<details>\n\
             <summary>Comments suppressed due to low confidence (5)</summary>\n\
             </details>",
        );
        assert_eq!(s.suppressed, 5);
    }

    #[test]
    fn parse_suppressed_count_new_wording() {
        // Wording observed 2026-08-07: the host renamed the block
        // from "Comments suppressed due to low confidence (N)" to
        // "Suppressed comments (N)". The stem match covers both.
        let s = parse_copilot_review_body(
            "<details>\n<summary>Suppressed comments (8)</summary>\n</details>",
        );
        assert_eq!(s.suppressed, 8);
    }

    #[test]
    fn parse_suppressed_with_nested_paren_still_extracts_count() {
        // Regression: old find(suffix) returned at the inner `)`
        // and parsed "5 of 12" \u{2192} 0. Digit-run terminator avoids it.
        let s = parse_copilot_review_body(
            "<details>\n\
             <summary>Comments suppressed due to low confidence (5 of 12 hidden)</summary>\n\
             </details>",
        );
        assert_eq!(s.suppressed, 5);
    }

    #[test]
    fn parse_skips_digit_free_paren_before_count() {
        let s = parse_copilot_review_body(
            "<details>\n<summary>Suppressed comments (low confidence) (4)</summary>\n</details>",
        );
        assert_eq!(s.suppressed, 4);
    }

    #[test]
    fn suppressed_sections_sum_across_blocks() {
        // Distinct sections hold distinct findings; within one block
        // the summary and a heading restate a single count.
        let body = "<details>\n<summary>Suppressed comments (2)</summary>\n\n\
             ### Suppressed comments (2)\n</details>\n\
             <details>\n<summary>Previously missed (3)</summary>\n</details>";
        assert_eq!(parse_copilot_review_body(body).suppressed, 5);
    }

    #[test]
    fn suppressed_count_is_max_of_witnesses_and_stated() {
        // Stated exceeds parsed witnesses (one entry malformed):
        // the count keeps the host's claim, witnesses stay partial.
        let understated = "<details>\n<summary>Suppressed comments (3)</summary>\n\n\
             **src/a.rs:1**\n* real entry\n\n\
             **src/b.rs:bogus**\n* dropped header\n\n</details>";
        let s = parse_copilot_review_body(understated);
        assert_eq!(s.suppressed_comments.len(), 1);
        assert_eq!(s.suppressed, 3);

        // Witnesses exceed the stated count: witnesses win.
        let overstated = "<details>\n<summary>Suppressed comments (1)</summary>\n\n\
             **src/a.rs:1**\n* first\n\n\
             **src/b.rs:2**\n* second\n\n</details>";
        let s = parse_copilot_review_body(overstated);
        assert_eq!(s.suppressed_comments.len(), 2);
        assert_eq!(s.suppressed, 2);
    }

    #[test]
    fn suppressed_witnesses_survive_title_rewording() {
        // Structural channel: entries parse regardless of what the
        // host renames the block to — no lexical match required.
        let body = "<details>\n<summary>Findings withheld this round (2)</summary>\n\n\
             **src/a.rs:10**\n* first finding\n\n\
             **src/b.rs:20**\n* second finding\n\n</details>";
        let s = parse_copilot_review_body(body);
        assert_eq!(s.suppressed_comments.len(), 2);
        assert_eq!(s.suppressed, 2);
    }

    #[test]
    fn suppressed_count_survives_entry_grammar_drift() {
        // Lexical channel: when no entry parses, a stem-matched
        // summary still carries the count — degraded, not zero.
        let body = "<details>\n<summary>Suppressed comments (8)</summary>\n\n\
             - src/a.rs line 1: hypothetical future entry shape\n\n</details>";
        let s = parse_copilot_review_body(body);
        assert!(s.suppressed_comments.is_empty());
        assert_eq!(s.suppressed, 8);
    }

    #[test]
    fn foreign_details_block_is_not_a_false_positive() {
        // A details block with neither entry-shaped content nor a
        // stem-matched summary contributes nothing.
        let body = "<details>\n<summary>Files not reviewed (3)</summary>\n\n\
             * script.min.js\n* vendor/lib.rs\n* generated/schema.rs\n\n</details>";
        let s = parse_copilot_review_body(body);
        assert!(s.suppressed_comments.is_empty());
        assert_eq!(s.suppressed, 0);
    }

    // ── suppressed-entry extraction ──

    const SAMPLE_DETAILS: &str = "\
**Findings:** None

<details>
<summary>Comments suppressed due to low confidence (2)</summary>

**src/core/config/src/impls/consensus.rs:16**
* This doc comment still says \"workflow consensus protocol\".

**src/core/config/src/impls/consensus.rs:19**
* `ConsensusConfig` is now a public type in `example_config::impls::consensus`.

</details>
";

    #[test]
    fn extract_suppressed_returns_empty_when_no_block_present() {
        assert!(
            parse_copilot_review_body("**Findings:** 3")
                .suppressed_comments
                .is_empty()
        );
    }

    #[test]
    fn extract_suppressed_parses_two_entries_from_sample_block() {
        let entries = parse_copilot_review_body(SAMPLE_DETAILS).suppressed_comments;
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
        let body = "<details>\n\
                    <summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:bogus**\n* should be skipped\n\
                    **src/y.rs:7**\n* this one is kept\n</details>";
        let entries = parse_copilot_review_body(body).suppressed_comments;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/y.rs");
        assert_eq!(entries[0].line, 7);
    }

    #[test]
    fn extract_suppressed_skips_empty_bodies() {
        let body = "<details>\n\
                    <summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n\n\
                    **src/y.rs:2**\n* real body\n</details>";
        let entries = parse_copilot_review_body(body).suppressed_comments;
        assert_eq!(entries.len(), 1, "header-only entry must not survive");
        assert_eq!(entries[0].path, "src/y.rs");
    }

    #[test]
    fn extract_suppressed_concatenates_multi_line_body() {
        let body = "<details>\n\
                    <summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n\
                    * first line\n\
                    continuation\n</details>";
        let entries = parse_copilot_review_body(body).suppressed_comments;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "first line\ncontinuation");
    }

    #[test]
    fn extract_suppressed_stops_at_details_close() {
        let body = "<details>\n\
                    <summary>Comments suppressed due to low confidence (1)</summary>\n\n\
                    **src/x.rs:1**\n* before close\n\
                    </details>\n\
                    **src/y.rs:2**\n* outside block — must be ignored\n";
        let entries = parse_copilot_review_body(body).suppressed_comments;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/x.rs");
    }

    #[test]
    fn nested_blocks_do_not_leak_into_their_parents_content() {
        // A parent's content is the text it renders itself; each
        // nested element is flattened into its own block. Without the
        // elision a parent would re-parse its children's entries.
        let body = "<details>\n<summary>Outer (1)</summary>\n\n\
             intro prose\n\n\
             <details>\n<summary>Inner</summary>\n\n\
             **src/a.rs:1**\n* only finding\n</details>\n</details>";
        let entries = parse_copilot_review_body(body).suppressed_comments;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/a.rs");
        assert_eq!(entries[0].body, "Inner\n\nonly finding");
    }

    #[test]
    fn round_populated_with_suppressed_entries_from_review_body() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
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

    #[test]
    fn visible_count_survives_prose_drift_via_thread_linkage() {
        // The body's stated channel yields nothing (hypothetical
        // rewording of the findings label); the structural
        // channel counts threads whose first comment links back to
        // this review's numeric id.
        use crate::observe::github::review_threads::{
            CommentAuthor, CommentReviewRef, PageInfo, ReviewRequestsPage, ReviewThread,
            ReviewThreadsData, ReviewThreadsPage, ReviewThreadsPr, ReviewThreadsRepo,
            ThreadComment, ThreadComments,
        };
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let mut rev = copilot_review(HEAD_SHA, "2026-04-23T10:05:00Z", "reworded body");
        rev.id = Some(42);
        let linked_thread = |body: &str| ReviewThread {
            id: String::new(),
            is_resolved: false,
            is_outdated: false,
            path: "src/x.rs".into(),
            line: Some(1),
            comments: ThreadComments {
                page_info: PageInfo::default(),
                nodes: vec![ThreadComment {
                    database_id: None,
                    pull_request_review: Some(CommentReviewRef {
                        database_id: Some(42),
                    }),
                    author: Some(CommentAuthor {
                        login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
                    }),
                    created_at: ts("2026-04-23T10:05:00Z"),
                    body: body.into(),
                }],
            },
        };
        let threads = ReviewThreadsResponse {
            data: ReviewThreadsData {
                repository: ReviewThreadsRepo {
                    pull_request: ReviewThreadsPr {
                        review_threads: ReviewThreadsPage {
                            page_info: PageInfo {
                                has_next_page: false,
                                end_cursor: None,
                            },
                            nodes: vec![linked_thread("first"), linked_thread("second")],
                        },
                        review_requests: ReviewRequestsPage { nodes: vec![] },
                    },
                },
            },
        };
        let r = orient_copilot_test(enabled(), &events, &[rev], &threads, &empty_reqs(), &head())
            .unwrap();
        assert_eq!(r.rounds.len(), 1);
        assert_eq!(
            r.rounds[0].comments_visible, 2,
            "structural channel must carry when the stated channel parses nothing"
        );
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
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        };
        let events = vec![req_event(
            "2026-04-23T10:00:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
        let r =
            orient_copilot_test(enabled(), &events, &[], &empty_threads(), &reqs, &head()).unwrap();
        assert!(matches!(r.activity, CopilotActivity::Requested { .. }));
    }

    #[test]
    fn working_when_acked_but_no_review_yet() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        // Copilot must still be in requested_reviewers for Working
        // to fire — represents a live in-flight review.
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
            // Auto-review starts later, with no preceding request.
            ack_event("2026-04-23T11:00:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            NO_FINDINGS,
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
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let reviews = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            NO_FINDINGS,
        )];
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            NO_FINDINGS,
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
            NO_FINDINGS,
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![
            copilot_review(HEAD_SHA, "2026-04-23T10:05:00Z", "**Findings:** 1"),
            copilot_review(HEAD_SHA, "2026-04-23T10:10:00Z", NO_FINDINGS),
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
        let events = vec![req_event(
            "2026-04-23T10:00:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
        let revs = vec![
            copilot_review(HEAD_SHA, "2026-04-23T10:05:00Z", NO_FINDINGS),
            copilot_review(HEAD_SHA, "2026-04-23T10:10:00Z", NO_FINDINGS),
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
            "**Findings:** 1",
        )];
        let events = vec![
            req_event("2026-04-23T11:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T11:01:00Z"),
        ];
        let reqs = RequestedReviewers {
            users: vec![RequestedUser {
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            NO_FINDINGS,
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
    fn tier_gold_when_reviewed_at_head_but_comments_visible() {
        // Confirm-clean bar: a review at HEAD that found comments
        // stays Gold even after every thread has been resolved.
        // Platinum requires Copilot to have re-inspected the code
        // and reported zero comments — thread resolution alone is
        // human/agent judgment, not upstream confirmation.
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "**Findings:** 3",
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
        assert!(r.fresh, "fresh remains true — the round IS at HEAD");
        assert_eq!(r.rounds.last().unwrap().comments_visible, 3);
    }

    #[test]
    fn tier_gold_when_reviewed_at_non_head() {
        let events = vec![
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(OLD_SHA, "2026-04-23T10:05:00Z", NO_FINDINGS)];
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            "**Findings:** 2\n\
             <details>\n<summary>Suppressed comments (3)</summary>\n</details>",
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
        ];
        let revs = vec![copilot_review(
            HEAD_SHA,
            "2026-04-23T10:05:00Z",
            NO_FINDINGS,
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
                                        pull_request_review: None,
                                        database_id: None,
                                        author: Some(CommentAuthor {
                                            login: GitHubLogin::parse(
                                                "copilot-pull-request-reviewer[bot]",
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T10:01:00Z"),
            req_event("2026-04-23T11:00:00Z", "copilot-pull-request-reviewer[bot]"),
            ack_event("2026-04-23T11:01:00Z"),
        ];
        let revs = vec![
            copilot_review(OLD_SHA, "2026-04-23T10:05:00Z", "**Findings:** 1"),
            copilot_review(HEAD_SHA, "2026-04-23T11:05:00Z", NO_FINDINGS),
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
                login: GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
                user_type: UserType::Bot,
            }],
            teams: vec![],
        }
    }

    #[test]
    fn requested_healthy_within_start_window() {
        let events = vec![req_event(
            "2026-04-23T10:00:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
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
        let events = vec![req_event(
            "2026-04-23T10:00:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
            req_event("2026-04-23T10:20:00Z", "copilot-pull-request-reviewer[bot]"),
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
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
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
        let events = vec![req_event(
            "2026-04-23T09:15:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
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
        let events = vec![req_event(
            "2026-04-23T09:15:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
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
        let events = vec![req_event(
            "2026-04-23T09:15:00Z",
            "copilot-pull-request-reviewer[bot]",
        )];
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
        let req = req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]");
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
            req_event("2026-04-23T09:00:00Z", "copilot-pull-request-reviewer[bot]"),
            req_event("2026-04-23T10:00:00Z", "copilot-pull-request-reviewer[bot]"),
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
