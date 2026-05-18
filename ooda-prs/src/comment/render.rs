//! Project orient + decision into the PR comment surface.
//!
//! Two outputs: the markdown body and a stable dedup key.
//!
//! Composition: the body is the dashboard projection's status-
//! comment render with a header prepended (the header carries
//! identifiers the projection itself does not hold). The dedup
//! key is keyed on structural state only — per-axis lines plus
//! decision-kind plus decision-blocker — so identical structure
//! across iterations collapses to one re-post-suppressing key,
//! while structural changes (axis transitions, blocker switches,
//! payload count flips) reliably break it.

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt, Terminal};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::orient::ci::CiReport;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::copilot::{CopilotActivity, CopilotReport};
use crate::orient::cursor::CursorReport;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use crate::orient::reviews::ReviewSummary;
use serde::Serialize;

/// Per-consumer input slice for [`render`]. Each field declares a
/// typed dep ref. The struct is the function signature reified;
/// it is not a god-struct in the [`crate::orient::OrientedState`]
/// sense — its scope is exactly what this one consumer reads.
pub(crate) struct RenderInputs<'a> {
    pub ci: &'a CiReport,
    pub cursor: Option<&'a CursorReport>,
    pub copilot: Option<&'a CopilotReport>,
    pub reviews: &'a ReviewSummary,
    pub pull_request_metadata: &'a PullRequestMetadata,
    pub doc_review: &'a DocReview,
    pub claude_review: &'a ClaudeReview,
}

impl<'a> From<&'a crate::orient::OrientedState> for RenderInputs<'a> {
    /// Transitional bridge: while [`crate::orient::OrientedState`]
    /// still exists, callers can construct `RenderInputs` from it
    /// via `From::from`. Once `OrientedState` is removed, callers
    /// will construct refs directly from per-axis locals.
    fn from(o: &'a crate::orient::OrientedState) -> Self {
        Self {
            ci: &o.ci,
            cursor: o.cursor.as_ref(),
            copilot: o.copilot.as_ref(),
            reviews: &o.reviews,
            pull_request_metadata: &o.pull_request_metadata,
            doc_review: &o.doc_review,
            claude_review: &o.claude_review,
        }
    }
}

