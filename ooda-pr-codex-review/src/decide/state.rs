//! State candidates split into blocking (must clear to merge) and
//! hygiene (non-blocking metadata). decide.rs emits hygiene last so
//! it doesn't shadow review/bot work.

use crate::ids::BlockerKey;

use crate::observe::github::pr_view::{MergeStateStatus, Mergeable};
use crate::orient::state::PullRequestState;

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

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
            effect: ActionEffect::Full {
                log: "Mark PR as ready for review".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Critical,
            blocker: BlockerKey::tag("draft"),
        });
    }
    if state.wip {
        out.push(Action {
            kind: ActionKind::RemoveWipLabel,
            effect: ActionEffect::Full {
                log: "Remove \"work in progress\" label".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Critical,
            blocker: BlockerKey::tag("wip_label"),
        });
    }
    if !state.title_ok {
        out.push(Action {
            kind: ActionKind::ShortenTitle {
                current_len: state.title_len as u32,
            },
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(format!(
                    "Shorten title ({} chars, max 50)",
                    state.title_len
                )),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("title_too_long"),
        });
    }

    // Mergeability-derived blockers come after PR-shape, since
    // mergeability requires the PR to be ready first.
    if state.conflict == Mergeable::Unknown {
        out.push(Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "GitHub is still computing mergeability — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::tag("mergeability_unknown"),
        });
    } else if state.conflict == Mergeable::Conflicting {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(rebase_description(
                    "Rebase to resolve merge conflicts",
                    state,
                )),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("merge_conflict"),
        });
    } else if state.behind {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(rebase_description(
                    "Rebase onto the latest base branch",
                    state,
                )),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("behind_base"),
        });
    }

    out
}

