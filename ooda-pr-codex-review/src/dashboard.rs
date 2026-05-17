//! Tier-grouped dashboard projection.
//!
//! Projects one iteration's `(oriented, candidates, decision)` triple
//! into a single typed structure that feeds three independent
//! rendering surfaces (per-iteration files, handoff-prompt preamble,
//! status comment). Renderers are pure functions over the projection.
//!
//! The driver-side decision type owns the executor signal; this
//! type owns the human-facing structured snapshot. The two names
//! coexist by design — the projection is not a decision, it is a
//! view derived from one.
//!
//! Per-binary by construction. A cross-binary lift would force the
//! shared spine to carry domain-specific axis names; the dashboard
//! is the right place for those to live.

use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt};
use crate::ids::BlockerKey;
use crate::orient::ci::ci_signal;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::copilot::copilot_signal;
use crate::orient::cursor::cursor_signal;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use ooda_core::MidTier;
use ooda_core::{ActionKindName, NonEmpty, PromptSection, SingleLineString, Urgency};
use serde::Serialize;
use std::fmt::Write;

// ── Public types ─────────────────────────────────────────────────────

/// Tier-grouped snapshot for human-facing rendering. Bundles the
/// ranked candidates, per-axis health signals, and the deduplicated
/// blocker list. Decoupled from the executor: the runner drives off
/// the decision type; the dashboard is the view.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Dashboard {
    /// Candidates in urgency order. Empty iff the upstream halt
    /// produced no actionable candidate (terminal arms).
    pub candidates: Vec<RankedCandidate>,
    /// Per-axis health signals, one entry per axis that elected to
    /// emit. Quiet/inapplicable axes project nothing and are
    /// omitted at the axis-projection boundary.
    pub signals: Vec<AxisSignal>,
    /// Cross-candidate blocker list, deduplicated by key and
    /// preserving first-seen order.
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RankedCandidate {
    /// Action discriminant for stable rendering — payload-free so
    /// the rendered surface survives payload-shape changes.
    pub action_name: &'static str,
    /// Single-line human-readable rendering of the action.
    pub action_log: String,
    /// Effect debug form for the detail line.
    pub effect_debug: String,
    /// Urgency tier — drives tier grouping in the renderer.
    pub urgency: Urgency,
    /// Gate identity for this candidate.
    pub blocker: BlockerKey,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AxisSignal {
    pub axis: AxisName,
    pub icon: SignalIcon,
    pub summary: String,
}

/// Coarse health classification on a five-bucket scale. The bucket
/// is the rendering primitive; per-axis projectors decide which
/// bucket each axis-internal state maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum SignalIcon {
    Ok,
    InFlight,
    Warn,
    Failed,
    NotApplicable,
}

impl SignalIcon {
    /// Glyph for compact rendering, paired with axis name and summary.
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
/// New axes append.
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
    /// Gate identity carried verbatim from the candidate.
    pub tag: BlockerKey,
    /// Action discriminant — the rendering context paired with the
    /// gate in human-facing surfaces.
    pub action_name: &'static str,
}

// ── Construction ─────────────────────────────────────────────────────

impl Dashboard {
    /// Assemble a [`Dashboard`] from the per-iteration triple.
    /// Pure projection — no additional observation source.
    ///
    /// Declared deps: the six axes' reports that feed the
    /// dashboard's signal stripe.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_iteration(
        ci: &crate::orient::ci::CiReport,
        cursor: Option<&crate::orient::cursor::CursorReport>,
        copilot: Option<&crate::orient::copilot::CopilotReport>,
        pull_request_metadata: &PullRequestMetadata,
        doc_review: &crate::orient::doc_review::DocReview,
        claude_review: &crate::orient::claude_review::ClaudeReview,
        candidates: &[Action],
        decision: &Decision,
    ) -> Self {
        let candidates = build_candidates(candidates, decision);
        let signals = collect_signals(
            ci,
            cursor,
            copilot,
            pull_request_metadata,
            doc_review,
            claude_review,
        );
        let blockers = collect_blockers(&candidates);
        Self {
            candidates,
            signals,
            blockers,
        }
    }
}

/// Project the candidate slice for dashboard rendering. Terminal
/// halt arms (no actionable candidate by construction) project to
/// the empty slice; renderers fall through to the no-action path.
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

