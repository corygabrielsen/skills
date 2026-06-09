//! Signing-eligibility closure check.
//!
//! Asserts that "if no axis emitted a candidate for signing" coincides
//! with "GitHub will accept every commit on this PR as verified."
//! Without this check, OODA's verdict-by-absence would project a
//! still-unmergeable PR into `Decision::Halt(Success)` whenever the
//! branch policy requires signed commits but none of the existing
//! axes surface the unsigned-commit gate.
//!
//! # Invariants
//!
//! - **Repo-aware gating**: the axis fires only when the branch
//!   actually requires signed commits (`state.signatures_required`).
//!   Most branches don't; this axis is silent for them — no false
//!   positives, no false halts.
//! - **At most one candidate per call**: a single `HandoffHuman` with
//!   the unsigned SHAs travelling on the prompt body. Per-SHA
//!   actions would multiply iteration cost without unlocking
//!   different remediation.
//! - **Pathology urgency**: the emission is `Mid(Pathology)` so it
//!   strictly outranks any `BlockingWait` that might fire on the
//!   same iteration — automation cannot rebase-and-sign on a
//!   shared branch, the human must own the recovery.

use crate::ids::{BlockerKey, GitCommitSha};
use crate::observe::github::pull_request_view::MergeStateStatus;
use crate::orient::state::PullRequestProjection;

use super::action::{Action, ActionEffect, ActionKind, MidTier, NonEmpty, TargetEffect, Urgency};

/// Compute the signing-eligibility candidate set.
///
/// Returns at most one candidate. Empty result attests one of:
/// branch does not require signed commits, host considers the merge
/// gate satisfied (`Clean` / `Unstable` / `HasHooks` / `Behind` /
/// `Dirty` / `Draft` / `Unknown`), or every commit on HEAD is
/// verified.
///
/// Closure-check semantic: attest the upstream gate, do not predict
/// it. On squash-merge repos with `required_signatures` enabled, the
/// squash commit is host-signed (web-flow) and the gate is satisfied
/// even when branch commits are unsigned — `merge_state_status`
/// stays `Clean`. Firing on those repos would be a false positive;
/// the `Blocked` precondition eliminates it by construction.
pub(crate) fn signing_eligibility_candidates(state: &PullRequestProjection) -> Vec<Action> {
    if !state.signatures_required {
        return vec![];
    }
    if !matches!(state.merge_state_status, MergeStateStatus::Blocked) {
        return vec![];
    }
    let Some(unsigned) = NonEmpty::try_from_vec(state.unsigned_commits.clone()) else {
        return vec![];
    };
    vec![signing_blocked_unverified(unsigned)]
}

