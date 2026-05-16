//! Tier-grouped dashboard projection.
//!
//! Mission 1 / Phase A — assembles the on-disk surfaces (`next.md`,
//! `blockers.md`, `latest/decision.json`) that human callers consume
//! between OODA iterations. Phases B (`HandoffPrompt` preamble) and C
//! (PR status comment) reuse the same types.
//!
//! Name dance: the spec calls this struct `Decision`, but
//! [`ooda_core::Decision`] already owns that name (`Execute | Halt`,
//! the runner's executor signal). To avoid collision while keeping
//! the spec's semantics intact, the structured form here is
//! [`Dashboard`]; the executor signal stays [`ooda_core::Decision`].
//! The runner still consumes `decision.candidates.head().action`,
//! just spelled `dashboard.head_action()`.
//!
//! Anti-DRY mirror: this module is per-binary and copied byte-for-
//! byte across the three PR-side binaries — the same rule the
//! per-axis health enums follow. Lifting into [`ooda_core`] would
//! force the cross-binary spine to carry domain-shaped axis names.

use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt};
use crate::ids::BlockerKey;
use crate::orient::OrientedState;
use crate::orient::ci::ci_signal;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::copilot::copilot_signal;
use crate::orient::cursor::cursor_signal;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use ooda_core::{ActionKindName, NonEmpty, PromptSection, SingleLineString, Urgency};
use serde::Serialize;
use std::fmt::Write;

// ── Public types ─────────────────────────────────────────────────────

/// Tier-grouped snapshot the dashboard surfaces consume. Bundles the
/// ranked candidates (urgency order), per-axis health signals, and
/// the cross-axis blocker list pulled off each candidate.
///
/// The runner does not consume [`Dashboard`] — it still drives off
/// [`Decision`] for executor semantics. The `Dashboard` exists purely
/// for human-facing rendering surfaces.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Dashboard {
    /// Candidates in urgency order (already sorted by [`crate::decide::candidates`]).
    /// `None` iff the executor halt was [`DecisionHalt::Success`] or
    /// [`DecisionHalt::Terminal`] — no work to surface.
    pub candidates: Vec<RankedCandidate>,
    /// One entry per axis that elected to emit a signal this
    /// iteration. Axes with `Activity::Idle` (Copilot, CI) and
    /// `Activity::NotApplicable` (Cursor) project `None` and are
    /// skipped — see per-axis `*_signal` functions.
    pub signals: Vec<AxisSignal>,
    /// Deduplicated blocker list across all candidates, in first-
    /// encountered order. Carries the blocker tag plus the action
    /// kind that named it.
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RankedCandidate {
    /// Action variant name (payload-free) for stable rendering.
    pub action_name: &'static str,
    /// Human-readable log line from `effect.rendered_message()` —
    /// the same payload the per-iteration log uses.
    pub action_log: String,
    /// Effect debug form for the per-candidate detail line.
    pub effect_debug: String,
    /// Urgency tier — drives the tier-grouping in the renderer.
    pub urgency: Urgency,
    /// Stable blocker tag for this candidate.
    pub blocker: BlockerKey,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AxisSignal {
    pub axis: AxisName,
    pub icon: SignalIcon,
    pub summary: String,
}

/// Per-axis health icon. Five-bucket coarse projection — the spec
/// table at the top of the `IMPLEMENTATION_SPEC` fixes the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum SignalIcon {
    Ok,
    InFlight,
    Warn,
    Failed,
    NotApplicable,
}

impl SignalIcon {
    /// Single-char glyph for compact rendering. The renderer pairs
    /// this with the axis name and summary.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::InFlight => "·",
            Self::Warn => "!",
            Self::Failed => "✗",
            Self::NotApplicable => "—",
        }
    }
}

/// Axis identifier — drives the leading column of each signal line.
/// Limited to the three reviewer axes; new axes append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AxisName {
    Copilot,
    Ci,
    Cursor,
    PullRequestMetadata,
    DocReview,
    ClaudeReview,
}

