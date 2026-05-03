//! State candidates split into blocking (must clear to merge) and
//! hygiene (non-blocking metadata). decide.rs emits hygiene last so
//! it doesn't shadow review/bot work.

use crate::ids::BlockerKey;
use std::time::Duration;

use crate::observe::github::pr_view::{Mergeable, MergeStateStatus};
use crate::orient::state::PullRequestState;

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};

/// Mechanical merge blockers — must clear for the PR to be
/// mergeable at all. Emitted by decide before review/bot axes.
pub fn blocking_candidates(state: &PullRequestState) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    // PR-shape blockers (draft / wip / title) come BEFORE
    // mergeability waits — drafts commonly report mergeable=UNKNOWN
    // and would otherwise spin on WaitForMergeability instead of
    // emitting MarkReady (the action that lets GitHub compute it).
    if state.draft {
        out.push(Action {
            kind: ActionKind::MarkReady,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Critical,
            description: "Mark PR as ready for review".into(),
            blocker: BlockerKey::tag("draft"),
        });
    }
    if state.wip {
        out.push(Action {
            kind: ActionKind::RemoveWipLabel,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Critical,
            description: "Remove \"work in progress\" label".into(),
            blocker: BlockerKey::tag("wip_label"),
        });
    }
    if !state.title_ok {
        out.push(Action {
            kind: ActionKind::ShortenTitle {
                current_len: state.title_len as u32,
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: format!(
                "Shorten title ({} chars, max 50)",
                state.title_len
            ),
            blocker: BlockerKey::tag("title_too_long"),
        });
    }

    // Mergeability-derived blockers come after PR-shape, since
    // mergeability requires the PR to be ready first.
    if state.conflict == Mergeable::Unknown {
        out.push(Action {
            kind: ActionKind::WaitForMergeability,
            automation: Automation::Wait { interval: Duration::from_secs(30) },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: "GitHub is still computing mergeability — wait and re-observe".into(),
            blocker: BlockerKey::tag("mergeability_unknown"),
        });
    } else if state.conflict == Mergeable::Conflicting {
        out.push(Action {
            kind: ActionKind::Rebase,
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: "Rebase to resolve merge conflicts".into(),
            blocker: BlockerKey::tag("merge_conflict"),
        });
    } else if state.behind {
        out.push(Action {
            kind: ActionKind::Rebase,
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: "Rebase onto the latest base branch".into(),
            blocker: BlockerKey::tag("behind_base"),
        });
    }

    out
}

/// Fallback merge-state blocker for when GitHub reports a non-clean
/// `mergeStateStatus` and *no other axis has emitted a blocker*.
///
/// Class invariant: every non-Clean `mergeStateStatus` must be
/// represented. The modeled axes (CI, reviews, draft/wip,
/// conflict/behind) cover the common reasons GitHub blocks merge.
/// What's left — `Blocked` for unmodeled policy (deployment
/// protection, signed commits, custom rulesets), `HasHooks` for
/// commit hooks pending, `Unknown` not already caught by
/// `Mergeable::Unknown` — would otherwise let `decide()` halt
/// `Success` on a still-unmergeable PR.
///
/// Caller (decide.rs) only invokes this when no other blocker has
/// fired. Otherwise the modeled axis already explains the BLOCKED
/// state and a duplicate emission would shadow the more actionable
/// candidate.
pub fn fallback_merge_state_blocker(state: &PullRequestState) -> Vec<Action> {
    match state.merge_state_status {
        MergeStateStatus::Blocked => vec![Action {
            kind: ActionKind::ResolveMergePolicy,
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            description: "GitHub reports BLOCKED but no modeled axis explains the blockage \
                — likely an unmodeled merge requirement (deployment protection, signed \
                commits, branch ruleset, etc.). Inspect the PR's Merge box on GitHub for \
                the specific gate."
                .into(),
            blocker: BlockerKey::tag("merge_blocked_unmodeled"),
        }],
        MergeStateStatus::HasHooks => vec![Action {
            kind: ActionKind::WaitForMergeability,
            automation: Automation::Wait { interval: Duration::from_secs(30) },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: "Commit hooks are still running — wait and re-observe".into(),
            blocker: BlockerKey::tag("merge_state_has_hooks"),
        }],
        MergeStateStatus::Unknown => vec![Action {
            kind: ActionKind::WaitForMergeability,
            automation: Automation::Wait { interval: Duration::from_secs(30) },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: "GitHub is still computing merge state — wait and re-observe".into(),
            blocker: BlockerKey::tag("merge_state_unknown"),
        }],
        // Clean / Behind / Dirty / Draft / Unstable / HasHooks
        // handled by other axes or non-blocking by definition.
        // (Behind → Rebase via state.behind; Dirty → Rebase via
        // Mergeable::Conflicting; Draft → MarkReady via state.draft;
        // Unstable → advisory CI failure surface, not a hard block.)
        _ => vec![],
    }
}

