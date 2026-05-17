//! State candidates split into blocking (must clear to merge) and
//! hygiene (non-blocking metadata). decide.rs emits hygiene last so
//! it doesn't shadow review/bot work.

use crate::ids::BlockerKey;

use crate::observe::github::compare::MergeBaseDelta;
use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
use crate::orient::OrientedState;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::{ReviewThread, ThreadState};

use super::action::{Action, ActionEffect, ActionKind, NonEmpty, TargetEffect, Urgency};

// Widened from `&PullRequestProjection` to `&OrientedState` so the Rebase
// emission can reach `oriented.threads` (orphan-risk witnesses) and
// any future compare-endpoint facts. Other action prompts can now
// enrich themselves from the same wider input without further
// signature churn.
/// Mechanical merge blockers — must clear for the PR to be
/// mergeable at all. Emitted by decide before review/bot axes.
pub(super) fn blocking_candidates(oriented: &OrientedState) -> Vec<Action> {
    let state = &oriented.state;
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
            blocker: BlockerKey::from_static("draft"),
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
            blocker: BlockerKey::from_static("wip_label"),
        });
    }
    if !state.title_ok {
        out.push(Action {
            kind: ActionKind::ShortenTitle {
                // PR title byte-length fits in u32: GitHub caps titles
                // at 256 chars; usize → u32 is structurally safe.
                current_len: u32::try_from(state.title_len)
                    .expect("PR title byte-length fits in u32"),
            },
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(format!(
                    "Shorten title ({} chars, max 50)",
                    state.title_len
                )),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::from_static("title_too_long"),
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
            blocker: BlockerKey::from_static("mergeability_unknown"),
        });
    } else if state.conflict == Mergeable::Conflicting {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: build_rebase_prompt(
                    "Rebase to resolve merge conflicts",
                    state,
                    &oriented.threads,
                    oriented.merge_base_delta.as_ref(),
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::from_static("merge_conflict"),
        });
    } else if state.behind {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: build_rebase_prompt(
                    "Rebase onto the latest base branch",
                    state,
                    &oriented.threads,
                    oriented.merge_base_delta.as_ref(),
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::from_static("behind_base"),
        });
    }

    out
}

