//! Per-concern decision unit for multi-lane OODA drivers.
//!
//! # Definition
//!
//! An [`Axis`] is a typed candidate emitter over a per-axis
//! observation slice. The driver merges candidates across axes by
//! phase-aware urgency. The trait is the minimum contract that any
//! concern-level state machine must satisfy to participate in a
//! [`super::Driver`] (forthcoming).
//!
//! # Why no projection method
//!
//! An earlier sketch split projection (`project(obs) -> Report`)
//! from decision (`candidates(report)`). The shape held for the
//! local-projection health axes (CI, Cursor, Copilot) but broke
//! for convergence axes — reviews, mechanical state, attestations,
//! closeout — whose candidates read across multiple axes' reports
//! and cannot be expressed as a function of a single per-axis
//! `Report`. Collapsing the trait to candidates-only restores
//! uniform shape across both kinds. Projection survives as a
//! private helper inside each impl when the impl wants it; the
//! contract no longer mandates it.
//!
//! # Invariants
//!
//! - **No cross-axis state mutation**: [`Axis::candidates`] takes
//!   `&self` and an immutable `&O`; it cannot reach into other
//!   axes' state.
//! - **Per-domain, not cross-domain**: all `Axis` impls within a
//!   single driver share `ActionKind` and `MidTier`. Cross-domain
//!   composition is a separate concern (the `Outcome` contract
//!   between drivers, not the `Axis` trait).
//! - **Observation slices the inputs honestly**: each axis's `O`
//!   names every input it reads — local observations and refs to
//!   upstream axes' projected state alike. Cross-axis dependencies
//!   are visible in the type.
//!
//! # Out of scope
//!
//! - Persistent per-axis state across driver runs (handled by the
//!   driver's recorder; the trait itself is stateless across
//!   ticks unless an impl chooses to hold internal state in `self`).
//! - Recursive composition (an `Axis` whose internals is itself
//!   a `Driver`) — the cross-driver composition lives at the
//!   `Outcome` boundary, not at the trait level.
//! - Dynamic axis types at runtime; `Driver` composition is
//!   compile-time.
//!
//! # Where the abstraction stops being useful
//!
//! Documented explicitly so the trait is not over-applied:
//!
//! - Single-concern targets: `Driver = Axis`; no merge value.
//! - Tightly-coupled concerns where every axis depends on every
//!   other; the merge becomes bookkeeping rather than decision.
//! - Probabilistic / expected-utility decision-making; the trait
//!   is deterministic-given-state by contract.
//!
//! See `project-ooda-algebra-evolution` memory for the locked
//! build sequence: this trait is **step 2**; canonical first impl
//! (CI in ooda-pr) is **step 2b**; mechanical sweep across the
//! remaining lanes is **step 3**; second domain is **step 4**.

use crate::action::{Action, ActionKindName};

/// Per-concern decision unit. See module doc for the full contract.
///
/// `O` is the axis's observation-input type. Each axis declares
/// its own observation slice; the driver constructs it from the
/// global observation bundle plus any cross-axis state the axis
/// names as dependencies.
pub trait Axis<O> {
    /// Domain-typed action variants this axis can emit. Must
    /// implement [`ActionKindName`] for log-line rendering and
    /// stall-comparator stability.
    type ActionKind: ActionKindName;

    /// Emit candidate actions from the observation slice. Each
    /// action carries phase-aware urgency (`Urgency<MidTier>`);
    /// the driver merges across axes by lex order.
    ///
    /// Pure: same observation → same candidates. State that varies
    /// across runs lives in the driver's recorder, not here.
    fn candidates(&self, obs: &O) -> Vec<Action<Self::ActionKind>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, ActionEffect, TargetEffect, Urgency};
    use crate::blocker::BlockerKey;
    use serde::Serialize;

    /// Minimal `ActionKind` for the doc-axis test.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    enum DocAxisKind {
        Sync,
    }

    impl ActionKindName for DocAxisKind {
        fn name(&self) -> &'static str {
            "Sync"
        }
    }

    /// Minimal observation: just a "needs sync" flag.
    struct DocAxisObservation {
        needs_sync: bool,
    }

    /// Trait-validation impl: smallest possible axis.
    struct DocAxis;

    impl Axis<DocAxisObservation> for DocAxis {
        type ActionKind = DocAxisKind;

        fn candidates(&self, obs: &DocAxisObservation) -> Vec<Action<Self::ActionKind>> {
            if obs.needs_sync {
                vec![Action {
                    kind: DocAxisKind::Sync,
                    effect: ActionEffect::Full {
                        log: "sync".into(),
                        upstream: crate::action::UpstreamConsistency::Sync,
                    },
                    target_effect: TargetEffect::Neutral,
                    urgency: Urgency::Pre,
                    blocker: BlockerKey::from_static("needs_sync"),
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn axis_smoke_test_with_minimal_impl() {
        let axis = DocAxis;
        let obs = DocAxisObservation { needs_sync: true };
        let candidates = axis.candidates(&obs);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind.name(), "Sync");
        assert_eq!(candidates[0].urgency, Urgency::Pre);
    }

    #[test]
    fn axis_emits_no_candidate_on_silent_state() {
        let axis = DocAxis;
        let obs = DocAxisObservation { needs_sync: false };
        assert!(axis.candidates(&obs).is_empty());
    }
}