/// Append a stack-aware rebase hint when the PR sits on top of an
/// open parent. A naive `git rebase <trunk>` would orphan stacked
/// branches; `gt restack` walks the chain and rebases every branch.
fn rebase_description(base: &str, state: &PullRequestState) -> String {
    if state.has_open_parent_pr {
        format!(
            "{base}. This PR is stacked under an unmerged parent PR — \
             use `gt sync && gt restack` rather than a direct `git rebase` \
             so the parent branch is rebased first and child branches \
             follow."
        )
    } else {
        base.to_string()
    }
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
            effect: ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new(
                    "GitHub reports BLOCKED but no modeled axis explains the blockage \
                     — likely an unmodeled merge requirement (deployment protection, signed \
                     commits, branch ruleset, etc.). Inspect the PR's Merge box on GitHub for \
                     the specific gate.",
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag("merge_blocked_unmodeled"),
        }],
        MergeStateStatus::HasHooks => vec![Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "Commit hooks are still running — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::tag("merge_state_has_hooks"),
        }],
        MergeStateStatus::Unknown => vec![Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "GitHub is still computing merge state — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
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
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Add a content label (bug or enhancement)"),
            },
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
            blocker: BlockerKey::tag("no_content_label"),
        });
    }
    if state.assignees == 0 {
        out.push(Action {
            kind: ActionKind::AddAssignee,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Assign the PR (default: author)"),
            },
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
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
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(format!(
                    "PR description missing: {}. Add `## Summary` and `## Test plan` sections.",
                    missing.join(", ")
                )),
            },
            target_effect: TargetEffect::Neutral,
            urgency: Urgency::Hygiene,
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
            has_open_parent_pr: false,
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
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn draft_emits_mark_ready_full_automation() {
        let mut s = clean();
        s.draft = true;
        let cs = blocking_candidates(&s);
        let mark = cs.iter().find(|a| matches!(a.kind, ActionKind::MarkReady));
        assert!(mark.is_some());
        assert!(matches!(mark.unwrap().effect, ActionEffect::Full { .. }));
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

    // ─── property tests for the class invariant ─────────────────────
    //
    // Class invariant from `fallback_merge_state_blocker`'s docs:
    // "Every non-Clean `mergeStateStatus` must be represented" —
    // either by another axis (Behind/Dirty/Draft/Unstable) or by the
    // fallback itself (Blocked/HasHooks/Unknown). Clean is the only
    // status that produces no candidate.
    //
    // The exhaustive match in `expected_fallback_behavior` is the
    // contract. Adding a new `MergeStateStatus` variant fails to
    // compile here until the new arm is added, which forces an
    // explicit decision about which axis handles it. The sample
    // list is length-sentineled so a forgotten sample also fails
    // loudly.

    /// What `fallback_merge_state_blocker` is contracted to emit for
    /// a given `MergeStateStatus`. The two `Empty` cases are
    /// distinguished only by intent (commented inline) — both must
    /// produce zero candidates.
    #[derive(Debug, PartialEq, Eq)]
    enum FallbackBehavior {
        /// Either Clean (mergeable, no blocker) or handled by another
        /// axis (state.behind → Rebase; state.conflict → Rebase;
        /// state.draft → MarkReady; Unstable → advisory CI surface).
        Empty,
        /// `Blocked` — unmodeled merge policy (deployment protection,
        /// signed commits, custom ruleset). Hand off to a human.
        EmitHumanResolveMergePolicy,
        /// Transient — wait and re-observe.
        EmitWaitForMergeability,
    }

    /// Exhaustive over `MergeStateStatus`. The compiler enforces
    /// that every variant has an explicit axis assignment.
    fn expected_fallback_behavior(status: MergeStateStatus) -> FallbackBehavior {
        match status {
            // Clean: mergeable, no candidate needed.
            MergeStateStatus::Clean => FallbackBehavior::Empty,
            // Behind / Dirty / Draft: another axis fires
            // (state.behind, Mergeable::Conflicting, state.draft).
            MergeStateStatus::Behind => FallbackBehavior::Empty,
            MergeStateStatus::Dirty => FallbackBehavior::Empty,
            MergeStateStatus::Draft => FallbackBehavior::Empty,
            // Unstable: advisory CI surface, not a hard block.
            MergeStateStatus::Unstable => FallbackBehavior::Empty,
            // Blocked: unmodeled merge requirement → human triage.
            MergeStateStatus::Blocked => FallbackBehavior::EmitHumanResolveMergePolicy,
            // Transient states: wait for GitHub to finish computing.
            MergeStateStatus::HasHooks => FallbackBehavior::EmitWaitForMergeability,
            MergeStateStatus::Unknown => FallbackBehavior::EmitWaitForMergeability,
        }
    }

    fn all_merge_state_statuses() -> Vec<MergeStateStatus> {
        vec![
            MergeStateStatus::Behind,
            MergeStateStatus::Blocked,
            MergeStateStatus::Clean,
            MergeStateStatus::Dirty,
            MergeStateStatus::Draft,
            MergeStateStatus::HasHooks,
            MergeStateStatus::Unstable,
            MergeStateStatus::Unknown,
        ]
    }

    fn observed_fallback_behavior(state: &PullRequestState) -> FallbackBehavior {
        let cs = fallback_merge_state_blocker(state);
        match cs.as_slice() {
            [] => FallbackBehavior::Empty,
            [a] => match (&a.kind, &a.effect) {
                (ActionKind::ResolveMergePolicy, ActionEffect::Human { .. }) => {
                    FallbackBehavior::EmitHumanResolveMergePolicy
                }
                (ActionKind::WaitForMergeability, ActionEffect::Wait { .. }) => {
                    FallbackBehavior::EmitWaitForMergeability
                }
                (kind, effect) => {
                    panic!("fallback emitted unexpected (kind, effect): {kind:?}, {effect:?}",)
                }
            },
            multi => panic!(
                "fallback emitted {} candidates; expected 0 or 1",
                multi.len()
            ),
        }
    }

    #[test]
    fn fallback_property_holds_for_every_merge_state_status() {
        let statuses = all_merge_state_statuses();
        assert_eq!(
            statuses.len(),
            8,
            "`all_merge_state_statuses` must include one sample per \
             `MergeStateStatus` variant; adding a new variant requires \
             adding both an arm in `expected_fallback_behavior` AND a \
             sample here.",
        );
        for status in statuses {
            let mut s = clean();
            s.merge_state_status = status;
            let actual = observed_fallback_behavior(&s);
            let expected = expected_fallback_behavior(status);
            assert_eq!(
                actual, expected,
                "fallback_merge_state_blocker contract violated for {status:?}",
            );
        }
    }
}
