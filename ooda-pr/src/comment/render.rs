//! Render an OrientedState + Decision into a PR comment.
//!
//! Returns both the full markdown body and a stable dedup key
//! (axis lines + decision-kind tag, no prose) so that count-only
//! changes within the same structural state don't suppress posts.

use crate::decide::action::{Action, Automation, TargetEffect};
use crate::decide::decision::{Decision, DecisionHalt, Terminal};
use crate::orient::copilot::CopilotActivity;
use crate::orient::OrientedState;

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

pub struct Rendered {
    pub body: String,
    /// Stable key for dedup. Identical decisions on identical state
    /// produce identical keys, regardless of when the snapshot is
    /// taken.
    pub dedup_key: String,
}

pub fn render(oriented: &OrientedState, decision: &Decision) -> Rendered {
    let ci = ci_line(oriented);
    let copilot = copilot_line(oriented);
    let cursor = cursor_line(oriented);
    let reviews = reviews_line(oriented);

    let body = [
        ci.as_str(),
        copilot.as_str(),
        cursor.as_str(),
        reviews.as_str(),
        "",
        &decision_block(decision),
    ]
    .join("\n");

    // Dedup key omits the action description's prose so that count
    // changes ("3 unresolved" → "2 unresolved") within the same
    // structural state don't suppress posting. Includes the action's
    // blocker slug so two different agent-handoff actions on the
    // same axis state don't collapse to the same key.
    let dedup_key = format!(
        "{ci}\n{copilot}\n{cursor}\n{reviews}\n{}\n{}",
        decision_kind_tag(decision),
        decision_blocker_tag(decision),
    );

    Rendered { body, dedup_key }
}

fn decision_blocker_tag(d: &Decision) -> String {
    let action = match d {
        Decision::Execute(a) => Some(a),
        Decision::Halt(DecisionHalt::AgentNeeded(a))
        | Decision::Halt(DecisionHalt::HumanNeeded(a)) => Some(a),
        Decision::Halt(_) => None,
    };
    match action {
        None => String::new(),
        Some(a) => format!("{}|{}", a.blocker, action_payload_tag(a)),
    }
}

/// Comma-join any `Display` slice. Used for dedup tags, where the
/// stringified payload distinguishes otherwise-identical actions.
fn join_display<T: std::fmt::Display>(items: &[T]) -> String {
    items.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

/// Stringify any in-action counts that materially change the
/// rendered body so dedup doesn't collapse e.g. 3 unresolved
/// threads → 2 unresolved threads to the same key.
fn action_payload_tag(a: &Action) -> String {
    use crate::decide::action::ActionKind;
    match &a.kind {
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
        // No payload that affects the rendered comment.
        _ => String::new(),
    }
}

// ── per-axis lines ───────────────────────────────────────────────────

fn ci_line(o: &OrientedState) -> String {
    let ci = &o.ci;
    if ci.required.fail() > 0 {
        let names: Vec<String> = ci
            .required
            .failed_names()
            .iter()
            .map(|n| n.to_string())
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
        let names: Vec<String> =
            ci.missing_names.iter().map(|n| n.to_string()).collect();
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
    use crate::observe::github::pr_view::ReviewDecision;
    match o.reviews.decision {
        Some(ReviewDecision::Approved) => "✅ Approval".into(),
        Some(ReviewDecision::ChangesRequested) => "❌ Changes requested".into(),
        Some(ReviewDecision::ReviewRequired) => "👤 Approval required".into(),
        None => "— Approval · no review policy".into(),
    }
}

// ── decision block ───────────────────────────────────────────────────

fn decision_block(d: &Decision) -> String {
    match d {
        Decision::Execute(action) => action_block("Top action", action),
        Decision::Halt(DecisionHalt::Success) => {
            "**Halt:** Success — no advancing actions remain.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Merged)) => {
            "**Halt:** PR merged.".into()
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Closed)) => {
            "**Halt:** PR closed.".into()
        }
        Decision::Halt(DecisionHalt::AgentNeeded(action)) => {
            action_block("Agent needed", action)
        }
        Decision::Halt(DecisionHalt::HumanNeeded(action)) => {
            action_block("Human needed", action)
        }
    }
}

fn action_block(prefix: &str, action: &Action) -> String {
    let auto = match action.automation {
        Automation::Full => "auto".to_owned(),
        Automation::Wait { interval } => format!("wait {}s", interval.as_secs()),
        Automation::Agent => "agent".to_owned(),
        Automation::Human => "human".to_owned(),
    };
    let effect = match action.target_effect {
        TargetEffect::Blocks => "blocks",
        TargetEffect::Advances => "advances",
        TargetEffect::Neutral => "neutral",
    };
    format!(
        "**{prefix}:** `{kind}` ({auto}, {effect})\n\n{quoted}",
        kind = action.blocker,
        quoted = blockquote(&action.description),
    )
}

/// Prefix every line with `> ` so multi-line descriptions render
/// as a single quote block on GitHub.
fn blockquote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
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
    use crate::decide::action::{Action, ActionKind, Automation, TargetEffect};
    use crate::decide::decision::{Decision, DecisionHalt};
    use crate::ids::Timestamp;
    use crate::observe::github::pr_view::Mergeable;
    use crate::orient::ci::{CheckBucket, CiSummary, FailedCheck};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestState;

    fn empty_oriented() -> OrientedState {
        OrientedState {
            ci: CiSummary {
                required: CheckBucket::default(),
                missing_names: vec![],
                completed_at: None,
                advisory: CheckBucket::default(),
            },
            state: PullRequestState {
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
                merge_state_status:
                    crate::observe::github::pr_view::MergeStateStatus::Clean,
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
            },
            copilot: None,
            cursor: None,
            threads: vec![],
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
        o.ci.required.failed = vec![FailedCheck {
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
    fn render_halt_success_yields_concise_block() {
        let o = empty_oriented();
        let r = render(&o, &Decision::Halt(DecisionHalt::Success));
        assert!(r.body.contains("Halt:"));
        assert!(r.body.contains("Success"));
        assert!(r.dedup_key.contains("halt:success"));
    }

    #[test]
    fn render_action_blockquotes_multiline_description() {
        let o = empty_oriented();
        let action = Action {
            kind: ActionKind::AddressThreads { threads: vec![] },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: crate::decide::action::Urgency::BlockingFix,
            description: "line one\nline two\nline three".into(),
            blocker: crate::ids::BlockerKey::tag("unresolved_threads"),
        };
        let r = render(&o, &Decision::Execute(action));
        assert!(r.body.contains("> line one"));
        assert!(r.body.contains("> line two"));
        assert!(r.body.contains("> line three"));
    }

    #[test]
    fn dedup_key_stable_across_volatile_render_calls() {
        let o = empty_oriented();
        let r1 = render(&o, &Decision::Halt(DecisionHalt::Success));
        let r2 = render(&o, &Decision::Halt(DecisionHalt::Success));
        assert_eq!(r1.dedup_key, r2.dedup_key);
    }
}