fn signing_blocked_unverified(unsigned: NonEmpty<GitCommitSha>) -> Action {
    use ooda_core::{HandoffPrompt, SingleLineString};
    let mut prompt = HandoffPrompt::new(
        "GitHub branch policy requires every commit on this PR to be \
         verified-signed; one or more commits on HEAD do not satisfy \
         that gate. Automation cannot rebase-and-sign on a shared \
         branch — the human owns the recovery. Recipe: fetch the \
         branch into a main worktree with `commit.gpgsign=true` \
         active, `git rebase --force-rebase <base>` to re-sign every \
         commit, then `git push --force-with-lease`.",
    );
    prompt.push_paragraph("Unsigned commits on HEAD:".to_string());
    prompt.push_numbered_list(
        NonEmpty::try_from_vec(
            unsigned
                .iter()
                .map(|sha| SingleLineString::new(sha.as_str().to_string()))
                .collect::<Vec<_>>(),
        )
        .expect("non-empty input yields non-empty list"),
    );
    Action {
        kind: ActionKind::EscalateSigningRequired {
            unsigned_commits: unsigned,
        },
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        // Pathology tier — verdict-by-absence is unsound when the
        // signal stream itself is broken (commits exist but none
        // are signed). Must strictly outrank Wait actions firing
        // concurrently on other axes.
        urgency: Urgency::Mid(MidTier::Pathology),
        // Gate identity: "≥1 unsigned commit on HEAD." Per-SHA
        // details travel on the payload — embedding them in the
        // key would violate gate stability across iterations.
        blocker: BlockerKey::from_static("signing_blocked_unverified"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;
    use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};

    fn sha(hex: char) -> GitCommitSha {
        GitCommitSha::parse(&hex.to_string().repeat(40)).unwrap()
    }

    fn projection_with(
        signatures_required: bool,
        unsigned_commits: Vec<GitCommitSha>,
    ) -> PullRequestProjection {
        PullRequestProjection {
            conflict: Mergeable::Mergeable,
            draft: false,
            wip: false,
            title_len: 10,
            title_ok: true,
            body: true,
            summary: true,
            test_plan: true,
            content_label: false,
            assignees: 1,
            reviewers: 1,
            merge_when_ready: false,
            commits: unsigned_commits.len().max(1),
            behind: false,
            has_open_parent_pr: false,
            merge_state_status: MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-06-08T00:00:00Z").unwrap(),
            last_commit_at: None,
            active_branch_rule_types: vec![],
            required_check_names_per_ruleset: vec![],
            missing_required_check_names_on_head: vec![],
            conversation_resolution_required: false,
            signatures_required,
            unsigned_commits,
            required_approving_review_count: None,
            copilot_review_required: false,
        }
    }

    #[test]
    fn silent_when_signing_not_required() {
        let s = projection_with(false, vec![sha('a')]);
        assert!(signing_eligibility_candidates(&s).is_empty());
    }

    fn projection_blocked(unsigned_commits: Vec<GitCommitSha>) -> PullRequestProjection {
        let mut s = projection_with(true, unsigned_commits);
        s.merge_state_status = MergeStateStatus::Blocked;
        s
    }

    #[test]
    fn silent_when_signing_required_but_all_verified() {
        let s = projection_blocked(vec![]);
        assert!(signing_eligibility_candidates(&s).is_empty());
    }

    #[test]
    fn silent_when_unsigned_but_merge_state_is_clean() {
        // Squash-merge repo with required_signatures: GitHub
        // web-flow-signs the squash commit; branch commits don't
        // gate the merge. mergeStateStatus stays CLEAN even with
        // unsigned branch commits — axis must stay silent.
        let mut s = projection_with(true, vec![sha('a')]);
        s.merge_state_status = MergeStateStatus::Clean;
        assert!(signing_eligibility_candidates(&s).is_empty());
    }

    #[test]
    fn silent_when_merge_state_unstable_or_hooks_or_behind() {
        // CLEAN / UNSTABLE / HAS_HOOKS / BEHIND all mean GitHub
        // does not currently block the merge on signing; the
        // axis attests the upstream gate rather than predicting
        // it, so it stays silent.
        for status in [
            MergeStateStatus::Clean,
            MergeStateStatus::Unstable,
            MergeStateStatus::HasHooks,
            MergeStateStatus::Behind,
            MergeStateStatus::Dirty,
            MergeStateStatus::Draft,
            MergeStateStatus::Unknown,
        ] {
            let mut s = projection_with(true, vec![sha('a')]);
            s.merge_state_status = status;
            assert!(
                signing_eligibility_candidates(&s).is_empty(),
                "must stay silent on mergeStateStatus = {status:?}"
            );
        }
    }

    #[test]
    fn fires_pathology_when_blocked_and_unsigned() {
        let s = projection_blocked(vec![sha('a')]);
        let cs = signing_eligibility_candidates(&s);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Pathology));
        assert_eq!(cs[0].blocker.as_str(), "signing_blocked_unverified");
        assert!(matches!(cs[0].effect, ActionEffect::Human { .. }));
        assert!(matches!(
            cs[0].kind,
            ActionKind::EscalateSigningRequired { .. }
        ));
    }

    #[test]
    fn payload_carries_every_unsigned_sha() {
        let s = projection_blocked(vec![sha('a'), sha('b'), sha('c')]);
        let cs = signing_eligibility_candidates(&s);
        let body = cs[0].rendered_payload();
        assert!(body.contains(&"a".repeat(40)));
        assert!(body.contains(&"b".repeat(40)));
        assert!(body.contains(&"c".repeat(40)));
    }

    #[test]
    fn prompt_names_recovery_recipe() {
        let s = projection_blocked(vec![sha('a')]);
        let body = cs(&s);
        assert!(body.contains("rebase"));
        assert!(body.contains("push --force-with-lease"));
    }

    #[test]
    fn blocker_key_is_stable_across_calls() {
        let s1 = projection_blocked(vec![sha('a')]);
        let s2 = projection_blocked(vec![sha('a'), sha('b'), sha('c')]);
        assert_eq!(
            signing_eligibility_candidates(&s1)[0].blocker.as_str(),
            signing_eligibility_candidates(&s2)[0].blocker.as_str(),
        );
    }

    #[test]
    fn pathology_strictly_outranks_blocking_wait() {
        // Sanity check the urgency invariant at the candidate level:
        // a hypothetical concurrent WaitForCi at BlockingWait must
        // be outranked by this axis's Pathology emission.
        let s = projection_blocked(vec![sha('a')]);
        let pathology = signing_eligibility_candidates(&s)[0].urgency;
        assert!(pathology < Urgency::Mid(MidTier::BlockingWait));
        assert!(pathology < Urgency::Mid(MidTier::BlockingHuman));
    }

    fn cs(state: &PullRequestProjection) -> String {
        signing_eligibility_candidates(state)[0].rendered_payload()
    }
}
