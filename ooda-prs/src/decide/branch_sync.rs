//! Branch-sync candidate — out-of-band push on the PR branch.
//!
//! Fires when the sticky head SHA differs from the current remote
//! head: someone (a human, a sibling automation, another OODA
//! invocation) pushed past us. Two classifications, partitioned by
//! the branch's local-tooling profile:
//!
//! - Graphite-tracked branch AND `gt` available ⇒
//!   [`SyncGraphiteStack`] — Full action, run `gt sync` in the repo
//!   root. The convergence path is known; the driver owns it.
//! - Anything else ⇒ [`InvestigatePush`] — Agent handoff. The
//!   prompt names the SHA delta and instructs the agent to
//!   reconcile before re-driving.
//!
//! Classification is by the BRANCH, not the pusher. We do not
//! inspect commit authorship; the only signal is "is this a stack
//! we know how to converge automatically". An untracked branch
//! always routes to handoff regardless of who pushed.
//!
//! Stall-key invariant: the `to_sha` is the gate identity. A
//! second iteration that re-observes the same divergent SHA is
//! the same gate; observing a different `to_sha` is a fresh gate.
//! The [`CohortSha`] newtype carries the stability witness.
//!
//! [`SyncGraphiteStack`]: super::action::ActionKind::SyncGraphiteStack
//! [`InvestigatePush`]: super::action::ActionKind::InvestigatePush
//! [`CohortSha`]: ooda_core::CohortSha

use ooda_core::{CohortSha, HandoffPrompt, NonEmpty, SingleLineString};

use crate::ids::BlockerKey;
use crate::observe::branch::{BranchDivergence, BranchSyncObservation};

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

/// Build candidate actions for the branch-sync axis. Returns 0
/// or 1 action; never more.
#[must_use]
pub(crate) fn candidates(obs: &BranchSyncObservation) -> Vec<Action> {
    let Some(divergence) = obs.divergence.as_ref() else {
        return Vec::new();
    };
    if obs.branch_graphite_tracked && obs.gt_available {
        vec![sync_graphite_stack_action(divergence)]
    } else {
        vec![investigate_push_action(divergence)]
    }
}

fn sync_graphite_stack_action(divergence: &BranchDivergence) -> Action {
    let log = format!(
        "remote head moved {}→{}; gt sync to converge stack",
        short(&divergence.from_sha),
        short(&divergence.to_sha),
    );
    Action {
        kind: ActionKind::SyncGraphiteStack {
            from_sha: divergence.from_sha.clone(),
            to_sha: divergence.to_sha.clone(),
        },
        effect: ActionEffect::Full {
            log,
            // `gt sync` blocks until the local rebase + remote push
            // complete; the next observe pass sees the new branch
            // head without delay.
            upstream: ooda_core::UpstreamConsistency::Sync,
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingFix),
        blocker: BlockerKey::typed("branch_sync_graphite", &CohortSha::new(&divergence.to_sha)),
    }
}

fn investigate_push_action(divergence: &BranchDivergence) -> Action {
    let prompt = build_investigate_push_prompt(divergence);
    Action {
        kind: ActionKind::InvestigatePush {
            from_sha: divergence.from_sha.clone(),
            to_sha: divergence.to_sha.clone(),
        },
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::typed("branch_sync_other", &CohortSha::new(&divergence.to_sha)),
    }
}

fn build_investigate_push_prompt(divergence: &BranchDivergence) -> HandoffPrompt {
    let headline = SingleLineString::from(format!(
        "Remote head changed from {} to {} — investigate before re-driving.",
        short(&divergence.from_sha),
        short(&divergence.to_sha),
    ));
    let mut prompt = HandoffPrompt::new(headline);
    prompt.push_paragraph(
        "The PR branch advanced past what this driver last observed or caused. \
         Reconcile the local tree against the remote before another iteration \
         drives the PR further; an unreviewed push may carry intent the \
         remaining axes cannot infer.",
    );
    prompt.push_heading(2, "Steps");
    prompt.push_numbered_list(
        NonEmpty::try_from_vec(vec![
            SingleLineString::from("Diff your local tree against origin/<branch>."),
            SingleLineString::from(
                "Decide whether `gt sync`, `git reset --hard origin/<branch>`, or a manual reconcile is the right convergence.",
            ),
            SingleLineString::from("Re-invoke /ooda-pr once your local matches the remote."),
        ])
        .expect("three-element literal is non-empty"),
    );
    prompt
}

