//! Handoff-prompt composition for the closeout candidate.
//!
//! This candidate's effect is agent-handoff; no driver-side action
//! runs. The prompt is the agent's input.
//!
//! Composition with the dashboard preamble (injected separately
//! upstream) is what determines the prompt's scope: per-axis
//! signals are already surfaced by the preamble, so the body here
//! is prescriptive only about the residue — the checks no axis
//! catches automatically.

use std::path::Path;

use ooda_core::HandoffPrompt;

use crate::ids::PullRequestNumber;
use crate::orient::closeout::Closeout;

/// Build the closeout handoff prompt body.
///
/// `state` selects the why-preamble. `attest_path` is `Option`
/// because the binary may run without a configured state root;
/// when present the prompt surfaces a literal CLI invocation,
/// when absent the prompt asks the agent to supply the path.
#[must_use]
pub(crate) fn build_closeout_prompt(
    pr: PullRequestNumber,
    state: &Closeout,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let headline = "Closeout: confirm PR ready for human handoff.";
    let mut prompt = HandoffPrompt::new(headline);

    prompt.push_paragraph(why_paragraph(state).to_string());

    prompt.push_paragraph(
        "Step 1 — verify what no axis catches. Walk the PR diff one \
         more time and check for: leftover scaffolding (TODO / XXX / \
         FIXME, debug prints, commented-out code, dead helpers); \
         description-to-diff alignment (the description describes \
         what actually shipped); label appropriateness; commit-message \
         hygiene; no accidental file inclusions (build artifacts, \
         local config, secrets). Treat anything you'd flag in a human \
         peer's PR as something to fix."
            .to_string(),
    );

    prompt.push_paragraph(
        "Step 2 — cross-check the Dashboard preamble above. Per-axis \
         state (CI, reviews, Copilot, Cursor, hygiene attestations) \
         is the system's view of the PR. If anything reads stale or \
         surprising, investigate before attesting — the gate is your \
         word that the PR is genuinely ready, not just that the queue \
         is empty."
            .to_string(),
    );

    prompt.push_paragraph(format!(
        "Step 3 — if everything checks out, run the attestation CLI:\n\n    {}\n\n\
         If anything needs fixing, fix it and push. The SHA will \
         change and the cycle reopens automatically — do not attest \
         in that case. Attesting is the terminal act; only attest \
         when you are ready to hand off.",
        cli_invocation(pr, attest_path),
    ));

    prompt
}

fn why_paragraph(state: &Closeout) -> &'static str {
    match state {
        Closeout::Drift { .. } => {
            "Every other axis has converged at a new HEAD, past your \
             last closeout attestation. A fresh final-pass review is \
             needed before handoff."
        }
        Closeout::NeverAttested => {
            "Every other axis has converged. No closeout attestation \
             has been recorded yet for this PR; a final-pass review \
             is needed before handoff."
        }
        Closeout::Synced => {
            "Closeout is already synced; re-attesting because a \
             downstream surface requested it."
        }
    }
}

fn cli_invocation(pr: PullRequestNumber, attest_path: Option<&Path>) -> String {
    match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => format!(
            "ooda-attest closeout --pr-id {pr} --state-root {}",
            state_root.display()
        ),
        None => format!(
            "ooda-attest closeout --pr-id {pr} --state-root <absolute path to \
             OODA state root; report back if you do not know it — this \
             invocation was started without --state-root>"
        ),
    }
}

/// Recover the state-root directory from the per-axis attestation
/// path. The path layout nests the attestation under a per-PR
/// directory under the state root; `None` when the structure
/// does not match.
fn state_root_from_attest_path(path: &Path) -> Option<std::path::PathBuf> {
    let pr_dir = path.parent()?;
    let state_root = pr_dir.parent()?;
    Some(state_root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PullRequestNumber;

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn drift() -> Closeout {
        Closeout::Drift {
            attested_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        }
    }

    #[test]
    fn prompt_for_drift_explains_convergence_at_new_head() {
        let path = std::path::PathBuf::from("/state/753/closeout_attest.json");
        let p = build_closeout_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(s.contains("converged at a new HEAD"), "{s}");
        assert!(
            s.starts_with("# Closeout: confirm PR ready for human handoff."),
            "{s}"
        );
    }

    #[test]
    fn prompt_for_never_attested_uses_first_attestation_language() {
        let path = std::path::PathBuf::from("/state/753/closeout_attest.json");
        let p = build_closeout_prompt(pr(), &Closeout::NeverAttested, Some(&path));
        let s = p.to_string();
        assert!(s.contains("No closeout attestation"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/closeout_attest.json");
        let p = build_closeout_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest closeout --pr-id 753 --state-root /state"),
            "{s}",
        );
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let p = build_closeout_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest closeout --pr-id 753 --state-root"),
            "{s}"
        );
        assert!(s.contains("report back if you do not know it"), "{s}");
        assert!(s.contains("without --state-root"), "{s}");
    }

    #[test]
    fn prompt_step_1_enumerates_non_axis_checks() {
        let p = build_closeout_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("scaffolding"), "{s}");
        assert!(s.contains("description-to-diff"), "{s}");
        assert!(s.contains("label appropriateness"), "{s}");
        assert!(s.contains("commit-message hygiene"), "{s}");
    }

    #[test]
    fn prompt_step_2_references_dashboard_preamble() {
        let p = build_closeout_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("Dashboard preamble"), "{s}");
    }

    #[test]
    fn prompt_step_3_explains_fix_path_does_not_attest() {
        let p = build_closeout_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("cycle reopens automatically"), "{s}");
        assert!(s.contains("do not attest"), "{s}");
    }

    #[test]
    fn prompt_orders_command_immediately_after_step_3_header() {
        let path = std::path::PathBuf::from("/state/753/closeout_attest.json");
        let s = build_closeout_prompt(pr(), &drift(), Some(&path)).to_string();
        let step3 = s.find("Step 3").expect("step 3 present");
        let command = s.find("ooda-attest closeout").expect("command present");
        assert!(step3 < command, "command should follow Step 3 header");
    }

    #[test]
    fn prompt_orders_steps_1_then_2_then_3() {
        let p = build_closeout_prompt(pr(), &drift(), None);
        let s = p.to_string();
        let step1 = s.find("Step 1").expect("step 1 present");
        let step2 = s.find("Step 2").expect("step 2 present");
        let step3 = s.find("Step 3").expect("step 3 present");
        assert!(step1 < step2, "step 1 before step 2");
        assert!(step2 < step3, "step 2 before step 3");
    }
}