fn collect_signals(
    ci: &crate::orient::ci::CiReport,
    cursor: Option<&crate::orient::cursor::CursorReport>,
    copilot: Option<&crate::orient::copilot::CopilotReport>,
    pull_request_metadata: &PullRequestMetadata,
    doc_review: &crate::orient::doc_review::DocReview,
    claude_review: &crate::orient::claude_review::ClaudeReview,
) -> Vec<AxisSignal> {
    let mut out: Vec<AxisSignal> = Vec::new();
    if let Some(c) = copilot
        && let Some(sig) = copilot_signal(&c.activity)
    {
        out.push(sig);
    }
    if let Some(sig) = ci_signal(&ci.activity) {
        out.push(sig);
    }
    if let Some(c) = cursor
        && let Some(sig) = cursor_signal(&c.activity)
    {
        out.push(sig);
    }
    out.push(pull_request_metadata_signal(pull_request_metadata));
    out.push(doc_review_signal(doc_review));
    out.push(claude_review_signal(claude_review));
    out
}

/// Project the PR-metadata attestation axis onto a dashboard signal.
/// Synced → Ok; drift → Warn with commit-count detail; never-attested
/// → Warn with a first-attestation summary.
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
                drift_commits(*commits_behind),
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

/// Project the doc-review attestation axis onto a dashboard signal.
/// Same bucket assignment as the other SHA-keyed attestation axes.
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
                drift_commits(*commits_behind),
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

/// Project the Claude-review axis onto a dashboard signal. Diverges
/// from the SHA-keyed axes by collapsing the "no surface to grade"
/// case to `NotApplicable` instead of a warn — there is no upstream
/// surface whose drift could be measured.
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

/// Render a drift commit-count. `None` encodes "drift exists but
/// the count is unobservable" — the upstream compare call was
/// unavailable.
fn drift_commits(commits_behind: Option<usize>) -> String {
    match commits_behind {
        Some(n) => crate::text::count(n, "commit"),
        None => "unknown commits".into(),
    }
}

/// Deduplicate by gate identity while preserving first-seen order.
/// Multiple candidates naming the same gate collapse to one row.
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
    /// Render the per-iteration next-action surface. Section order:
    /// recommended winner → same-tier alternatives → one section
    /// per lower-urgency tier → signals → blockers. Empty sections
    /// are omitted.
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
        // The winner was rendered in its own section; drop it from
        // its bucket so the alternatives list does not repeat it.
        if let Some(bucket) = by_tier.first_mut() {
            bucket.candidates.remove(0);
        }
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            out.push_str("\n## Also at this tier\n");
            for c in &top.candidates {
                writeln!(out, "- {}: {}", c.action_name, c.action_log).expect("write to String");
            }
        }

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

    /// Render the PR status comment body. Same section order as the
    /// per-iteration surface; tuned for the comment context (compact
    /// headline, no effect-debug line). The caller supplies the
    /// header (it depends on identifiers the projection does not
    /// carry). Empty candidate list ⇒ empty body, leaving the
    /// caller to substitute a halt summary.
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
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            out.push_str("\n**Also at this tier:**\n");
            for c in &top.candidates {
                writeln!(out, "- {}: {}", c.action_name, c.action_log).expect("write to String");
            }
        }

        // Lower tiers render inline with italicised labels rather
        // than as nested subheadings — the comment surface reads
        // better as one bulleted block.
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

    /// Render the structured blocker list as a standalone surface.
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

    /// Project the dashboard as prompt sections suitable for
    /// prepending to a handoff body. Section order matches the
    /// other rendering surfaces; empty sections are omitted.
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
        // Header-and-list pairing keeps the candidate ordinals
        // meaningful: header is a separate paragraph so the list
        // numbering starts at one.
        if let Some(top) = by_tier.first()
            && !top.candidates.is_empty()
        {
            sections.push(PromptSection::Paragraph(format!(
                "Also at this tier ({}):",
                urgency_label(top.urgency),
            )));
            push_candidate_list(&mut sections, &top.candidates);
        }

        // One header-and-list pair per lower tier; only the tier
        // label varies across buckets.
        for bucket in by_tier.iter().skip(1) {
            sections.push(PromptSection::Paragraph(format!(
                "Queued ({}):",
                urgency_label(bucket.urgency),
            )));
            push_candidate_list(&mut sections, &bucket.candidates);
        }

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