/// Build the structured Rebase prompt. Combines:
///
/// * Headline with the stack-aware addendum when this PR is stacked
///   under an open parent.
/// * `Witnesses` listing open review threads with line anchors —
///   each will need re-anchoring once the rebase moves the hunks.
///   Omitted entirely when no live, line-anchored threads exist.
/// * `Paragraph` with the merge-base delta (commits behind, oldest
///   master commit) and a `NumberedList` of files master touched
///   since the merge base — when known.
/// * `Paragraph` + optional `NumberedList` reporting the file
///   intersection between master and the branch. An empty
///   intersection is itself a recommendation: no overlap, no
///   conflict surface, just rebase.
fn build_rebase_prompt(
    base: &str,
    state: &PullRequestProjection,
    threads: &[ReviewThread],
    delta: Option<&MergeBaseDelta>,
) -> ooda_core::HandoffPrompt {
    use ooda_core::{HandoffPrompt, SingleLineString, Witness};

    let mut prompt = HandoffPrompt::new(rebase_headline(base, state));

    let live: Vec<&ReviewThread> = threads
        .iter()
        .filter(|t| t.state == ThreadState::Live && t.location.line.is_some())
        .collect();
    if let Some(live) = NonEmpty::try_from_vec(live) {
        prompt.push_paragraph(
            "Open review threads (will need re-anchoring after rebase):".to_string(),
        );
        let witnesses = live.map_ref(|t| {
            let label = SingleLineString::new(format!(
                "{} — {} (thread_id: {})",
                t.location, t.author, t.id,
            ));
            let body = t
                .body
                .lines()
                .map(|line| format!("   > {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            Witness {
                label,
                body,
                url: None,
            }
        });
        prompt.push_witnesses(witnesses);
    }

    if let Some(delta) = delta {
        push_merge_base_delta_sections(&mut prompt, delta);
    }

    prompt
}

/// Stack-aware rebase headline. A naive `git rebase <trunk>` would
/// orphan stacked branches; `gt restack` walks the chain and rebases
/// every branch.
fn rebase_headline(base: &str, state: &PullRequestProjection) -> String {
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

/// Append the compare-endpoint sections (commits behind, files
/// touched, conflict surface) to a Rebase prompt. An empty
/// `conflict_surface` is itself the recommendation — the prompt
/// surfaces "clean rebase, just go" so the human doesn't second-
/// guess and dig through the diff.
fn push_merge_base_delta_sections(prompt: &mut ooda_core::HandoffPrompt, delta: &MergeBaseDelta) {
    use ooda_core::SingleLineString;

    if delta.commits_behind == 0 && delta.master_files.is_empty() {
        return;
    }

    let oldest = delta
        .oldest_master_commit_at
        .as_ref()
        .map(|t| format!(" (oldest: {t})"))
        .unwrap_or_default();
    prompt.push_paragraph(format!(
        "Behind base by {} since merge-base{oldest}.",
        crate::text::count(delta.commits_behind as usize, "commit"),
    ));

    if let Some(master_files) = NonEmpty::try_from_vec(
        delta
            .master_files
            .iter()
            .map(|p| SingleLineString::new(p.clone()))
            .collect(),
    ) {
        prompt.push_paragraph(format!(
            "Base touched {} since merge-base:",
            crate::text::count(delta.master_files.len(), "file"),
        ));
        prompt.push_numbered_list(master_files);
    }

    if let Some(conflict_surface) = NonEmpty::try_from_vec(
        delta
            .conflict_surface
            .iter()
            .map(|p| SingleLineString::new(p.clone()))
            .collect(),
    ) {
        prompt.push_paragraph(format!(
            "Of those, {} overlap with this branch (potential conflict surface):",
            crate::text::count(delta.conflict_surface.len(), "file"),
        ));
        prompt.push_numbered_list(conflict_surface);
    } else if !delta.master_files.is_empty() && !delta.branch_files.is_empty() {
        // Empty intersection on a non-empty cross-product is a real
        // signal, not "we didn't observe" — surface it as a positive
        // recommendation so the human doesn't second-guess.
        prompt.push_paragraph(
            "No file overlap with base since merge-base — clean rebase, just go.".to_string(),
        );
    }
}

/// Compose the human-facing prompt for `merge_blocked_unmodeled`.
/// Headline is the generic "GitHub reports BLOCKED" prose; the three
/// optional sections (active ruleset rules, required check names,
/// missing-on-HEAD) are appended only when their backing projection
/// field is non-empty. When all three are empty the prompt is the
/// generic headline alone.
fn merge_blocked_unmodeled_prompt(state: &PullRequestProjection) -> ooda_core::HandoffPrompt {
    use ooda_core::{HandoffPrompt, NonEmpty, SingleLineString};

    let mut prompt = HandoffPrompt::new(
        "GitHub reports BLOCKED but no modeled axis explains the blockage \
         — likely an unmodeled merge requirement (deployment protection, signed \
         commits, branch ruleset, etc.). Inspect the PR's Merge box on GitHub for \
         the specific gate.",
    );

    if let Some(rule_types) = NonEmpty::try_from_vec(
        state
            .active_branch_rule_types
            .iter()
            .map(|s| SingleLineString::new(s.clone()))
            .collect(),
    ) {
        prompt.push_paragraph("Active ruleset rules on this branch:".to_string());
        prompt.push_numbered_list(rule_types);
    }

    if !state.required_check_names_per_ruleset.is_empty() {
        prompt.push_paragraph(format!(
            "Required check names from ruleset: {}",
            state.required_check_names_per_ruleset.join(", "),
        ));
    }

    if !state.missing_required_check_names_on_head.is_empty() {
        prompt.push_paragraph(format!(
            "Missing on HEAD: {}",
            state.missing_required_check_names_on_head.join(", "),
        ));
    }

    prompt
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
pub(super) fn fallback_merge_state_blocker(state: &PullRequestProjection) -> Vec<Action> {
    match state.merge_state_status {
        MergeStateStatus::Blocked => vec![Action {
            kind: ActionKind::ResolveMergePolicy,
            effect: ActionEffect::Human {
                prompt: merge_blocked_unmodeled_prompt(state),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::from_static("merge_blocked_unmodeled"),
        }],
        MergeStateStatus::HasHooks => vec![Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "Commit hooks are still running — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::from_static("merge_state_has_hooks"),
        }],
        MergeStateStatus::Unknown => vec![Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "GitHub is still computing merge state — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::from_static("merge_state_unknown"),
        }],
        // Clean / Behind / Dirty / Draft / Unstable / HasHooks
        // handled by other axes or non-blocking by definition.
        // (Behind → Rebase via state.behind; Dirty → Rebase via
        // Mergeable::Conflicting; Draft → MarkReady via state.draft;
        // Unstable → advisory CI failure surface, not a hard block.)
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::thread::{BotName, FilePath, ThreadAuthor, ThreadId, ThreadLocation};

    fn clean() -> PullRequestProjection {
        PullRequestProjection {
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
            active_branch_rule_types: vec![],
            required_check_names_per_ruleset: vec![],
            missing_required_check_names_on_head: vec![],
        }
    }

    fn clean_ci() -> CiReport {
        CiReport {
            summary: CiSummary {
                required: CheckBucket::default(),
                missing_names: vec![],
                completed_at: None,
                advisory: CheckBucket::default(),
            },
            activity: CiActivity::Resolved(ResolvedState::AllGreen),
        }
    }

    fn clean_reviews() -> ReviewSummary {
        ReviewSummary {
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
        }
    }

    fn oriented(state: PullRequestProjection) -> OrientedState {
        OrientedState {
            ci: clean_ci(),
            state,
            reviews: clean_reviews(),
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata:
                crate::orient::pull_request_metadata::PullRequestMetadata::NeverAttested,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::NeverAttested,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
        }
    }

    fn oriented_with(
        state: PullRequestProjection,
        threads: Vec<ReviewThread>,
        delta: Option<MergeBaseDelta>,
    ) -> OrientedState {
        OrientedState {
            ci: clean_ci(),
            state,
            reviews: clean_reviews(),
            copilot: None,
            cursor: None,
            threads,
            merge_base_delta: delta,
            pull_request_metadata:
                crate::orient::pull_request_metadata::PullRequestMetadata::NeverAttested,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::NeverAttested,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
        }
    }

    fn live_thread_with_line(path: &str, line: u32, body: &str, id: &str) -> ReviewThread {
        ReviewThread {
            id: ThreadId::new(id.to_string()),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new(path),
                line: Some(line),
            },
            body: body.into(),
            state: ThreadState::Live,
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
        }
    }

    fn resolved_thread_with_line(path: &str, line: u32, body: &str, id: &str) -> ReviewThread {
        let mut t = live_thread_with_line(path, line, body, id);
        t.state = ThreadState::Resolved;
        t
    }

    #[test]
    fn clean_state_yields_no_candidates() {
        assert!(blocking_candidates(&oriented(clean())).is_empty());
    }

    #[test]
    fn conflict_emits_rebase() {
        let mut s = clean();
        s.conflict = Mergeable::Conflicting;
        let cs = blocking_candidates(&oriented(s));
        assert!(matches!(cs[0].kind, ActionKind::Rebase));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn behind_emits_rebase() {
        let mut s = clean();
        s.behind = true;
        s.merge_state_status = MergeStateStatus::Behind;
        let cs = blocking_candidates(&oriented(s));
        let rebase = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::Rebase))
            .expect("Behind state must emit Rebase");
        assert!(matches!(rebase.effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn draft_emits_mark_ready_full_automation() {
        let mut s = clean();
        s.draft = true;
        let cs = blocking_candidates(&oriented(s));
        let mark = cs.iter().find(|a| matches!(a.kind, ActionKind::MarkReady));
        assert!(mark.is_some());
        assert!(matches!(mark.unwrap().effect, ActionEffect::Full { .. }));
    }

    #[test]
    fn missing_metadata_does_not_block() {
        let mut s = clean();
        s.content_label = false;
        s.assignees = 0;
        s.body = false;
        assert!(blocking_candidates(&oriented(s)).is_empty());
    }

    // ─── Rebase prompt enrichment tests ─────────────────────────────

    fn rebase_prompt_for(oriented: &OrientedState) -> String {
        let cs = blocking_candidates(oriented);
        let rebase = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::Rebase))
            .expect("expected Rebase candidate");
        rebase.rendered_payload()
    }

    #[test]
    fn rebase_prompt_includes_witnesses_when_live_threads_present() {
        let mut s = clean();
        s.conflict = Mergeable::Conflicting;
        let threads = vec![
            live_thread_with_line("src/foo.rs", 42, "use ? not unwrap", "T1"),
            live_thread_with_line("src/bar.rs", 108, "off-by-one", "T2"),
        ];
        let rendered = rebase_prompt_for(&oriented_with(s, threads, None));
        assert!(
            rendered.contains("Open review threads"),
            "headline paragraph missing: {rendered}"
        );
        assert!(rendered.contains("src/foo.rs:42"), "anchor 1: {rendered}");
        assert!(rendered.contains("src/bar.rs:108"), "anchor 2: {rendered}");
        assert!(rendered.contains("> use ? not unwrap"));
        assert!(rendered.contains("thread_id: T1"));
    }

    #[test]
    fn rebase_prompt_omits_witnesses_section_when_no_live_threads() {
        let mut s = clean();
        s.conflict = Mergeable::Conflicting;
        // All resolved → no witnesses.
        let threads = vec![resolved_thread_with_line(
            "src/foo.rs",
            1,
            "already addressed",
            "T_r",
        )];
        let rendered = rebase_prompt_for(&oriented_with(s, threads, None));
        assert!(
            !rendered.contains("Open review threads"),
            "must not emit the witnesses preamble when none are live: {rendered}"
        );
        assert!(!rendered.contains("thread_id"));
    }

    #[test]
    fn rebase_prompt_includes_conflict_surface_when_overlap_present() {
        let mut s = clean();
        s.behind = true;
        s.merge_state_status = MergeStateStatus::Behind;
        let delta = MergeBaseDelta {
            commits_behind: 5,
            commits_ahead: 2,
            master_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            branch_files: vec!["src/b.rs".into(), "src/c.rs".into()],
            conflict_surface: vec!["src/b.rs".into()],
            oldest_master_commit_at: Some(Timestamp::parse("2026-05-10T09:00:00Z").unwrap()),
        };
        let rendered = rebase_prompt_for(&oriented_with(s, vec![], Some(delta)));
        assert!(
            rendered.contains("Behind base by 5 commits"),
            "commits-behind headline missing: {rendered}"
        );
        assert!(rendered.contains("2026-05-10"), "oldest ts missing");
        assert!(rendered.contains("1. src/a.rs"));
        assert!(rendered.contains("2. src/b.rs"));
        assert!(rendered.contains("potential conflict surface"));
        assert!(rendered.contains("1. src/b.rs"));
    }

    #[test]
    fn rebase_prompt_says_clean_rebase_when_intersection_empty() {
        let mut s = clean();
        s.behind = true;
        s.merge_state_status = MergeStateStatus::Behind;
        let delta = MergeBaseDelta {
            commits_behind: 4,
            commits_ahead: 2,
            master_files: vec!["docs/readme.md".into()],
            branch_files: vec!["src/lib.rs".into()],
            conflict_surface: vec![],
            oldest_master_commit_at: Some(Timestamp::parse("2026-05-12T11:00:00Z").unwrap()),
        };
        let rendered = rebase_prompt_for(&oriented_with(s, vec![], Some(delta)));
        assert!(
            rendered.contains("clean rebase, just go"),
            "missing clean-rebase recommendation: {rendered}"
        );
        // Must NOT spuriously surface a conflict-surface list.
        assert!(!rendered.contains("potential conflict surface"));
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
        /// state.draft → `MarkReady`; Unstable → advisory CI surface).
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
        // Intentional exhaustive match per axis pattern; arms are
        // duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
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

    fn observed_fallback_behavior(state: &PullRequestProjection) -> FallbackBehavior {
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
                    panic!("fallback emitted unexpected (kind, effect): {kind:?}, {effect:?}")
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
            // Enrichment fields populated; behavior contract must
            // hold regardless of their content.
            s.active_branch_rule_types = vec!["required_status_checks".into()];
            s.required_check_names_per_ruleset = vec!["Mergeability Check".into()];
            s.missing_required_check_names_on_head = vec!["Mergeability Check".into()];
            let actual = observed_fallback_behavior(&s);
            let expected = expected_fallback_behavior(status);
            assert_eq!(
                actual, expected,
                "fallback_merge_state_blocker contract violated for {status:?}",
            );
        }
    }

    fn blocked_with(
        active: Vec<String>,
        required: Vec<String>,
        missing: Vec<String>,
    ) -> PullRequestProjection {
        let mut s = clean();
        s.merge_state_status = MergeStateStatus::Blocked;
        s.active_branch_rule_types = active;
        s.required_check_names_per_ruleset = required;
        s.missing_required_check_names_on_head = missing;
        s
    }

    fn rendered_blocked_prompt(state: &PullRequestProjection) -> String {
        let cs = fallback_merge_state_blocker(state);
        assert_eq!(cs.len(), 1, "Blocked must emit exactly one candidate");
        cs[0].rendered_payload()
    }

    #[test]
    fn fallback_includes_active_rule_types_when_present() {
        let s = blocked_with(
            vec!["copilot_code_review".into(), "required_signatures".into()],
            vec![],
            vec![],
        );
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            rendered.contains("Active ruleset rules on this branch"),
            "header missing: {rendered}",
        );
        assert!(rendered.contains("1. copilot_code_review"));
        assert!(rendered.contains("2. required_signatures"));
    }

    #[test]
    fn fallback_omits_active_rules_section_when_empty() {
        let s = blocked_with(vec![], vec![], vec![]);
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            !rendered.contains("Active ruleset rules"),
            "must not emit ruleset header when empty: {rendered}",
        );
    }

    #[test]
    fn fallback_includes_required_check_names_when_present() {
        let s = blocked_with(
            vec!["required_status_checks".into()],
            vec!["Mergeability Check".into(), "Build".into()],
            vec![],
        );
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            rendered.contains("Required check names from ruleset: Mergeability Check, Build"),
            "required-names line missing or malformed: {rendered}",
        );
    }

    #[test]
    fn fallback_omits_required_check_names_when_empty() {
        let s = blocked_with(vec!["required_signatures".into()], vec![], vec![]);
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            !rendered.contains("Required check names"),
            "must not emit required-names line when empty: {rendered}",
        );
    }

    #[test]
    fn fallback_includes_missing_on_head_when_present() {
        let s = blocked_with(
            vec!["required_status_checks".into()],
            vec!["Mergeability Check".into(), "Build".into()],
            vec!["Build".into()],
        );
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            rendered.contains("Missing on HEAD: Build"),
            "missing-on-HEAD line missing or malformed: {rendered}",
        );
    }

    #[test]
    fn fallback_omits_missing_on_head_when_empty() {
        let s = blocked_with(
            vec!["required_status_checks".into()],
            vec!["Mergeability Check".into()],
            vec![],
        );
        let rendered = rendered_blocked_prompt(&s);
        assert!(
            !rendered.contains("Missing on HEAD"),
            "must not emit missing-on-HEAD line when empty: {rendered}",
        );
    }

    #[test]
    fn fallback_emits_generic_prompt_only_when_all_enrichment_empty() {
        let s = blocked_with(vec![], vec![], vec![]);
        let rendered = rendered_blocked_prompt(&s);
        assert!(rendered.contains("GitHub reports BLOCKED"));
        assert!(!rendered.contains("Active ruleset rules"));
        assert!(!rendered.contains("Required check names"));
        assert!(!rendered.contains("Missing on HEAD"));
    }
}