impl AxisName {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::Ci => "ci",
            Self::Cursor => "cursor",
            Self::PullRequestMetadata => "pr_meta",
            Self::DocReview => "doc_review",
            Self::ClaudeReview => "claude_review",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Blocker {
    /// The stable `BlockerKey` from the candidate naming this blocker.
    pub tag: BlockerKey,
    /// Action variant name that named this blocker — gives a human
    /// reader the "context" half of the `tag: context` line in
    /// `blockers.md`.
    pub action_name: &'static str,
}

// ── Construction ─────────────────────────────────────────────────────

impl Dashboard {
    /// Assemble a [`Dashboard`] from the per-iteration triple
    /// `(OrientedState, candidates, decision)`. Mirrors the data
    /// already flowing through [`Recorder::record_iteration`] — no
    /// new observation source.
    pub(crate) fn from_iteration(
        oriented: &OrientedState,
        candidates: &[Action],
        decision: &Decision,
    ) -> Self {
        let candidates = build_candidates(candidates, decision);
        let signals = collect_signals(oriented);
        let blockers = collect_blockers(&candidates);
        Self {
            candidates,
            signals,
            blockers,
        }
    }
}

/// Halt arms with no actionable candidate (Success, Terminal) yield
/// an empty candidate list — there is nothing for the dashboard to
/// surface beyond the halt itself. The executor-side `Decision`
/// already captures that and the renderer falls through to the
/// "no action selected" path.
fn build_candidates(candidates: &[Action], decision: &Decision) -> Vec<RankedCandidate> {
    match decision {
        Decision::Halt(DecisionHalt::Success | DecisionHalt::Terminal(_)) => Vec::new(),
        Decision::Execute(_)
        | Decision::Halt(DecisionHalt::AgentNeeded(_) | DecisionHalt::HumanNeeded(_)) => candidates
            .iter()
            .map(RankedCandidate::from_action)
            .collect(),
    }
}

impl RankedCandidate {
    fn from_action(action: &Action) -> Self {
        Self {
            action_name: ActionKindName::name(&action.kind),
            action_log: action.rendered_payload(),
            effect_debug: format!("{:?}", action.effect),
            urgency: action.urgency,
            blocker: action.blocker.clone(),
        }
    }
}

fn collect_signals(oriented: &OrientedState) -> Vec<AxisSignal> {
    let mut out: Vec<AxisSignal> = Vec::new();
    if let Some(c) = &oriented.copilot
        && let Some(sig) = copilot_signal(&c.activity)
    {
        out.push(sig);
    }
    if let Some(sig) = ci_signal(&oriented.ci.activity) {
        out.push(sig);
    }
    if let Some(c) = &oriented.cursor
        && let Some(sig) = cursor_signal(&c.activity)
    {
        out.push(sig);
    }
    out.push(pull_request_metadata_signal(
        &oriented.pull_request_metadata,
    ));
    out.push(doc_review_signal(&oriented.doc_review));
    out.push(claude_review_signal(&oriented.claude_review));
    out
}

/// Project PR-meta state onto a dashboard signal. `Synced`
/// projects an `Ok` quiet positive; `Drift` warns with the commit
/// count; `NeverAttested` warns with a first-attestation prompt.
#[must_use]
pub(crate) fn pull_request_metadata_signal(state: &PullRequestMetadata) -> AxisSignal {
    let (icon, summary) = match state {
        PullRequestMetadata::Synced => (SignalIcon::Ok, "PR meta synced".to_string()),
        PullRequestMetadata::Drift {
            attested_sha,
            commits_behind,
            ..
        } => (
            SignalIcon::Warn,
            format!(
                "PR meta drifted {} since {}",
                crate::text::count(*commits_behind, "commit"),
                short_sha(attested_sha),
            ),
        ),
        PullRequestMetadata::NeverAttested => (
            SignalIcon::Warn,
            "PR meta never attested for this PR".to_string(),
        ),
    };
    AxisSignal {
        axis: AxisName::PullRequestMetadata,
        icon,
        summary,
    }
}

/// Project doc-review state onto a dashboard signal. Same shape as
/// `pull_request_metadata_signal`.
#[must_use]
pub(crate) fn doc_review_signal(state: &DocReview) -> AxisSignal {
    let (icon, summary) = match state {
        DocReview::Synced => (SignalIcon::Ok, "doc review synced".to_string()),
        DocReview::Drift {
            attested_sha,
            commits_behind,
            ..
        } => (
            SignalIcon::Warn,
            format!(
                "doc review drifted {} since {}",
                crate::text::count(*commits_behind, "commit"),
                short_sha(attested_sha),
            ),
        ),
        DocReview::NeverAttested => (
            SignalIcon::Warn,
            "doc review never attested for this PR".to_string(),
        ),
    };
    AxisSignal {
        axis: AxisName::DocReview,
        icon,
        summary,
    }
}

/// Project Claude-review state onto a dashboard signal. Three
/// projections: `NoActivity` collapses to `NotApplicable` (Claude has
/// not been requested on this PR — no review surface to grade);
/// `Addressed` is an `Ok` quiet positive; `Fresh` is a `Warn` with
/// the inline thread count.
#[must_use]
pub(crate) fn claude_review_signal(state: &ClaudeReview) -> AxisSignal {
    let (icon, summary) = match state {
        ClaudeReview::NoActivity => (
            SignalIcon::NotApplicable,
            "claude review not requested".to_string(),
        ),
        ClaudeReview::Addressed => (SignalIcon::Ok, "claude review addressed".to_string()),
        ClaudeReview::Fresh {
            inline_thread_count,
            ..
        } => (
            SignalIcon::Warn,
            format!(
                "claude review fresh ({})",
                crate::text::count(*inline_thread_count, "inline thread"),
            ),
        ),
    };
    AxisSignal {
        axis: AxisName::ClaudeReview,
        icon,
        summary,
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Deduplicate by `BlockerKey` while preserving first-seen order.
/// Two candidates naming the same blocker (e.g. two CI escalations
/// on the same `ci:failed` tag) collapse to one row.
fn collect_blockers(candidates: &[RankedCandidate]) -> Vec<Blocker> {
    let mut out: Vec<Blocker> = Vec::new();
    for c in candidates {
        if out.iter().any(|b| b.tag == c.blocker) {
            continue;
        }
        out.push(Blocker {
            tag: c.blocker.clone(),
            action_name: c.action_name,
        });
    }
    out
}

// ── Rendering ────────────────────────────────────────────────────────

impl Dashboard {
    /// Render `next.md`. Tier-grouped — winner with its `action_log`,
    /// then any same-tier alternatives, then a section per lower
    /// urgency tier. Empty sections are omitted.
    pub(crate) fn render_next_md(&self) -> String {
        let Some(winner) = self.candidates.first() else {
            return "# Next\n\nNo action selected.\n".to_string();
        };

        let mut out = String::new();
        out.push_str("# Next\n\n");
        writeln!(out, "## Recommended ({})", urgency_label(winner.urgency))
            .expect("write to String");
        writeln!(out, "{}: {}\n", winner.action_name, winner.action_log).expect("write to String");
        writeln!(out, "- effect: `{}`", winner.effect_debug).expect("write to String");
        writeln!(out, "- blocker: `{}`", winner.blocker).expect("write to String");

        let mut by_tier = tiers(&self.candidates);
        // Drop the winner from its own tier — same urgency, but the
        // first entry is already rendered above. Skip the bucket
        // entirely if the winner was alone in it.
        if let Some(bucket) = by_tier.first_mut() {
            bucket.candidates.remove(0);
        }
        // Render same-tier alternatives (if any survived the drop).
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            out.push_str("\n## Also at this tier\n");
            for c in &top.candidates {
                writeln!(out, "- {}: {}", c.action_name, c.action_log).expect("write to String");
            }
        }

        // Render lower tiers — every bucket past the first.
        let lower: Vec<&TierBucket> = by_tier.iter().skip(1).collect();
        if !lower.is_empty() {
            out.push_str("\n## Queued (lower urgency)\n");
            for bucket in lower {
                writeln!(out, "\n### {}", urgency_label(bucket.urgency)).expect("write to String");
                for c in &bucket.candidates {
                    writeln!(out, "- {}: {}", c.action_name, c.action_log)
                        .expect("write to String");
                }
            }
        }

        if !self.signals.is_empty() {
            out.push_str("\n## Signals\n");
            for sig in &self.signals {
                writeln!(
                    out,
                    "- {}: {} {}",
                    sig.axis.as_str(),
                    sig.icon.glyph(),
                    sig.summary,
                )
                .expect("write to String");
            }
        }

        if !self.blockers.is_empty() {
            out.push_str("\n## Blockers\n");
            for b in &self.blockers {
                writeln!(out, "- `{}`: {}", b.tag, b.action_name).expect("write to String");
            }
        }

        out
    }

    /// Render the PR status comment body. Tier-grouped — recommended
    /// winner on top, then same-tier alternatives, then lower-urgency
    /// tiers grouped one section per tier, then per-axis signals, then
    /// the cross-axis blocker list. Empty sections are omitted.
    ///
    /// Tuned for the GitHub PR comment view: compact at the top
    /// (winner front and centre), bullet-list dense below, no
    /// `effect` debug line (that's a `next.md` reader's concern).
    /// The header (`## OODA · {repo}#{pr} — iteration N`) is added by
    /// the caller that knows slug/pr/iter; this method renders the
    /// body below the header. Returns the empty string for an empty
    /// candidate list — the caller decides what to substitute (e.g.
    /// a terminal-halt summary line).
    pub(crate) fn render_status_comment(&self) -> String {
        let Some(winner) = self.candidates.first() else {
            return String::new();
        };

        let mut out = String::new();
        writeln!(
            out,
            "**Recommended ({}):** {}: {}",
            urgency_label(winner.urgency),
            winner.action_name,
            winner.action_log,
        )
        .expect("write to String");

        let mut by_tier = tiers(&self.candidates);
        if let Some(bucket) = by_tier.first_mut() {
            bucket.candidates.remove(0);
        }
        // Same-tier alternatives — listed inline under the winner;
        // tier label is implicit (same as the winner's).
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            out.push_str("\n**Also at this tier:**\n");
            for c in &top.candidates {
                writeln!(out, "- {}: {}", c.action_name, c.action_log).expect("write to String");
            }
        }

        // Lower-tier queued candidates — one bullet per candidate,
        // tier label italicised inline so the comment stays a single
        // bulleted block rather than a stack of `###` subheadings.
        let lower: Vec<&TierBucket> = by_tier.iter().skip(1).collect();
        if !lower.is_empty() {
            out.push_str("\n**Queued (lower urgency):**\n");
            for bucket in lower {
                let label = urgency_label(bucket.urgency);
                for c in &bucket.candidates {
                    writeln!(out, "- _{label}_ — {}: {}", c.action_name, c.action_log)
                        .expect("write to String");
                }
            }
        }

        if !self.signals.is_empty() {
            out.push_str("\n**Signals:**\n");
            for sig in &self.signals {
                writeln!(
                    out,
                    "- {}: {} {}",
                    sig.axis.as_str(),
                    sig.icon.glyph(),
                    sig.summary,
                )
                .expect("write to String");
            }
        }

        if !self.blockers.is_empty() {
            out.push_str("\n**Blockers:**\n");
            for b in &self.blockers {
                writeln!(out, "- `{}` — {}", b.tag, b.action_name).expect("write to String");
            }
        }

        out
    }

    /// Render `blockers.md` — structured blocker list on its own.
    pub(crate) fn render_blockers_md(&self) -> String {
        if self.blockers.is_empty() {
            return "# Blockers\n\nNo current blocker.\n".to_string();
        }
        let mut out = String::from("# Blockers\n\n");
        for b in &self.blockers {
            writeln!(out, "- `{}`: {}", b.tag, b.action_name).expect("write to String");
        }
        out
    }

    /// Project the dashboard as a sequence of [`PromptSection`]s
    /// suitable for prepending to an existing handoff prompt body.
    /// The Phase-B preamble — universal across `HandoffAgent` and
    /// `HandoffHuman` handoffs, layered on top of any per-action
    /// context the existing decorator (5bf9c7c) appends.
    ///
    /// Section order mirrors `next.md`: recommended winner →
    /// queued lower-tier groups → signals → blockers. Empty
    /// sections are omitted — the same rule the on-disk surfaces
    /// follow.
    pub(crate) fn render_handoff_preamble(&self) -> Vec<PromptSection> {
        let mut sections: Vec<PromptSection> = Vec::new();

        let Some(winner) = self.candidates.first() else {
            return sections;
        };

        sections.push(PromptSection::Paragraph(format!(
            "Recommended ({}): {}: {} [blocker: {}]",
            urgency_label(winner.urgency),
            winner.action_name,
            winner.action_log,
            winner.blocker,
        )));

        let mut by_tier = tiers(&self.candidates);
        if let Some(bucket) = by_tier.first_mut() {
            bucket.candidates.remove(0);
        }
        // Same-tier alternatives — header paragraph then a numbered
        // list of candidates. Two-section pairing keeps each
        // candidate at a meaningful index ("1.", "2." …) without
        // the header chewing position 1.
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            sections.push(PromptSection::Paragraph(format!(
                "Also at this tier ({}):",
                urgency_label(top.urgency),
            )));
            push_candidate_list(&mut sections, &top.candidates);
        }

        // Queued lower tiers — one header paragraph + numbered list
        // pair per tier. The tier label is the only thing that
        // varies between buckets.
        for bucket in by_tier.iter().skip(1) {
            sections.push(PromptSection::Paragraph(format!(
                "Queued ({}):",
                urgency_label(bucket.urgency),
            )));
            push_candidate_list(&mut sections, &bucket.candidates);
        }

        // Signals — header paragraph then numbered list of axis
        // entries (axis name + icon glyph + summary).
        if !self.signals.is_empty() {
            sections.push(PromptSection::Paragraph("Signals:".to_string()));
            let items: Vec<SingleLineString> = self
                .signals
                .iter()
                .map(|s| {
                    SingleLineString::new(format!(
                        "{}: {} {}",
                        s.axis.as_str(),
                        s.icon.glyph(),
                        s.summary,
                    ))
                })
                .collect();
            if let Some(list) = NonEmpty::try_from_vec(items) {
                sections.push(PromptSection::NumberedList(list));
            }
        }

        // Blockers — header paragraph then numbered list of `tag:
        // action` lines, deduplicated upstream by `collect_blockers`.
        if !self.blockers.is_empty() {
            sections.push(PromptSection::Paragraph("Blockers:".to_string()));
            let items: Vec<SingleLineString> = self
                .blockers
                .iter()
                .map(|b| SingleLineString::new(format!("{}: {}", b.tag, b.action_name)))
                .collect();
            if let Some(list) = NonEmpty::try_from_vec(items) {
                sections.push(PromptSection::NumberedList(list));
            }
        }

        sections
    }
}