/// Glyph for a tier slug. The set of slugs is shared across the
/// bot-review axes; an unknown slug renders as a question mark.
fn tier_emoji(slug: &str) -> &'static str {
    match slug {
        "bronze" => "🥉",
        "silver" => "🥈",
        "gold" => "🥇",
        "platinum" => "💠",
        _ => "?",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Rendered {
    pub body: String,
    /// Structural dedup key. Identical structural state projects
    /// to the same key on every iteration, independent of when
    /// the projection was taken.
    pub dedup_key: String,
}

/// Project the per-iteration triple plus header identifiers into
/// a body + dedup key pair.
///
/// Body composition: header (depends on caller-supplied
/// identifiers) followed by the dashboard's status-comment
/// projection. Terminal halts produce an empty dashboard body;
/// the caller substitutes a concise halt summary instead.
///
/// Dedup key composition: per-axis lines, decision discriminant,
/// decision blocker. Iteration is deliberately excluded — its
/// inclusion would defeat the dedup invariant by breaking the key
/// on every iteration.
pub(crate) fn render(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    iteration: Option<u32>,
    inputs: &RenderInputs<'_>,
    candidates: &[Action],
    decision: &Decision,
) -> Rendered {
    let dashboard = Dashboard::from_iteration(
        &crate::dashboard::DashboardInputs {
            ci: inputs.ci,
            cursor: inputs.cursor,
            copilot: inputs.copilot,
            pull_request_metadata: inputs.pull_request_metadata,
            doc_review: inputs.doc_review,
            claude_review: inputs.claude_review,
        },
        candidates,
        decision,
    );
    let header = header_line(slug, pr, iteration);
    let dashboard_body = dashboard.render_status_comment();
    let body = if dashboard_body.is_empty() {
        // Empty dashboard body ⇒ terminal halt path; substitute
        // a halt summary so the comment surface stays meaningful.
        format!("{header}\n\n{}\n", halt_summary(decision))
    } else {
        format!("{header}\n\n{dashboard_body}")
    };

    let ci = ci_line(inputs.ci);
    let copilot = copilot_line(inputs.copilot);
    let cursor = cursor_line(inputs.cursor);
    let reviews = reviews_line(inputs.reviews);
    let pr_meta = pull_request_metadata_line(inputs.pull_request_metadata);
    let doc_review = doc_review_line(inputs.doc_review);
    let claude_review = claude_review_line(inputs.claude_review);
    // Dedup key composition: per-axis lines (structural state) +
    // decision discriminant (which arm fired) + decision blocker
    // tagged with payload (which gate, with cardinality-bearing
    // payload). The action's human prose is intentionally absent
    // so wording-only changes do not break dedup; cardinality
    // changes travel through the payload tag instead.
    let dedup_key = format!(
        "{ci}\n{copilot}\n{cursor}\n{reviews}\n{pr_meta}\n{doc_review}\n{claude_review}\n{}\n{}",
        decision_kind_tag(decision),
        decision_blocker_tag(decision),
    );

    Rendered { body, dedup_key }
}

fn header_line(slug: &RepoSlug, pr: PullRequestNumber, iteration: Option<u32>) -> String {
    match iteration {
        Some(i) => format!("## OODA · {slug}#{pr} — iteration {i}"),
        None => format!("## OODA · {slug}#{pr}"),
    }
}

/// One-line halt summary for the empty-candidate path. Reached
/// only by terminal halts in practice; handoff arms always
/// populate candidates. The trailing fallback covers a future
/// halt variant that elects to emit nothing.
fn halt_summary(d: &Decision) -> String {
    match d {
        Decision::Halt(DecisionHalt::Success) => {
            "**Halt:** Success — no advancing actions remain.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded)) => {
            "**Halt:** PR merged.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted)) => "**Halt:** PR closed.".into(),
        // Handoff arms populate candidates by construction; this
        // arm is reachable only via a future halt variant that
        // opts out of candidate emission.
        _ => "**Halt:** no candidates.".into(),
    }
}

fn decision_blocker_tag(d: &Decision) -> String {
    // Execute and handoff arms carry different payload carriers
    // but expose the same two fields the tag depends on. The
    // projection is inline rather than threading a unification
    // trait through a single-call-site abstraction.
    let (kind, blocker) = match d {
        Decision::Execute(a) => (&a.kind, &a.blocker),
        Decision::Halt(DecisionHalt::AgentNeeded(h) | DecisionHalt::HumanNeeded(h)) => {
            (&h.kind, &h.blocker)
        }
        Decision::Halt(_) => return String::new(),
    };
    format!("{}|{}", blocker, action_payload_tag(kind))
}

/// Comma-join a displayable slice. The stringified payload is what
/// distinguishes otherwise-identical actions in dedup tags.
fn join_display<T: std::fmt::Display>(items: &[T]) -> String {
    items.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

/// Stringify the payload bits that materially change the rendered
/// body. Inclusion here is the contract for "this change must
/// break dedup"; absence is the contract for "this change must
/// not break dedup". Operates on the discriminant carrier (shared
/// by Execute and handoff carriers) so the projection is uniform.
fn action_payload_tag(kind: &crate::decide::action::ActionKind) -> String {
    use crate::decide::action::ActionKind;
    match kind {
        // Cardinality projection: count change breaks the key. A
        // per-entry projection would be more precise but is not
        // what the rendered body distinguishes on.
        ActionKind::AddressThreads { threads } => threads.len().to_string(),
        ActionKind::AddressCopilotSuppressed { count } => count.to_string(),
        ActionKind::FixCi { check_name } => check_name.to_string(),
        ActionKind::WaitForCi { pending } => join_display(pending),
        ActionKind::TriageWait { blocked_checks } => join_display(blocked_checks),
        ActionKind::WaitForBotReview { reviewers } => join_display(reviewers),
        ActionKind::WaitForHumanReview { reviewers } => join_display(reviewers),
        ActionKind::ShortenTitle { current_len } => current_len.to_string(),
        // Distinct scope ⇒ distinct dedup key, so a re-throttle
        // against a different upstream bucket forces a re-post.
        ActionKind::WaitForRateLimit { scope } => scope.name().into(),
        // Variants whose payload does not affect the rendered body.
        _ => String::new(),
    }
}

// ── per-axis lines ───────────────────────────────────────────────────

fn ci_line(ci: &crate::orient::ci::CiReport) -> String {
    let ci = &ci.summary;
    if ci.required.fail() > 0 {
        let names: Vec<String> = ci
            .required
            .failed_names()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        format!(
            "❌ CI · {} failing: {}",
            crate::text::count(ci.required.fail(), "required check"),
            names.join(", "),
        )
    } else if ci.required.pending() > 0 {
        format!(
            "⏳ CI · {} pending",
            crate::text::count(ci.required.pending(), "required check"),
        )
    } else if ci.missing() > 0 {
        let names: Vec<String> = ci
            .missing_names
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        format!(
            "❓ CI · {} not started: {}",
            crate::text::count(ci.missing(), "required check"),
            names.join(", "),
        )
    } else {
        "✅ CI · required checks pass".into()
    }
}

fn copilot_line(copilot: Option<&crate::orient::copilot::CopilotReport>) -> String {
    let Some(c) = copilot else {
        return "— Copilot · not configured".into();
    };
    // An idle axis does not render a tier glyph: the default
    // tier would otherwise read as a judgment the bot has not
    // actually made.
    if matches!(c.activity, CopilotActivity::Idle) {
        return "— Copilot · idle (not requested for this PR)".into();
    }
    let mut detail: Vec<String> = Vec::new();
    if c.threads.unresolved > 0 {
        detail.push(format!("{} unresolved", c.threads.unresolved));
    } else if c.threads.stale > 0 {
        detail.push(format!("{} stale replies", c.threads.stale));
    } else if !c.fresh && c.tier.slug() == "gold" {
        detail.push("not at HEAD".into());
    }
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" · {}", detail.join(", "))
    };
    let slug = c.tier.slug();
    format!("{} Copilot · {}{suffix}", tier_emoji(slug), slug)
}

fn cursor_line(cursor: Option<&crate::orient::cursor::CursorReport>) -> String {
    let Some(c) = cursor else {
        return "— Cursor · no activity".into();
    };
    let mut detail: Vec<String> = Vec::new();
    if c.threads.unresolved > 0 {
        let bits = c.severity.nonzero_parts();
        if bits.is_empty() {
            detail.push(format!("{} unresolved", c.threads.unresolved));
        } else {
            detail.push(bits.join(", "));
        }
    }
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" · {}", detail.join(", "))
    };
    let slug = c.tier.slug();
    format!("{} Cursor · {}{suffix}", tier_emoji(slug), slug)
}

