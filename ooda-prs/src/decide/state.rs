//! Mechanical merge-shape and merge-policy candidates.
//!
//! Split into two surfaces: a blocking set that must clear before
//! the PR can merge at all, and a fallback for upstream-reported
//! merge-blocked states that no modeled axis explains.

use crate::ids::BlockerKey;

use crate::observe::github::compare::MergeBaseDelta;
use crate::observe::github::pull_request_view::Mergeable;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::{ReviewThread, ThreadState};

use super::action::{Action, ActionEffect, ActionKind, MidTier, NonEmpty, TargetEffect, Urgency};

/// Mechanical merge blockers — every candidate here must clear
/// before the PR can merge at all.
///
/// Declared deps: the merge-shape projection plus the two
/// witness sources used by rebase-prompt enrichment (review
/// threads for re-anchoring; merge-base delta for behind-base
/// guidance). Each is a typed ref; the fn does not consume the
/// whole oriented bundle.
pub(crate) fn blocking_candidates(
    state: &PullRequestProjection,
    threads: &[ReviewThread],
    merge_base_delta: Option<&crate::observe::github::compare::MergeBaseDelta>,
) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    // PR-shape blockers precede mergeability waits: an incomplete
    // PR shape commonly leaves the upstream mergeability signal
    // undetermined, and the shape-fixing actions are what unblock
    // the upstream computation.
    if state.draft {
        out.push(Action {
            kind: ActionKind::MarkReady,
            effect: ActionEffect::Full {
                log: "Mark PR as ready for review".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::Critical),
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
            urgency: Urgency::Mid(MidTier::Critical),
            blocker: BlockerKey::from_static("wip_label"),
        });
    }
    if !state.title_ok {
        out.push(Action {
            kind: ActionKind::ShortenTitle {
                // Upstream caps PR title length well below u32::MAX;
                // the conversion is structurally safe.
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
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("title_too_long"),
        });
    }

    // Mergeability-derived blockers depend on the shape being
    // resolved first; they come after.
    if state.conflict == Mergeable::Unknown {
        out.push(Action {
            kind: ActionKind::WaitForMergeability,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "GitHub is still computing mergeability — wait and re-observe".into(),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingWait),
            blocker: BlockerKey::from_static("mergeability_unknown"),
        });
    } else if state.conflict == Mergeable::Conflicting {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: build_rebase_prompt(
                    "Rebase to resolve merge conflicts",
                    state,
                    threads,
                    merge_base_delta,
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("merge_conflict"),
        });
    } else if state.behind {
        out.push(Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: build_rebase_prompt(
                    "Rebase onto the latest base branch",
                    state,
                    threads,
                    merge_base_delta,
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("behind_base"),
        });
    }

    out
}

/// Build the structured rebase prompt. Composed of an
/// optionally-stack-aware headline, a witnesses section listing
/// open line-anchored review threads (each will need re-anchoring
/// after the rebase moves the hunks), and — when the upstream
/// compare endpoint is available — a merge-base delta section
/// listing files the base touched and any overlap with the branch.
/// An empty overlap is itself a recommendation rather than an
/// omission.
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
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            Witness {
                label,
                body: body.into(),
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

/// Headline that surfaces stack-aware rebase guidance when the PR
/// sits under an unmerged parent. A naive base-trunk rebase would
/// orphan stacked branches; the addendum points at the stack tool
/// that walks the chain.
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

/// Append the merge-base delta sections to a rebase prompt.
/// Empty file overlap is rendered as a positive "clean rebase"
/// recommendation rather than an omission, so the reader is not
/// invited to look for hidden conflicts.
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
        // Empty intersection on a non-empty cross-product is a
        // positive observation (no overlap exists), not a missing
        // measurement.
        prompt.push_paragraph(
            "No file overlap with base since merge-base — clean rebase, just go.".to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;
    use crate::observe::github::pull_request_view::MergeStateStatus;
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
            conversation_resolution_required: false,
            signatures_required: false,
            unsigned_commits: vec![],
            required_approving_review_count: None,
            copilot_review_required: false,
        }
    }

    fn live_thread_with_line(path: &str, line: u32, body: &str, id: &str) -> ReviewThread {
        ReviewThread {
            id: ThreadId::new(id.to_string()).unwrap(),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new(path).unwrap(),
                line: Some(line),
            },
            body: body.into(),
            state: ThreadState::Live,
            originating_comment_id: None,
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
        assert!(blocking_candidates(&clean(), &[], None).is_empty());
    }

    #[test]
    fn conflict_emits_rebase() {
        let mut s = clean();
        s.conflict = Mergeable::Conflicting;
        let cs = blocking_candidates(&s, &[], None);
        assert!(matches!(cs[0].kind, ActionKind::Rebase));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn behind_emits_rebase() {
        let mut s = clean();
        s.behind = true;
        s.merge_state_status = MergeStateStatus::Behind;
        let cs = blocking_candidates(&s, &[], None);
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
        let cs = blocking_candidates(&s, &[], None);
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
        assert!(blocking_candidates(&s, &[], None).is_empty());
    }

    // ─── Rebase prompt enrichment tests ─────────────────────────────

    fn rebase_prompt_for(
        state: &PullRequestProjection,
        threads: &[ReviewThread],
        delta: Option<&MergeBaseDelta>,
    ) -> String {
        let cs = blocking_candidates(state, threads, delta);
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
        let rendered = rebase_prompt_for(&s, &threads, None);
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
        let rendered = rebase_prompt_for(&s, &threads, None);
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
        let rendered = rebase_prompt_for(&s, &[], Some(&delta));
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
        let rendered = rebase_prompt_for(&s, &[], Some(&delta));
        assert!(
            rendered.contains("clean rebase, just go"),
            "missing clean-rebase recommendation: {rendered}"
        );
        // Must NOT spuriously surface a conflict-surface list.
        assert!(!rendered.contains("potential conflict surface"));
    }

    // Merge-state coverage (BLOCKED / HAS_HOOKS / UNKNOWN) plus the
    // BLOCKED-cause drill-down is owned by `decide::merge_eligibility`
    // and pinned by that module's tests. The contract previously held
    // here lifted into the new module's exhaustive case analysis.
}