/// Metadata hygiene — non-blocking but worth fixing. Emitted last
/// so any blocking or advancing work runs first.
pub fn hygiene_candidates(state: &PullRequestState) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    if !state.content_label {
        out.push(Action {
            kind: ActionKind::AddContentLabel,
            automation: Automation::Agent,
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
            description: "Add a content label (bug or enhancement)".into(),
            blocker: BlockerKey::tag("no_content_label"),
        });
    }
    if state.assignees == 0 {
        out.push(Action {
            kind: ActionKind::AddAssignee,
            automation: Automation::Agent,
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
            description: "Assign the PR (default: author)".into(),
            blocker: BlockerKey::tag("no_assignee"),
        });
    }
    // Fire when ANY of {body present, Summary heading, Test plan
    // heading} is missing. orient_state computes all three; emitting
    // only on `!body` ignored two of the three signals.
    if !state.body || !state.summary || !state.test_plan {
        let missing: Vec<&str> = [
            (!state.body).then_some("body"),
            (!state.summary).then_some("Summary"),
            (!state.test_plan).then_some("Test plan"),
        ]
        .into_iter()
        .flatten()
        .collect();
        out.push(Action {
            kind: ActionKind::AddDescription,
            automation: Automation::Agent,
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
            description: format!(
                "PR description missing: {}. Add `## Summary` and `## Test plan` sections.",
                missing.join(", ")
            ),
            blocker: BlockerKey::tag("incomplete_description"),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;

    fn clean() -> PullRequestState {
        PullRequestState {
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
            merge_state_status: MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
        }
    }

    #[test]
    fn clean_state_yields_no_candidates() {
        assert!(blocking_candidates(&clean()).is_empty());
        assert!(hygiene_candidates(&clean()).is_empty());
    }

    #[test]
    fn conflict_emits_rebase() {
        let mut s = clean();
        s.conflict = Mergeable::Conflicting;
        let cs = blocking_candidates(&s);
        assert!(matches!(cs[0].kind, ActionKind::Rebase));
        assert_eq!(cs[0].automation, Automation::Agent);
    }

    #[test]
    fn draft_emits_mark_ready_full_automation() {
        let mut s = clean();
        s.draft = true;
        let cs = blocking_candidates(&s);
        let mark = cs.iter().find(|a| matches!(a.kind, ActionKind::MarkReady));
        assert!(mark.is_some());
        assert_eq!(mark.unwrap().automation, Automation::Full);
    }

    #[test]
    fn missing_metadata_lives_in_hygiene_not_blocking() {
        let mut s = clean();
        s.content_label = false;
        s.assignees = 0;
        s.body = false;
        assert!(blocking_candidates(&s).is_empty());
        let cs = hygiene_candidates(&s);
        for a in &cs {
            assert_eq!(a.target_effect, TargetEffect::Neutral);
        }
        assert_eq!(cs.len(), 3);
    }

    #[test]
    fn fallback_emits_human_handoff_for_blocked_merge_state() {
        // BLOCKED with all modeled axes clean = unmodeled policy
        // gate (deployment protection, signed commits, etc.).
        let mut s = clean();
        s.merge_state_status = MergeStateStatus::Blocked;
        let cs = fallback_merge_state_blocker(&s);
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::ResolveMergePolicy));
        assert_eq!(cs[0].automation, Automation::Human);
        assert_eq!(cs[0].target_effect, TargetEffect::Blocks);
    }

    #[test]
    fn fallback_emits_wait_for_transient_merge_states() {
        // HasHooks and Unknown are transient — wait, don't halt.
        for status in [MergeStateStatus::HasHooks, MergeStateStatus::Unknown] {
            let mut s = clean();
            s.merge_state_status = status;
            let cs = fallback_merge_state_blocker(&s);
            assert_eq!(cs.len(), 1, "expected emit for {status:?}");
            assert!(matches!(cs[0].kind, ActionKind::WaitForMergeability));
            assert!(matches!(cs[0].automation, Automation::Wait { .. }));
        }
    }

    #[test]
    fn fallback_no_op_for_clean_and_handled_merge_states() {
        // Clean is non-blocking. Behind, Dirty, Draft, Unstable
        // are handled by other axes (state.behind, Mergeable
        // ::Conflicting, state.draft, advisory CI). The fallback
        // must NOT double-emit for any of them.
        for status in [
            MergeStateStatus::Clean,
            MergeStateStatus::Behind,
            MergeStateStatus::Dirty,
            MergeStateStatus::Draft,
            MergeStateStatus::Unstable,
        ] {
            let mut s = clean();
            s.merge_state_status = status;
            assert!(
                fallback_merge_state_blocker(&s).is_empty(),
                "fallback must not fire for {status:?}"
            );
        }
    }
}
