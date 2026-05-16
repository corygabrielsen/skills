//! Render an `OrientedState` + Decision into a PR comment.
//!
//! Returns both the full markdown body and a stable dedup key
//! (axis lines + decision-kind tag, no prose) so that count-only
//! changes within the same structural state don't suppress posts.
//!
//! Phase C of the tier-grouped dashboard work routes the body
//! through [`crate::dashboard::Dashboard::render_status_comment`]
//! so the PR comment surfaces the same tier-grouped projection the
//! on-disk `next.md` and the handoff preamble already use. The
//! header is rendered here (it depends on slug/pr/iteration, which
//! the dashboard doesn't carry); the dedup key stays per-axis +
//! decision-kind so structural state — not iteration count or
//! action prose — gates re-posts.

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt, Terminal};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::orient::OrientedState;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::copilot::CopilotActivity;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use serde::Serialize;

/// Lookup by tier slug (`bronze`/`silver`/`gold`/`platinum`).
/// Both `CopilotTier` and `CursorTier` produce the same slugs.
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
pub struct Rendered {
    pub body: String,
    /// Stable key for dedup. Identical decisions on identical state
    /// produce identical keys, regardless of when the snapshot is
    /// taken.
    pub dedup_key: String,
}

/// Render the PR status comment from the per-iteration triple
/// `(oriented, candidates, decision)` plus the slug / pr / iteration
/// the header needs.
///
/// Body shape:
/// * `## OODA · {repo}#{pr} — iteration N` header
/// * Dashboard render (winner + queued + signals + blockers) —
///   tier-grouped, same projection as `next.md`. For terminal halts
///   (`Halt::Success` / `Halt::Terminal`) the dashboard yields no
///   candidates and this function substitutes a concise halt line.
///
/// Dedup key shape (unchanged): per-axis lines plus decision-kind
/// tag plus decision-blocker tag. Iteration is intentionally
/// absent — every iteration would otherwise force a re-post even
/// when nothing structural changed.
pub fn render(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    iteration: Option<u32>,
    oriented: &OrientedState,
    candidates: &[Action],
    decision: &Decision,
) -> Rendered {
    let dashboard = Dashboard::from_iteration(oriented, candidates, decision);
    let header = header_line(slug, pr, iteration);
    let dashboard_body = dashboard.render_status_comment();
    let body = if dashboard_body.is_empty() {
        // Terminal halt or empty-candidate path — no recommended
        // action to render. Surface the halt summary so the comment
        // is still meaningful in the PR timeline.
        format!("{header}\n\n{}\n", halt_summary(decision))
    } else {
        format!("{header}\n\n{dashboard_body}")
    };

    let ci = ci_line(oriented);
    let copilot = copilot_line(oriented);
    let cursor = cursor_line(oriented);
    let reviews = reviews_line(oriented);
    let pr_meta = pull_request_metadata_line(oriented);
    let doc_review = doc_review_line(oriented);
    let claude_review = claude_review_line(oriented);
    // Dedup key omits the action description's prose so that count
    // changes ("3 unresolved" → "2 unresolved") within the same
    // structural state don't suppress posting. Includes the action's
    // blocker slug so two different agent-handoff actions on the
    // same axis state don't collapse to the same key.
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

/// Concise halt-line for the empty-candidate case (terminal halts
/// only; agent/human handoffs populate candidates and never hit this
/// branch). Mirrors the `decision_block` strings used pre-Phase-C so
/// the PR timeline still gets the same halt message.
fn halt_summary(d: &Decision) -> String {
    match d {
        Decision::Halt(DecisionHalt::Success) => {
            "**Halt:** Success — no advancing actions remain.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded)) => {
            "**Halt:** PR merged.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted)) => "**Halt:** PR closed.".into(),
        // Every other arm carries candidates so render_status_comment
        // produces a non-empty body — this fallback would only trip
        // on a future variant that opts out of candidate emission.
        _ => "**Halt:** no candidates.".into(),
    }
}

fn decision_blocker_tag(d: &Decision) -> String {
    // Mechanical (Execute) and handoff variants carry different
    // payload types — `Action<K>` vs `HandoffAction<K>` — but both
    // expose `kind: K` and `blocker: BlockerKey`. The tag depends
    // only on those two fields, so we project them inline rather
    // than threading a unification trait.
    let (kind, blocker) = match d {
        Decision::Execute(a) => (&a.kind, &a.blocker),
        Decision::Halt(DecisionHalt::AgentNeeded(h) | DecisionHalt::HumanNeeded(h)) => {
            (&h.kind, &h.blocker)
        }
        Decision::Halt(_) => return String::new(),
    };
    format!("{}|{}", blocker, action_payload_tag(kind))
}

/// Comma-join any `Display` slice. Used for dedup tags, where the
/// stringified payload distinguishes otherwise-identical actions.
fn join_display<T: std::fmt::Display>(items: &[T]) -> String {
    items.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

/// Stringify any in-action counts that materially change the
/// rendered body so dedup doesn't collapse e.g. 3 unresolved
/// threads → 2 unresolved threads to the same key.
///
/// Takes `&ActionKind` rather than `&Action` so it works
/// uniformly over `Action<K>` and `HandoffAction<K>` — both
/// expose `kind: K`.
fn action_payload_tag(kind: &crate::decide::action::ActionKind) -> String {
    use crate::decide::action::ActionKind;
    match kind {
        // Use len() so 3→2 progress flips the dedup key (re-post),
        // matching the prior count-based behavior. Using thread IDs
        // here would be more precise but unnecessary churn — the
        // count is what materially changes the rendered comment.
        ActionKind::AddressThreads { threads } => threads.len().to_string(),
        ActionKind::AddressCopilotSuppressed { count } => count.to_string(),
        ActionKind::FixCi { check_name } => check_name.to_string(),
        ActionKind::WaitForCi { pending } => join_display(pending),
        ActionKind::TriageWait { blocked_checks } => join_display(blocked_checks),
        ActionKind::WaitForBotReview { reviewers } => join_display(reviewers),
        ActionKind::WaitForHumanReview { reviewers } => join_display(reviewers),
        ActionKind::ShortenTitle { current_len } => current_len.to_string(),
        // Scope is in the dedup key so a Wait for graphql-primary
        // doesn't re-post the same comment when the next iteration
        // hits secondary instead.
        ActionKind::WaitForRateLimit { scope } => scope.name().into(),
        // No payload that affects the rendered comment.
        _ => String::new(),
    }
}

// ── per-axis lines ───────────────────────────────────────────────────

fn ci_line(o: &OrientedState) -> String {
    let ci = &o.ci.summary;
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

fn copilot_line(o: &OrientedState) -> String {
    let Some(c) = &o.copilot else {
        return "— Copilot · not configured".into();
    };
    // Configured-but-dormant doesn't get a tier emoji — the tier
    // would be Bronze by default and that misleads the reader into
    // thinking Copilot judged the PR poorly.
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

fn cursor_line(o: &OrientedState) -> String {
    let Some(c) = &o.cursor else {
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

fn reviews_line(o: &OrientedState) -> String {
    use crate::observe::github::pull_request_view::ReviewDecision;
    match o.reviews.decision {
        Some(ReviewDecision::Approved) => "✅ Approval".into(),
        Some(ReviewDecision::ChangesRequested) => "❌ Changes requested".into(),
        Some(ReviewDecision::ReviewRequired) => "👤 Approval required".into(),
        None => "— Approval · no review policy".into(),
    }
}

fn pull_request_metadata_line(o: &OrientedState) -> String {
    match &o.pull_request_metadata {
        PullRequestMetadata::Synced => "✅ PR meta · synced".into(),
        PullRequestMetadata::Drift {
            attested_sha,
            commits_behind,
            ..
        } => format!(
            "⚠ PR meta · drifted {} since {}",
            crate::text::count(*commits_behind, "commit"),
            attested_sha.chars().take(7).collect::<String>(),
        ),
        PullRequestMetadata::NeverAttested => "⚠ PR meta · never attested".into(),
    }
}

fn doc_review_line(o: &OrientedState) -> String {
    match &o.doc_review {
        DocReview::Synced => "✅ Doc review · synced".into(),
        DocReview::Drift {
            attested_sha,
            commits_behind,
            ..
        } => format!(
            "⚠ Doc review · drifted {} since {}",
            crate::text::count(*commits_behind, "commit"),
            attested_sha.chars().take(7).collect::<String>(),
        ),
        DocReview::NeverAttested => "⚠ Doc review · never attested".into(),
    }
}

fn claude_review_line(o: &OrientedState) -> String {
    match &o.claude_review {
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
    use crate::orient::ci::{
        CheckBucket, CiActivity, CiReport, CiSummary, FailedCheck, ResolvedState,
    };
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;

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
            urgency: crate::decide::action::Urgency::BlockingFix,
            blocker: BlockerKey::tag("rebase-needed"),
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
            codex_review: None,
        }
    }

    #[test]
    fn ci_line_pass_state() {
        let o = empty_oriented();
        assert_eq!(ci_line(&o), "✅ CI · required checks pass");
    }

    #[test]
    fn ci_line_failure_lists_names() {
        let mut o = empty_oriented();
        o.ci.summary.required.failed = vec![FailedCheck {
            name: crate::ids::CheckName::parse("Lint").unwrap(),
            description: String::new(),
            link: String::new(),
        }];
        assert!(ci_line(&o).starts_with("❌ CI · 1 required check"));
        assert!(ci_line(&o).contains("Lint"));
    }

    #[test]
    fn copilot_line_unconfigured() {
        let o = empty_oriented();
        assert_eq!(copilot_line(&o), "— Copilot · not configured");
    }

    #[test]
    fn cursor_line_no_activity() {
        let o = empty_oriented();
        assert_eq!(cursor_line(&o), "— Cursor · no activity");
    }

    #[test]
    fn reviews_line_no_policy() {
        let o = empty_oriented();
        assert_eq!(reviews_line(&o), "— Approval · no review policy");
    }

    #[test]
    fn render_halt_success_yields_terminal_halt_line() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(7),
            &o,
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
            &o,
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
            &o,
            std::slice::from_ref(&action),
            &Decision::Execute(action.clone()),
        );
        // Body now goes through the dashboard's status-comment
        // render — confirm the tier-grouped headline is present
        // instead of the legacy `Top action:` block.
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
            &o,
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        assert!(r.body.starts_with("## OODA · acme/widget#753\n"));
        assert!(!r.body.contains("iteration"));
    }

    #[test]
    fn dedup_key_stable_across_volatile_render_calls() {
        let o = empty_oriented();
        // Iteration is part of the header but intentionally absent
        // from the dedup key — re-iterating without structural state
        // changes must collapse to the same key so the dedup post
        // path skips the comment.
        let r1 = render(
            &slug(),
            pr(),
            Some(1),
            &o,
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        let r2 = render(
            &slug(),
            pr(),
            Some(99),
            &o,
            &[],
            &Decision::Halt(DecisionHalt::Success),
        );
        assert_eq!(r1.dedup_key, r2.dedup_key);
    }

    // ── halt_summary fallback arms ──
    //
    // Direct render coverage for the Terminal::Aborted arm and the
    // catch-all `_` arm. The `_` arm fires when AgentNeeded or
    // HumanNeeded reach the empty-candidate path; in practice candidates
    // are populated by the handoff path, but the fallback exists so a
    // future halt variant that opts out of candidate emission still
    // produces a non-empty body in the PR timeline.

    fn handoff_action() -> ooda_core::HandoffAction<crate::decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            target_effect: TargetEffect::Blocks,
            urgency: crate::decide::action::Urgency::BlockingFix,
            blocker: BlockerKey::tag("rebase-needed"),
        }
    }

    #[test]
    fn render_terminal_aborted_yields_pull_request_closed_halt_line() {
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(5),
            &o,
            &[],
            &Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted)),
        );
        assert!(r.body.contains("PR closed"), "{}", r.body);
        assert!(r.dedup_key.contains("halt:terminal"));
    }

    #[test]
    fn render_agent_needed_with_empty_candidates_uses_fallback_summary() {
        // Synthetic: empty candidates with an AgentNeeded halt
        // (production-side AgentNeeded paths always populate
        // candidates, but the fallback arm exists for resilience).
        let o = empty_oriented();
        let r = render(
            &slug(),
            pr(),
            Some(4),
            &o,
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
        assert_eq!(pull_request_metadata_line(&o), "✅ PR meta · synced");
    }

    #[test]
    fn pull_request_metadata_line_drift_includes_count_and_short_sha() {
        let mut o = empty_oriented();
        o.pull_request_metadata = PullRequestMetadata::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: 5,
        };
        let line = pull_request_metadata_line(&o);
        assert!(line.contains("drifted 5 commits"), "{line}");
        assert!(line.contains("abcdef1"), "{line}");
    }

    #[test]
    fn pull_request_metadata_line_never_attested_renders_warn() {
        let mut o = empty_oriented();
        o.pull_request_metadata = PullRequestMetadata::NeverAttested;
        assert_eq!(pull_request_metadata_line(&o), "⚠ PR meta · never attested");
    }

    // ── doc_review_line ──

    #[test]
    fn doc_review_line_synced_renders_check() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::Synced;
        assert_eq!(doc_review_line(&o), "✅ Doc review · synced");
    }

    #[test]
    fn doc_review_line_drift_includes_count_and_short_sha() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::Drift {
            attested_sha: "abcdef1234567890abcdef1234567890abcdef12".into(),
            head_sha: "9".repeat(40),
            commits_behind: 5,
        };
        let line = doc_review_line(&o);
        assert!(line.contains("drifted 5 commits"), "{line}");
        assert!(line.contains("abcdef1"), "{line}");
    }

    #[test]
    fn doc_review_line_never_attested_renders_warn() {
        let mut o = empty_oriented();
        o.doc_review = DocReview::NeverAttested;
        assert_eq!(doc_review_line(&o), "⚠ Doc review · never attested");
    }

    // ── claude_review_line ──

    #[test]
    fn claude_review_line_no_activity_renders_dash() {
        let o = empty_oriented();
        assert_eq!(claude_review_line(&o), "— Claude review · not requested");
    }

    #[test]
    fn claude_review_line_addressed_renders_check() {
        let mut o = empty_oriented();
        o.claude_review = ClaudeReview::Addressed;
        assert_eq!(claude_review_line(&o), "✅ Claude review · addressed");
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
        let line = claude_review_line(&o);
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
            &o,
            &[],
            &Decision::Halt(DecisionHalt::HumanNeeded(handoff_action())),
        );
        assert!(r.body.contains("**Halt:** no candidates."), "{}", r.body);
        assert!(r.dedup_key.contains("halt:human"));
    }
}