fn reviews_line(reviews: &crate::orient::reviews::ReviewSummary) -> String {
    use crate::observe::github::pull_request_view::ReviewDecision;
    match reviews.decision {
        Some(ReviewDecision::Approved) => "✅ Approval".into(),
        Some(ReviewDecision::ChangesRequested) => "❌ Changes requested".into(),
        Some(ReviewDecision::ReviewRequired) => "👤 Approval required".into(),
        None => "— Approval · no review policy".into(),
    }
}

fn pull_request_metadata_line(meta: &PullRequestMetadata) -> String {
    match meta {
        PullRequestMetadata::Synced => "✅ PR meta · synced".into(),
        PullRequestMetadata::Drift {
            attested_sha,
            commits_behind,
            ..
        } => format!(
            "⚠ PR meta · drifted {} since {}",
            drift_count(*commits_behind),
            attested_sha.chars().take(7).collect::<String>(),
        ),
        PullRequestMetadata::NeverAttested => "⚠ PR meta · never attested".into(),
    }
}

fn doc_review_line(doc_review: &DocReview) -> String {
    match doc_review {
        DocReview::Synced => "✅ Doc review · synced".into(),
        DocReview::Drift {
            attested_sha,
            commits_behind,
            ..
        } => format!(
            "⚠ Doc review · drifted {} since {}",
            drift_count(*commits_behind),
            attested_sha.chars().take(7).collect::<String>(),
        ),
        DocReview::NeverAttested => "⚠ Doc review · never attested".into(),
    }
}

