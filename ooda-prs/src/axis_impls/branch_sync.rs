//! `BranchSyncAxis` — divergence between the per-PR sticky head
//! SHA and the live remote head, lifted to the [`Axis`] trait.
//!
//! Declared deps: the branch-sync observation bundle, sourced from
//! [`crate::observe::branch`].

use crate::decide::action::{Action, ActionKind};
use crate::observe::branch::BranchSyncObservation;
use ooda_core::Axis;

pub(crate) struct BranchSyncAxis;

impl<'a> Axis<&'a BranchSyncObservation> for BranchSyncAxis {
    type ActionKind = ActionKind;

    fn candidates(&self, obs: &&'a BranchSyncObservation) -> Vec<Action> {
        crate::decide::branch_sync::candidates(obs)
    }
}