/// Seven-character abbreviation, defensive against shorter inputs.
fn short(sha: &str) -> &str {
    let end = sha.len().min(7);
    &sha[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_core::MidTier;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn divergence() -> BranchDivergence {
        BranchDivergence {
            from_sha: SHA_A.to_owned(),
            to_sha: SHA_B.to_owned(),
        }
    }

    #[test]
    fn no_divergence_emits_nothing() {
        let cs = candidates(&BranchSyncObservation {
            divergence: None,
            branch_graphite_tracked: true,
            gt_available: true,
        });
        assert!(cs.is_empty());
    }

    #[test]
    fn divergence_plus_graphite_emits_sync_graphite_stack() {
        let cs = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: true,
        });
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::SyncGraphiteStack { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Full { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingFix));
        assert_eq!(cs[0].target_effect, TargetEffect::Blocks);
    }

    #[test]
    fn divergence_without_graphite_emits_investigate_push() {
        let cs = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: false,
            gt_available: true,
        });
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::InvestigatePush { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn graphite_tracked_but_gt_missing_falls_through_to_investigate() {
        // The probe says the branch was tracked by graphite at
        // some point (metadata exists locally) but `gt` is not on
        // PATH; we cannot drive the convergence, so hand off.
        let cs = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: false,
        });
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::InvestigatePush { .. }));
    }

    #[test]
    fn investigate_push_prompt_carries_short_sha_delta() {
        let cs = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: false,
            gt_available: false,
        });
        let ActionEffect::Agent { prompt } = &cs[0].effect else {
            panic!("expected Agent effect");
        };
        let rendered = prompt.to_string();
        assert!(rendered.contains("0123456"), "{rendered}");
        assert!(rendered.contains("fedcba9"), "{rendered}");
    }

    #[test]
    fn sync_graphite_stack_carries_sha_pair_in_payload() {
        let cs = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: true,
        });
        let ActionKind::SyncGraphiteStack { from_sha, to_sha } = &cs[0].kind else {
            panic!("expected SyncGraphiteStack");
        };
        assert_eq!(from_sha, SHA_A);
        assert_eq!(to_sha, SHA_B);
    }

    #[test]
    fn blocker_key_is_cohort_keyed_on_to_sha() {
        // Same to_sha across iterations ⇒ same stall key
        // (graphite branch). Distinct to_sha ⇒ distinct stall key.
        let a = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: true,
        })
        .into_iter()
        .next()
        .unwrap();
        let b = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: true,
        })
        .into_iter()
        .next()
        .unwrap();
        assert_eq!(a.stall_key(), b.stall_key());

        let other_divergence = BranchDivergence {
            from_sha: SHA_A.to_owned(),
            to_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned(),
        };
        let c = candidates(&BranchSyncObservation {
            divergence: Some(other_divergence),
            branch_graphite_tracked: true,
            gt_available: true,
        })
        .into_iter()
        .next()
        .unwrap();
        assert_ne!(a.stall_key(), c.stall_key());
    }

    #[test]
    fn graphite_and_other_paths_have_distinct_blocker_categories() {
        let g = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: true,
            gt_available: true,
        })
        .into_iter()
        .next()
        .unwrap();
        let o = candidates(&BranchSyncObservation {
            divergence: Some(divergence()),
            branch_graphite_tracked: false,
            gt_available: false,
        })
        .into_iter()
        .next()
        .unwrap();
        // Same to_sha cohort, different category — the two paths
        // are recorded distinctly in stall analysis.
        assert!(g.blocker.as_str().starts_with("branch_sync_graphite:"));
        assert!(o.blocker.as_str().starts_with("branch_sync_other:"));
    }
}
