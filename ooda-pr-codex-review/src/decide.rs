//! Decide stage: per-axis candidate generation.
//!
//! Each submodule below is a free `candidates(...)` function
//! taking typed dep refs and emitting a `Vec<Action>` for one
//! axis. Aggregation across axes is the Driver's job
//! (see [`crate::runner::drive`]) — there is no top-level
//! aggregator here.
//!
//! This binary's `decide` includes a codex-review axis (`mod
//! codex_review`) that other PR-side binaries do not.
//!
//! Halt is a predicate over the aggregated candidate set, not a
//! scalar threshold. Empty set ⇒ Success; top candidate requires
//! an external resolver ⇒ handoff; otherwise Execute. There is
//! no aggregate score gating the loop.

pub(crate) mod action;
pub(crate) mod branch_sync;
pub(crate) mod ci;
pub(crate) mod claude_review;
pub(crate) mod closeout;
pub(crate) mod codex_review;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod decision;
pub(crate) mod doc_review;
pub(crate) mod pull_request_metadata;
pub(crate) mod reviews;
pub(crate) mod state;

#[cfg(test)]
mod tests {
    use crate::decide::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;

    fn act(name: &str, urgency: Urgency) -> Action {
        Action {
            kind: ActionKind::RequestApproval,
            effect: ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new(name),
            },
            target_effect: TargetEffect::Blocks,
            urgency,
            blocker: BlockerKey::for_test(name),
        }
    }

    #[test]
    fn urgency_total_order_matches_design_intent() {
        // The total order on `Urgency` IS the priority lattice.
        // New tiers slot in by definition; the sort never changes.
        assert!(Urgency::Mid(MidTier::Critical) < Urgency::Mid(MidTier::BlockingFix));
        assert!(Urgency::Mid(MidTier::BlockingFix) < Urgency::Mid(MidTier::BlockingWait));
        assert!(Urgency::Mid(MidTier::BlockingWait) < Urgency::Mid(MidTier::BlockingHuman));
        assert!(Urgency::Mid(MidTier::BlockingHuman) < Urgency::Mid(MidTier::Advancing));
        assert!(Urgency::Mid(MidTier::Advancing) < Urgency::Mid(MidTier::Hygiene));
    }

    #[test]
    fn priority_sort_is_stable_within_urgency() {
        // Equal-urgency actions keep their input (axis) order
        // after sorting — the stability witness for the rank step.
        let mut v = [
            act("fix-a", Urgency::Mid(MidTier::BlockingFix)),
            act("wait", Urgency::Mid(MidTier::BlockingWait)),
            act("fix-b", Urgency::Mid(MidTier::BlockingFix)),
            act("critical", Urgency::Mid(MidTier::Critical)),
            act("human", Urgency::Mid(MidTier::BlockingHuman)),
            act("hygiene", Urgency::Mid(MidTier::Hygiene)),
        ];
        v.sort_by_key(|a| a.urgency);
        let order: Vec<&str> = v.iter().map(|a| a.blocker.as_str()).collect();
        assert_eq!(
            order,
            vec!["critical", "fix-a", "fix-b", "wait", "human", "hygiene"]
        );
    }
}