/// Push a [`PromptSection::NumberedList`] of `<action>: <log>`
/// lines for each candidate. No-op when the slice is empty (the
/// caller already gated on emptiness for the section header).
fn push_candidate_list(sections: &mut Vec<PromptSection>, candidates: &[&RankedCandidate]) {
    let items: Vec<SingleLineString> = candidates
        .iter()
        .map(|c| SingleLineString::new(format!("{}: {}", c.action_name, c.action_log)))
        .collect();
    if let Some(list) = NonEmpty::try_from_vec(items) {
        sections.push(PromptSection::NumberedList(list));
    }
}

/// Tier bucket — one urgency level with its candidates in input
/// order (axis order, since `candidates()` does a stable sort by
/// `urgency`). Used by [`Dashboard::render_next_md`] to walk the
/// tier-grouped output.
struct TierBucket<'a> {
    urgency: Urgency,
    candidates: Vec<&'a RankedCandidate>,
}

fn tiers(candidates: &[RankedCandidate]) -> Vec<TierBucket<'_>> {
    let mut out: Vec<TierBucket<'_>> = Vec::new();
    for c in candidates {
        match out.last_mut() {
            Some(bucket) if bucket.urgency == c.urgency => bucket.candidates.push(c),
            _ => out.push(TierBucket {
                urgency: c.urgency,
                candidates: vec![c],
            }),
        }
    }
    out
}