/// Append a numbered list of `<action>: <log>` lines for the given
/// candidates. No-op on an empty slice; the caller is responsible
/// for gating the section header.
fn push_candidate_list(sections: &mut Vec<PromptSection>, candidates: &[&RankedCandidate]) {
    let items: Vec<SingleLineString> = candidates
        .iter()
        .map(|c| SingleLineString::new(format!("{}: {}", c.action_name, c.action_log)))
        .collect();
    if let Some(list) = NonEmpty::try_from_vec(items) {
        sections.push(PromptSection::NumberedList(list));
    }
}

/// One urgency level paired with its candidates in stable input
/// order. The stability of the upstream sort makes this equivalent
/// to axis order within the bucket.
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

/// Stable human-readable label for an urgency tier. Decoupled from
/// the variant identifier so the rendered surface survives Rust-side
/// renames.
fn urgency_label(u: Urgency) -> &'static str {
    match u {
        Urgency::Pre => "opening",
        Urgency::Mid(MidTier::Critical) => "critical",
        Urgency::Mid(MidTier::BlockingFix) => "blocking fix",
        Urgency::Mid(MidTier::BlockingWait) => "blocking wait",
        Urgency::Mid(MidTier::BlockingHuman) => "blocking human",
        Urgency::Mid(MidTier::Advancing) => "advancing",
        Urgency::Mid(MidTier::Hygiene) => "hygiene",
        Urgency::Post => "closeout",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect};
    use crate::orient::copilot::CopilotActivity;
    use crate::orient::cursor::CursorActivity;
    use ooda_core::HandoffPrompt;
    use ooda_core::MidTier;

    fn act(kind: ActionKind, urgency: Urgency, blocker: &str, log: &str) -> Action {
        Action {
            kind,
            effect: ActionEffect::Full { log: log.into() },
            target_effect: TargetEffect::Blocks,
            urgency,
            blocker: BlockerKey::for_test(blocker),
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
            Urgency::Mid(MidTier::Critical),
            "copilot:rerequest",
            "rerequest copilot",
        )
    }

    fn wait_for_ci() -> Action {
        let pending =
            ooda_core::NonEmpty::singleton(crate::ids::CheckName::parse("ci/build").unwrap());
        act(
            ActionKind::WaitForCi { pending },
            Urgency::Mid(MidTier::BlockingWait),
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
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("review:approval"),
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
            Urgency::Mid(MidTier::Critical),
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
        // Empty projection ⇒ single no-action paragraph; every
        // optional section is suppressed.
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
            Urgency::Mid(MidTier::Critical),
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
        // Representative projection for visual inspection under
        // `--nocapture`. The assertion is the binding contract.
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
        // A non-empty candidate set always projects a Blockers
        // section — even one candidate names one gate.
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
            Urgency::Mid(MidTier::Critical),
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
        // Visual inspection under --nocapture; the headline
        // assertion is the binding contract.
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
        // Render through the runtime Display path so any drift
        // between the prompt section formatter and the dashboard's
        // expected rendering surfaces in tests.
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
            Urgency::Mid(MidTier::Critical),
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
        // Visual inspection under --nocapture; the headline
        // assertion is the binding contract in CI.
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
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("review:approval"),
        };
        let decision = Decision::Halt(DecisionHalt::HumanNeeded(handoff));
        let candidates = build_candidates(&[request_approval()], &decision);
        assert_eq!(candidates.len(), 1);
    }

    // ── Signal projection exhaustive enumeration ──────────────────

    /// Exhaustive cross-product over the per-axis activity enums:
    /// every variant is classified to its expected bucket. The
    /// matches below are structurally exhaustive — a new variant in
    /// any axis fails to compile here.
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

        // Idle is the single-emit-nothing case for this axis.
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

        // Idle is the only non-emitting CI state; every Resolved
        // and InFlight variant projects to a bucket.
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

        // NotApplicable is the only non-emitting Cursor state.
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
            commits_behind: Some(4),
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
            commits_behind: Some(4),
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