/// Render a drift commit-count. `None` encodes "drift exists but
/// the count is unobservable" — distinct from `Some(0)`, which
/// would denote "drift exists, but zero commits separate the
/// states", a state the upstream cannot actually produce.
fn drift_count(commits_behind: Option<usize>) -> String {
    match commits_behind {
        Some(n) => crate::text::count(n, "commit"),
        None => "unknown commits".into(),
    }
}

fn claude_review_line(claude_review: &ClaudeReview) -> String {
    match claude_review {
        ClaudeReview::NoActivity => "— Claude review · not requested".into(),
        ClaudeReview::Addressed => "✅ Claude review · addressed".into(),
        ClaudeReview::Fresh {
            inline_thread_count,
            ..
        } => format!(
            "⚠ Claude review · fresh ({})",
            crate::text::count(*inline_thread_count, "inline thread"),
        ),
    }
}

fn decision_kind_tag(d: &Decision) -> &'static str {
    match d {
        Decision::Execute(_) => "exec",
        Decision::Halt(DecisionHalt::Success) => "halt:success",
        Decision::Halt(DecisionHalt::Terminal(_)) => "halt:terminal",
        Decision::Halt(DecisionHalt::AgentNeeded(_)) => "halt:agent",
        Decision::Halt(DecisionHalt::HumanNeeded(_)) => "halt:human",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect};
    use crate::decide::decision::{Decision, DecisionHalt};
    use crate::ids::{BlockerKey, Timestamp};
    use crate::observe::github::pull_request_view::Mergeable;
    use crate::orient::OrientedState;
    use crate::orient::ci::{
        CheckBucket, CiActivity, CiReport, CiSummary, FailedCheck, ResolvedState,
    };
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;
    use ooda_core::MidTier;

    fn slug() -> RepoSlug {
        RepoSlug::parse("acme/widget").unwrap()
    }

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn rebase_action() -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            },
            target_effect: TargetEffect::Blocks,
            urgency: crate::decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    fn empty_oriented() -> OrientedState {
        OrientedState {
            ci: CiReport {
                summary: CiSummary {
                    required: CheckBucket::default(),
                    missing_names: vec![],
                    completed_at: None,
                    advisory: CheckBucket::default(),
                },
                activity: CiActivity::Resolved(ResolvedState::AllGreen),
            },
            state: PullRequestProjection {
                conflict: Mergeable::Mergeable,
                draft: false,
                wip: false,
                title_len: 30,
                title_ok: true,
                body: true,
                summary: true,
                test_plan: true,
                content_label: true,
                assignees: 1,
                reviewers: 1,
                merge_when_ready: false,
                commits: 1,
                behind: false,
                has_open_parent_pr: false,
                merge_state_status:
                    crate::observe::github::pull_request_view::MergeStateStatus::Clean,
                updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                last_commit_at: None,
                active_branch_rule_types: vec![],
                required_check_names_per_ruleset: vec![],
                missing_required_check_names_on_head: vec![],
            },
            reviews: ReviewSummary {
                decision: None,
                threads_unresolved: 0,
                threads_total: 0,
                bot_comments: 0,
                approvals_on_head: 0,
                approvals_stale: 0,
                pending_reviews: PendingReviews::default(),
                bot_reviews: vec![],
                requested_reviewers: crate::orient::reviews::RequestedReviewerSet::default(),
                latest_human_changes_requested: None,
            },
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata: PullRequestMetadata::NeverAttested,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::NeverAttested,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
            branch_sync: crate::observe::branch::BranchSyncObservation {
                divergence: None,
                branch_graphite_tracked: false,
                gt_available: false,
            },
        }
    }

    #[test]
    fn ci_line_pass_state() {
        let o = empty_oriented();
        assert_eq!(ci_line(&o.ci), "✅ CI · required checks pass");
    }

    #[test]
    fn ci_line_failure_lists_names() {
        let mut o = empty_oriented();
        o.ci.summary.required.failed = vec![FailedCheck {
            name: crate::ids::CheckName::parse("Lint").unwrap(),
            description: String::new(),
            link: String::new(),
        }];
        assert!(ci_line(&o.ci).starts_with("❌ CI · 1 required check"));
        assert!(ci_line(&o.ci).contains("Lint"));
    }

    #[test]
    fn copilot_line_unconfigured() {
        let o = empty_oriented();
        assert_eq!(
            copilot_line(o.copilot.as_ref()),
            "— Copilot · not configured"
        );
    }

    #[test]
    fn cursor_line_no_activity() {
        let o = empty_oriented();
        assert_eq!(cursor_line(o.cursor.as_ref()), "— Cursor · no activity");
    }

    #[test]
    fn reviews_line_no_policy() {
        let o = empty_oriented();
        assert_eq!(reviews_line(&o.reviews), "— Approval · no review policy");
    }

    #[test]
    fn render_halt_success_yields_terminal_halt_line() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(7),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        assert!(r.body.contains("## OODA · acme/widget#753 — iteration 7"));
        assert!(r.body.contains("**Halt:**"));
        assert!(r.body.contains("Success"));
        assert!(r.dedup_key.contains("halt:success"));
    }

    #[test]
    fn render_terminal_merged_yields_merged_halt_line() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(3),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded)),
        );
        assert!(r.body.contains("PR merged"));
        assert!(r.dedup_key.contains("halt:terminal"));
    }

    #[test]
    fn render_execute_action_routes_through_dashboard() {
        let o = empty_oriented();
        let action = rebase_action();
        let r = render(
            &slug(),
            pr(),
            Some(2),
            &RenderInputs::from(&o),
            std::slice::from_ref(&action),
            &Decision::Execute(action.clone()),
        );
        // Witness that the body is the dashboard projection's
        // tier-grouped output, not a flat top-action line.
        assert!(r.body.contains("## OODA · acme/widget#753 — iteration 2"));
        assert!(
            r.body.contains("**Recommended (blocking fix):** Rebase:"),
            "{}",
            r.body
        );
        assert!(!r.body.contains("Top action"));
    }

    #[test]
    fn render_header_omits_iteration_when_absent() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            None,
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        assert!(r.body.starts_with("## OODA · acme/widget#753\n"));
        assert!(!r.body.contains("iteration"));
    }

    #[test]
    fn dedup_key_stable_across_volatile_render_calls() {
        let o = empty_oriented();
        // Iteration appears in the header but not the dedup key:
        // identical structural state across iterations must
        // produce identical keys so the dedup path suppresses
        // the re-post.
        let r1 = render(
            &slug(),
            pr(),
            Some(1),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        let r2 = render(
            &slug(),
            pr(),
            Some(99),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        assert_eq!(r1.dedup_key, r2.dedup_key);
    }

    // ── halt_summary fallback coverage ──
    //
    // Direct render coverage for the terminal-aborted arm and the
    // catch-all. The catch-all is unreachable in practice because
    // handoff arms always populate candidates; the coverage here
    // pins its behaviour for a future variant that elects to emit
    // nothing.

    fn handoff_action() -> ooda_core::HandoffAction<crate::decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            target_effect: TargetEffect::Blocks,
            urgency: crate::decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    #[test]
    fn render_terminal_aborted_yields_pull_request_closed_halt_line() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(5),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted)),
        );
        assert!(r.body.contains("PR closed"), "{}", r.body);
        assert!(r.dedup_key.contains("halt:terminal"));
    }

    #[test]
    fn render_agent_needed_with_empty_candidates_uses_fallback_summary() {
        // Synthetic: production never reaches this combination —
        // handoff arms always populate candidates — but the
        // fallback arm exists for resilience and must produce
        // a meaningful summary.
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(4),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::AgentNeeded(handoff_action())),
        );
        assert!(r.body.contains("**Halt:** no candidates."), "{}", r.body);
        assert!(r.dedup_key.contains("halt:agent"));
    }

    // ── pull_request_metadata_line ─────────────────────────────────────────────

    #[test]
    fn pull_request_metadata_line_synced_renders_check() {
        let mut o = empty_oriented();
        o.pull_request_metadata = PullRequestMetadata::Synced;
        assert_eq!(
            pull_request_metadata_line(&o.pull_request_metadata),
            "✅ PR meta · synced"
        );
    }

    #[test]
    fn pull_request_metadata_line_drift_includes_count_and_short_sha() {
        let mut o = empty_oriented();
        o.pull_request_metadata = PullRequestMetadata::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: Some(5),
        };
        let line = pull_request_metadata_line(&o.pull_request_metadata);
        assert!(line.contains("drifted 5 commits"), "{line}");
        assert!(line.contains("abcdef1"), "{line}");
    }

    #[test]
    fn pull_request_metadata_line_never_attested_renders_warn() {
        let mut o = empty_oriented();
        o.pull_request_metadata = PullRequestMetadata::NeverAttested;
        assert_eq!(
            pull_request_metadata_line(&o.pull_request_metadata),
            "⚠ PR meta · never attested"
        );
    }

    // ── doc_review_line ──

    #[test]
    fn doc_review_line_synced_renders_check() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::Synced;
        assert_eq!(doc_review_line(&o.doc_review), "✅ Doc review · synced");
    }

    #[test]
    fn doc_review_line_drift_includes_count_and_short_sha() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: Some(5),
        };
        let line = doc_review_line(&o.doc_review);
        assert!(line.contains("drifted 5 commits"), "{line}");
        assert!(line.contains("abcdef1"), "{line}");
    }

    #[test]
    fn doc_review_line_never_attested_renders_warn() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::NeverAttested;
        assert_eq!(
            doc_review_line(&o.doc_review),
            "⚠ Doc review · never attested"
        );
    }

    // ── claude_review_line ──

    #[test]
    fn claude_review_line_no_activity_renders_dash() {
        let o = empty_oriented();
        assert_eq!(
            claude_review_line(&o.claude_review),
            "— Claude review · not requested"
        );
    }

    #[test]
    fn claude_review_line_addressed_renders_check() {
        let mut o = empty_oriented();
        o.claude_review = ClaudeReview::Addressed;
        assert_eq!(
            claude_review_line(&o.claude_review),
            "✅ Claude review · addressed"
        );
    }

    #[test]
    fn claude_review_line_fresh_renders_warn_with_thread_count() {
        let mut o = empty_oriented();
        let at = chrono::DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        o.claude_review = ClaudeReview::Fresh {
            latest_claude_at: at,
            body_at: at,
            latest_claude_body: String::new(),
            latest_claude_url: String::new(),
            inline_thread_count: 2,
            attested_at: None,
            head_sha: "a".repeat(40),
        };
        let line = claude_review_line(&o.claude_review);
        assert!(line.contains("Claude review · fresh"), "{line}");
        assert!(line.contains("2 inline threads"), "{line}");
    }

    #[test]
    fn render_human_needed_with_empty_candidates_uses_fallback_summary() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(6),
            &RenderInputs::from(&o),
            &[],
            &Decision::Halt(DecisionHalt::HumanNeeded(handoff_action())),
        );
        assert!(r.body.contains("**Halt:** no candidates."), "{}", r.body);
        assert!(r.dedup_key.contains("halt:human"));
    }
}
