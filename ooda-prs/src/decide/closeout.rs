//! Closeout candidate — the convergence gate.
//!
//! Fires when the axis is unsynced, at the least-urgent tier in
//! the priority lattice. The urgency placement is the mechanism:
//! every other axis preempts the closeout by construction, so the
//! candidate is selected only on global quiescence and the
//! terminal human handoff is conditional on an explicit
//! agent-signed attestation at HEAD.
//!
//! Unlike the SHA-keyed attestation axes, the closeout fires on
//! zero-commit PRs as well — the gate is about pre-handoff
//! sign-off rather than hygiene-when-there-is-work.

use std::path::Path;

use crate::act::closeout::build_closeout_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::closeout::Closeout;

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

/// Declared deps: own report + own attest-path location.
#[must_use]
pub(crate) fn candidates(
    closeout: &Closeout,
    attest_path: Option<&Path>,
    pr: PullRequestNumber,
) -> Vec<Action> {
    let needs_closeout = matches!(closeout, Closeout::Drift { .. } | Closeout::NeverAttested);
    if !needs_closeout {
        return Vec::new();
    }
    let Some(attest_path) = attest_path else {
        return Vec::new();
    };

    let attest_path_opt: Option<&Path> = Some(attest_path);
    let prompt = build_closeout_prompt(pr, closeout, attest_path_opt);
    let kind = ActionKind::Closeout {
        attest_path: attest_path.to_path_buf(),
    };
    // Distinct keys for distinct gate identities so a transition
    // from never-attested to drift is not masked as a stall.
    let blocker = match closeout {
        Closeout::Drift { .. } => BlockerKey::from_static("closeout_drift"),
        Closeout::NeverAttested => BlockerKey::from_static("closeout_never_attested"),
        Closeout::Synced => BlockerKey::from_static("closeout_synced"),
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::Post,
        blocker,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber};
    use ooda_core::MidTier;

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn attest_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/state/753/closeout_attest.json")
    }

    fn drift() -> Closeout {
        Closeout::Drift {
            attested_sha: GitCommitSha::parse(&"a".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            head_sha: GitCommitSha::parse(&"b".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
        }
    }

    #[test]
    fn drift_emits_closeout_at_closeout_urgency() {
        let cs = candidates(&drift(), Some(&attest_path()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::Closeout { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Post);
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "closeout_drift");
    }

    #[test]
    fn never_attested_emits_closeout() {
        let cs = candidates(&Closeout::NeverAttested, Some(&attest_path()), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_never_attested");
    }

    #[test]
    fn never_attested_with_zero_commits_still_emits() {
        // The closeout's commit-count contract diverges from the
        // SHA-keyed attestation axes: pre-handoff sign-off is the
        // gate, independent of whether work has shipped.
        let cs = candidates(&Closeout::NeverAttested, Some(&attest_path()), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_never_attested");
    }

    #[test]
    fn drift_with_zero_commits_still_emits() {
        let cs = candidates(&drift(), Some(&attest_path()), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_drift");
    }

    #[test]
    fn synced_emits_nothing() {
        let cs = candidates(&Closeout::Synced, Some(&attest_path()), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn closeout_carries_attest_path_in_payload() {
        let cs = candidates(&drift(), Some(&attest_path()), pr());
        let ActionKind::Closeout { attest_path } = &cs[0].kind else {
            panic!("expected Closeout");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/closeout_attest.json")
        );
    }

    #[test]
    fn closeout_stall_key_distinguishes_drift_from_never_attested() {
        let drift_action = candidates(&drift(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        let never_action = candidates(&Closeout::NeverAttested, Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(drift_action.stall_key(), never_action.stall_key());
    }

    #[test]
    fn closeout_stall_key_equal_to_itself() {
        let a = candidates(&drift(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        let b = candidates(&drift(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn drift_with_no_attest_path_emits_nothing() {
        assert!(candidates(&drift(), None, pr()).is_empty());
    }

    #[test]
    fn never_attested_with_no_attest_path_emits_nothing() {
        assert!(candidates(&Closeout::NeverAttested, None, pr()).is_empty());
    }

    #[test]
    fn closeout_action_name_is_closeout() {
        let a = candidates(&drift(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "Closeout");
    }

    #[test]
    fn closeout_is_strictly_least_urgent_in_decide() {
        let a = candidates(&drift(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        // Structural witness for the global-quiescence gate: the
        // tier sits strictly below every other axis's tier.
        assert!(Urgency::Mid(MidTier::Hygiene) < a.urgency);
        assert!(Urgency::Mid(MidTier::Critical) < a.urgency);
    }
}