/// Stable human-readable label for an [`Urgency`] tier. Lowercase,
/// space-separated — distinct from the variant names so the rendered
/// surface stays readable without coupling to the Rust enum casing.
fn urgency_label(u: Urgency) -> &'static str {
    match u {
        Urgency::Critical => "critical",
        Urgency::BlockingFix => "blocking fix",
        Urgency::BlockingWait => "blocking wait",
        Urgency::BlockingHuman => "blocking human",
        Urgency::Advancing => "advancing",
        Urgency::Hygiene => "hygiene",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect};
    use crate::orient::copilot::CopilotActivity;
    use crate::orient::cursor::CursorActivity;
    use ooda_core::HandoffPrompt;

    fn act(kind: ActionKind, urgency: Urgency, blocker: &str, log: &str) -> Action {
        Action {
            kind,
            effect: ActionEffect::Full { log: log.into() },
            target_effect: TargetEffect::Blocks,
            urgency,
            blocker: BlockerKey::tag(blocker),
        }
    }

    fn dashboard_from_candidates(cs: &[Action]) -> Dashboard {
        let candidates: Vec<RankedCandidate> =
            cs.iter().map(RankedCandidate::from_action).collect();
        let blockers = collect_blockers(&candidates);
        Dashboard {
            candidates,
            signals: Vec::new(),
            blockers,
        }
    }

    fn rerequest_copilot() -> Action {
        act(
            ActionKind::RerequestCopilot { symptom: None },
            Urgency::Critical,
            "copilot:rerequest",
            "rerequest copilot",
        )
    }

    fn wait_for_ci() -> Action {
        let pending =
            ooda_core::NonEmpty::singleton(crate::ids::CheckName::parse("ci/build").unwrap());
        act(
            ActionKind::WaitForCi { pending },
            Urgency::BlockingWait,
            "ci:wait",
            "wait for ci/build",
        )
    }

    fn request_approval() -> Action {
        Action {
            kind: ActionKind::RequestApproval,
            effect: ActionEffect::Human {
                prompt: HandoffPrompt::new("approve"),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag("review:approval"),
        }
    }

    // ── Snapshot tests for next.md rendering ─────────────────────

    #[test]
    fn next_md_single_winner_alone_in_tier() {
        let d = dashboard_from_candidates(&[rerequest_copilot()]);
        let md = d.render_next_md();
        assert!(md.starts_with("# Next\n"), "{md}");
        assert!(md.contains("## Recommended (critical)"), "{md}");
        assert!(md.contains("RerequestCopilot: rerequest copilot"), "{md}");
        assert!(md.contains("- effect: `Full"), "{md}");
        assert!(md.contains("- blocker: `copilot:rerequest`"), "{md}");
        assert!(!md.contains("## Also at this tier"), "{md}");
        assert!(!md.contains("## Queued"), "{md}");
        assert!(!md.contains("## Signals"), "{md}");
        assert!(md.contains("## Blockers"), "{md}");
    }

    #[test]
    fn next_md_winner_with_same_tier_alternative() {
        let other = act(
            ActionKind::Rebase,
            Urgency::Critical,
            "state:rebase",
            "rebase onto base",
        );
        let d = dashboard_from_candidates(&[rerequest_copilot(), other]);
        let md = d.render_next_md();
        assert!(md.contains("## Also at this tier"), "{md}");
        assert!(md.contains("- Rebase: rebase onto base"), "{md}");
        assert!(!md.contains("## Queued"), "{md}");
    }

    #[test]
    fn next_md_winner_with_lower_tier_candidate() {
        let d = dashboard_from_candidates(&[rerequest_copilot(), wait_for_ci()]);
        let md = d.render_next_md();
        assert!(md.contains("## Recommended (critical)"), "{md}");
        assert!(!md.contains("## Also at this tier"), "{md}");
        assert!(md.contains("## Queued (lower urgency)"), "{md}");
        assert!(md.contains("### blocking wait"), "{md}");
        assert!(md.contains("- WaitForCi: wait for ci/build"), "{md}");
    }

    #[test]
    fn next_md_all_sections_present() {
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::InFlight,
                summary: "reviewing".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::Ok,
                summary: "all required checks passing".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let md = d.render_next_md();
        assert!(md.contains("## Recommended (critical)"), "{md}");
        assert!(md.contains("## Queued (lower urgency)"), "{md}");
        assert!(md.contains("### blocking wait"), "{md}");
        assert!(md.contains("### blocking human"), "{md}");
        assert!(md.contains("## Signals"), "{md}");
        assert!(md.contains("- copilot: · reviewing"), "{md}");
        assert!(md.contains("- ci: ✓ all required checks passing"), "{md}");
        assert!(md.contains("## Blockers"), "{md}");
        assert!(md.contains("`copilot:rerequest`"), "{md}");
        assert!(md.contains("`ci:wait`"), "{md}");
        assert!(md.contains("`review:approval`"), "{md}");
    }

    #[test]
    fn next_md_empty_optional_sections_omitted() {
        // Empty candidate set → "No action selected" — no signals,
        // no blockers, no recommended block.
        let d = Dashboard {
            candidates: Vec::new(),
            signals: Vec::new(),
            blockers: Vec::new(),
        };
        let md = d.render_next_md();
        assert_eq!(md, "# Next\n\nNo action selected.\n");
    }

    #[test]
    fn blockers_md_lists_each_blocker() {
        let d = dashboard_from_candidates(&[rerequest_copilot(), wait_for_ci()]);
        let md = d.render_blockers_md();
        assert!(md.starts_with("# Blockers\n"), "{md}");
        assert!(md.contains("`copilot:rerequest`: RerequestCopilot"), "{md}");
        assert!(md.contains("`ci:wait`: WaitForCi"), "{md}");
    }

    #[test]
    fn blockers_md_empty_message_when_no_blockers() {
        let d = Dashboard {
            candidates: Vec::new(),
            signals: Vec::new(),
            blockers: Vec::new(),
        };
        assert_eq!(
            d.render_blockers_md(),
            "# Blockers\n\nNo current blocker.\n"
        );
    }

    #[test]
    fn blockers_dedup_preserves_first_seen_order() {
        let dup = act(
            ActionKind::RerequestCopilot { symptom: None },
            Urgency::Critical,
            "copilot:rerequest",
            "rerequest copilot again",
        );
        let d = dashboard_from_candidates(&[rerequest_copilot(), dup, wait_for_ci()]);
        assert_eq!(d.blockers.len(), 2);
        assert_eq!(d.blockers[0].tag.as_str(), "copilot:rerequest");
        assert_eq!(d.blockers[1].tag.as_str(), "ci:wait");
    }

    #[test]
    fn next_md_three_candidate_scenario_demo() {
        // Prints a representative 3-candidate dashboard. Behind
        // `--nocapture` this is the visual-verification path; the
        // assertion is the contract.
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::Warn,
                summary: "request received but no review yet (degraded)".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::InFlight,
                summary: "2 checks pending (worst: healthy)".into(),
            },
            AxisSignal {
                axis: AxisName::Cursor,
                icon: SignalIcon::Ok,
                summary: "no findings".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let md = d.render_next_md();
        eprintln!("---- next.md sample ----\n{md}---- end ----");
        assert!(md.contains("## Recommended (critical)"));
    }

    // ── Snapshot tests for status comment rendering ──────────────

    #[test]
    fn status_comment_empty_when_no_candidates() {
        let d = Dashboard {
            candidates: Vec::new(),
            signals: Vec::new(),
            blockers: Vec::new(),
        };
        assert_eq!(d.render_status_comment(), "");
    }

    #[test]
    fn status_comment_single_winner_alone_in_tier() {
        let d = dashboard_from_candidates(&[rerequest_copilot()]);
        let body = d.render_status_comment();
        assert!(
            body.starts_with("**Recommended (critical):** RerequestCopilot: rerequest copilot\n"),
            "{body}",
        );
        assert!(!body.contains("**Also at this tier:**"), "{body}");
        assert!(!body.contains("**Queued"), "{body}");
        assert!(!body.contains("**Signals:**"), "{body}");
        // Single candidate still yields a Blockers section — the
        // winner's blocker is always surfaced for the reader.
        assert!(body.contains("**Blockers:**"), "{body}");
        assert!(
            body.contains("- `copilot:rerequest` — RerequestCopilot"),
            "{body}",
        );
    }

    #[test]
    fn status_comment_winner_with_same_tier_alternative() {
        let other = act(
            ActionKind::Rebase,
            Urgency::Critical,
            "state:rebase",
            "rebase onto base",
        );
        let d = dashboard_from_candidates(&[rerequest_copilot(), other]);
        let body = d.render_status_comment();
        assert!(body.contains("**Recommended (critical):**"), "{body}");
        assert!(body.contains("**Also at this tier:**"), "{body}");
        assert!(body.contains("- Rebase: rebase onto base\n"), "{body}");
        assert!(!body.contains("**Queued"), "{body}");
    }

    #[test]
    fn status_comment_winner_with_lower_tier_candidate() {
        let d = dashboard_from_candidates(&[rerequest_copilot(), wait_for_ci()]);
        let body = d.render_status_comment();
        assert!(body.contains("**Recommended (critical):**"), "{body}");
        assert!(!body.contains("**Also at this tier:**"), "{body}");
        assert!(body.contains("**Queued (lower urgency):**"), "{body}");
        assert!(
            body.contains("- _blocking wait_ — WaitForCi: wait for ci/build\n"),
            "{body}",
        );
    }

    #[test]
    fn status_comment_all_sections_populated() {
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::Warn,
                summary: "request received but no review yet (degraded)".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::InFlight,
                summary: "2 checks pending (worst: healthy)".into(),
            },
            AxisSignal {
                axis: AxisName::Cursor,
                icon: SignalIcon::Ok,
                summary: "no findings".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let body = d.render_status_comment();
        assert!(body.contains("**Recommended (critical):**"), "{body}");
        assert!(body.contains("**Queued (lower urgency):**"), "{body}");
        assert!(
            body.contains("- _blocking wait_ — WaitForCi: wait for ci/build"),
            "{body}",
        );
        assert!(
            body.contains("- _blocking human_ — RequestApproval"),
            "{body}",
        );
        assert!(body.contains("**Signals:**"), "{body}");
        assert!(
            body.contains("- copilot: ! request received but no review yet (degraded)"),
            "{body}",
        );
        assert!(
            body.contains("- ci: · 2 checks pending (worst: healthy)"),
            "{body}",
        );
        assert!(body.contains("- cursor: ✓ no findings"), "{body}");
        assert!(body.contains("**Blockers:**"), "{body}");
        assert!(
            body.contains("- `copilot:rerequest` — RerequestCopilot"),
            "{body}",
        );
        assert!(body.contains("- `ci:wait` — WaitForCi"), "{body}");
        assert!(
            body.contains("- `review:approval` — RequestApproval"),
            "{body}",
        );
    }

    #[test]
    fn status_comment_demo_print() {
        // Visual-verification path under --nocapture; pins the
        // headline as the contract.
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::Warn,
                summary: "request received but no review yet (degraded)".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::InFlight,
                summary: "2 checks pending (worst: healthy)".into(),
            },
            AxisSignal {
                axis: AxisName::Cursor,
                icon: SignalIcon::Ok,
                summary: "no findings".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let body = d.render_status_comment();
        eprintln!("---- status comment sample ----\n{body}---- end ----");
        assert!(body.contains("**Recommended (critical):**"));
    }

    // ── Snapshot tests for handoff preamble rendering ────────────

    fn preamble_text(d: &Dashboard) -> String {
        // Render via HandoffPrompt so the preamble flows through
        // the same Display path the binary ships at runtime — any
        // section-formatter drift surfaces here.
        let mut p = HandoffPrompt::new("HEAD");
        for s in d.render_handoff_preamble() {
            p.sections.push(s);
        }
        p.to_string()
    }

    #[test]
    fn preamble_empty_when_no_candidates() {
        let d = Dashboard {
            candidates: Vec::new(),
            signals: Vec::new(),
            blockers: Vec::new(),
        };
        assert!(d.render_handoff_preamble().is_empty());
    }

    #[test]
    fn preamble_single_winner_alone() {
        let d = dashboard_from_candidates(&[rerequest_copilot()]);
        let text = preamble_text(&d);
        assert!(
            text.contains("Recommended (critical): RerequestCopilot: rerequest copilot"),
            "{text}",
        );
        assert!(text.contains("[blocker: copilot:rerequest]"), "{text}");
        assert!(!text.contains("Also at this tier"), "{text}");
        assert!(!text.contains("Queued"), "{text}");
        assert!(!text.contains("Signals"), "{text}");
        assert!(text.contains("Blockers"), "{text}");
        assert!(
            text.contains("copilot:rerequest: RerequestCopilot"),
            "{text}",
        );
    }

    #[test]
    fn preamble_winner_with_same_tier_alternative() {
        let other = act(
            ActionKind::Rebase,
            Urgency::Critical,
            "state:rebase",
            "rebase onto base",
        );
        let d = dashboard_from_candidates(&[rerequest_copilot(), other]);
        let text = preamble_text(&d);
        assert!(text.contains("Recommended (critical)"), "{text}");
        assert!(text.contains("Also at this tier (critical)"), "{text}");
        assert!(text.contains("Rebase: rebase onto base"), "{text}");
        assert!(!text.contains("Queued"), "{text}");
    }

    #[test]
    fn preamble_winner_with_lower_tier_candidate() {
        let d = dashboard_from_candidates(&[rerequest_copilot(), wait_for_ci()]);
        let text = preamble_text(&d);
        assert!(text.contains("Recommended (critical)"), "{text}");
        assert!(!text.contains("Also at this tier"), "{text}");
        assert!(text.contains("Queued (blocking wait)"), "{text}");
        assert!(text.contains("WaitForCi: wait for ci/build"), "{text}");
    }

    #[test]
    fn preamble_demo_print_full_dashboard() {
        // Visual-verification path — prints the preamble for a
        // representative 3-candidate / 3-signal / 3-blocker state
        // behind --nocapture. The assertion just pins the headline
        // so the test stays meaningful in CI.
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::Warn,
                summary: "request received but no review yet (degraded)".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::InFlight,
                summary: "2 checks pending (worst: healthy)".into(),
            },
            AxisSignal {
                axis: AxisName::Cursor,
                icon: SignalIcon::Ok,
                summary: "no findings".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let mut p = HandoffPrompt::new("Rebase onto the latest base branch");
        for s in d.render_handoff_preamble() {
            p.sections.push(s);
        }
        eprintln!("---- preamble sample ----\n{p}\n---- end ----");
        assert!(p.to_string().contains("Recommended (critical)"));
    }

    #[test]
    fn preamble_all_signals_and_blockers_populated() {
        let signals = vec![
            AxisSignal {
                axis: AxisName::Copilot,
                icon: SignalIcon::Warn,
                summary: "request received but no review yet (degraded)".into(),
            },
            AxisSignal {
                axis: AxisName::Ci,
                icon: SignalIcon::InFlight,
                summary: "2 checks pending (worst: healthy)".into(),
            },
            AxisSignal {
                axis: AxisName::Cursor,
                icon: SignalIcon::Ok,
                summary: "no findings".into(),
            },
        ];
        let candidates_v: Vec<RankedCandidate> =
            [rerequest_copilot(), wait_for_ci(), request_approval()]
                .iter()
                .map(RankedCandidate::from_action)
                .collect();
        let blockers = collect_blockers(&candidates_v);
        let d = Dashboard {
            candidates: candidates_v,
            signals,
            blockers,
        };
        let text = preamble_text(&d);
        assert!(text.contains("Recommended (critical)"), "{text}");
        assert!(text.contains("Queued (blocking wait)"), "{text}");
        assert!(text.contains("Queued (blocking human)"), "{text}");
        assert!(text.contains("Signals"), "{text}");
        assert!(
            text.contains("copilot: ! request received but no review yet (degraded)"),
            "{text}",
        );
        assert!(
            text.contains("ci: · 2 checks pending (worst: healthy)"),
            "{text}",
        );
        assert!(text.contains("cursor: ✓ no findings"), "{text}");
        assert!(text.contains("Blockers"), "{text}");
        assert!(
            text.contains("copilot:rerequest: RerequestCopilot"),
            "{text}",
        );
        assert!(text.contains("ci:wait: WaitForCi"), "{text}");
        assert!(text.contains("review:approval: RequestApproval"), "{text}");
    }

    #[test]
    fn build_candidates_drops_on_success_halt() {
        let decision = Decision::Halt(DecisionHalt::Success);
        let candidates = build_candidates(&[rerequest_copilot()], &decision);
        assert!(candidates.is_empty());
    }

    #[test]
    fn build_candidates_drops_on_terminal_halt() {
        let decision = Decision::Halt(DecisionHalt::Terminal(
            crate::decide::decision::Terminal::Succeeded,
        ));
        let candidates = build_candidates(&[rerequest_copilot()], &decision);
        assert!(candidates.is_empty());
    }

    #[test]
    fn build_candidates_keeps_on_handoff_halt() {
        let handoff = ooda_core::HandoffAction {
            kind: ActionKind::RequestApproval,
            prompt: HandoffPrompt::new("approve"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag("review:approval"),
        };
        let decision = Decision::Halt(DecisionHalt::HumanNeeded(handoff));
        let candidates = build_candidates(&[request_approval()], &decision);
        assert_eq!(candidates.len(), 1);
    }

    // ── Signal projection exhaustive enumeration ──────────────────

    /// Walk every (`CopilotActivity`, `CiActivity`, `CursorActivity`)
    /// combination that the per-axis signal projections classify, and
    /// assert each yields the spec-table icon. Adding a variant to
    /// any activity enum breaks compilation in the matches below.
    #[test]
    fn signal_projection_table_matches_spec() {
        use crate::ids::{CheckName, GitCommitSha, Timestamp};
        use crate::observe::github::workflow_runs::WorkflowRunId;
        use crate::orient::ci::{CheckHealth, CiActivity, PendingCheck, ResolvedState};
        use crate::orient::copilot::{CopilotReviewRound, InFlightHealth as CopilotHealth};
        use crate::orient::cursor::{
            InFlightHealth as CursorHealth, ReviewedState as CursorReviewedState, SkipReason,
        };

        let ts = Timestamp::parse("2024-01-01T00:00:00Z").unwrap();
        let sha = GitCommitSha::parse(&"a".repeat(40)).unwrap();
        let round = CopilotReviewRound {
            round: 1,
            requested_at: ts,
            ack_at: None,
            reviewed_at: None,
            commit: Some(sha),
            comments_visible: 0,
            comments_suppressed: 0,
        };

        // Copilot — Idle omits; every other variant emits.
        assert!(copilot_signal(&CopilotActivity::Idle).is_none());
        for (h, icon) in [
            (CopilotHealth::Healthy, SignalIcon::InFlight),
            (CopilotHealth::Degraded, SignalIcon::Warn),
            (CopilotHealth::Failed, SignalIcon::Failed),
        ] {
            let sig = copilot_signal(&CopilotActivity::Requested {
                requested_at: ts,
                health: h,
            })
            .expect("Requested emits a signal");
            assert_eq!(sig.icon, icon, "Requested({h:?})");
            let sig = copilot_signal(&CopilotActivity::Working {
                requested_at: ts,
                ack_at: ts,
                health: h,
            })
            .expect("Working emits a signal");
            assert_eq!(sig.icon, icon, "Working({h:?})");
        }
        let sig = copilot_signal(&CopilotActivity::Reviewed { latest: round })
            .expect("Reviewed emits a signal");
        assert_eq!(sig.icon, SignalIcon::Ok);

        // CI — Idle omits; InFlight + every ResolvedState emits.
        assert!(ci_signal(&CiActivity::Idle).is_none());
        let pending = PendingCheck {
            name: CheckName::parse("ci/build").unwrap(),
            run_id: WorkflowRunId(1),
            health: CheckHealth::Healthy,
        };
        let sig = ci_signal(&CiActivity::InFlight(vec![pending])).expect("InFlight emits");
        assert_eq!(sig.icon, SignalIcon::InFlight);
        let sig = ci_signal(&CiActivity::Resolved(ResolvedState::AllGreen))
            .expect("Resolved AllGreen emits");
        assert_eq!(sig.icon, SignalIcon::Ok);
        let sig = ci_signal(&CiActivity::Resolved(ResolvedState::HasFailures(vec![
            CheckName::parse("ci/build").unwrap(),
        ])))
        .expect("Resolved HasFailures emits");
        assert_eq!(sig.icon, SignalIcon::Failed);
        let sig = ci_signal(&CiActivity::Resolved(ResolvedState::MissingRequired(vec![
            CheckName::parse("ci/build").unwrap(),
        ])))
        .expect("Resolved MissingRequired emits");
        assert_eq!(sig.icon, SignalIcon::Warn);

        // Cursor — NotApplicable omits; every other variant emits.
        assert!(cursor_signal(&CursorActivity::NotApplicable).is_none());
        let sig = cursor_signal(&CursorActivity::Skipped(SkipReason::AuthorClass))
            .expect("Skipped emits");
        assert_eq!(sig.icon, SignalIcon::NotApplicable);
        let sig = cursor_signal(&CursorActivity::InFlight(CursorHealth::Healthy))
            .expect("InFlight Healthy emits");
        assert_eq!(sig.icon, SignalIcon::InFlight);
        let sig = cursor_signal(&CursorActivity::InFlight(CursorHealth::Failed))
            .expect("InFlight Failed emits");
        assert_eq!(sig.icon, SignalIcon::Failed);
        let sig = cursor_signal(&CursorActivity::Reviewed(CursorReviewedState::Clean))
            .expect("Reviewed Clean emits");
        assert_eq!(sig.icon, SignalIcon::Ok);
        let sig = cursor_signal(&CursorActivity::Reviewed(CursorReviewedState::HasFindings))
            .expect("Reviewed HasFindings emits");
        assert_eq!(sig.icon, SignalIcon::Warn);
    }

    // ── PullRequestMetadata signal projection ─────────────────────────────────

    #[test]
    fn pull_request_metadata_signal_synced_renders_ok() {
        let sig = pull_request_metadata_signal(&PullRequestMetadata::Synced);
        assert_eq!(sig.icon, SignalIcon::Ok);
        assert_eq!(sig.axis, AxisName::PullRequestMetadata);
        assert!(sig.summary.contains("synced"), "{}", sig.summary);
    }

    #[test]
    fn pull_request_metadata_signal_drift_renders_warn_with_count_and_short_sha() {
        let sig = pull_request_metadata_signal(&PullRequestMetadata::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: 4,
        });
        assert_eq!(sig.icon, SignalIcon::Warn);
        assert!(sig.summary.contains("4 commits"), "{}", sig.summary);
        assert!(sig.summary.contains("abcdef1"), "{}", sig.summary);
    }

    #[test]
    fn pull_request_metadata_signal_never_attested_renders_warn() {
        let sig = pull_request_metadata_signal(&PullRequestMetadata::NeverAttested);
        assert_eq!(sig.icon, SignalIcon::Warn);
        assert!(sig.summary.contains("never attested"), "{}", sig.summary);
    }

    // ── DocReview signal projection ─────────────────────────────────

    #[test]
    fn doc_review_signal_synced_renders_ok() {
        let sig = doc_review_signal(&DocReview::Synced);
        assert_eq!(sig.icon, SignalIcon::Ok);
        assert_eq!(sig.axis, AxisName::DocReview);
        assert!(sig.summary.contains("synced"), "{}", sig.summary);
    }

    #[test]
    fn doc_review_signal_drift_renders_warn_with_count_and_short_sha() {
        let sig = doc_review_signal(&DocReview::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: 4,
        });
        assert_eq!(sig.icon, SignalIcon::Warn);
        assert!(sig.summary.contains("4 commits"), "{}", sig.summary);
        assert!(sig.summary.contains("abcdef1"), "{}", sig.summary);
    }

    #[test]
    fn doc_review_signal_never_attested_renders_warn() {
        let sig = doc_review_signal(&DocReview::NeverAttested);
        assert_eq!(sig.icon, SignalIcon::Warn);
        assert!(sig.summary.contains("never attested"), "{}", sig.summary);
    }

    // ── ClaudeReview signal projection ─────────────────────────────

    #[test]
    fn claude_review_signal_no_activity_renders_not_applicable() {
        let sig = claude_review_signal(&ClaudeReview::NoActivity);
        assert_eq!(sig.icon, SignalIcon::NotApplicable);
        assert_eq!(sig.axis, AxisName::ClaudeReview);
        assert!(sig.summary.contains("not requested"), "{}", sig.summary);
    }

    #[test]
    fn claude_review_signal_addressed_renders_ok() {
        let sig = claude_review_signal(&ClaudeReview::Addressed);
        assert_eq!(sig.icon, SignalIcon::Ok);
        assert!(sig.summary.contains("addressed"), "{}", sig.summary);
    }

    #[test]
    fn claude_review_signal_fresh_renders_warn_with_thread_count() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let sig = claude_review_signal(&ClaudeReview::Fresh {
            latest_claude_at: at,
            body_at: at,
            latest_claude_body: String::new(),
            latest_claude_url: String::new(),
            inline_thread_count: 3,
            attested_at: None,
            head_sha: "a".repeat(40),
        });
        assert_eq!(sig.icon, SignalIcon::Warn);
        assert!(sig.summary.contains("fresh"), "{}", sig.summary);
        assert!(sig.summary.contains("3 inline threads"), "{}", sig.summary);
    }
}
