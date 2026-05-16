//! Compose the `SyncPrMeta` handoff prompt.
//!
//! `SyncPrMeta` is `ActionEffect::Agent` — `act()` never executes
//! it; the runner converts it to `Outcome::HandoffAgent` and exits.
//! This module only builds the prompt body the recipient agent
//! reads.

use std::path::Path;

use ooda_core::HandoffPrompt;

use crate::ids::PullRequestNumber;
use crate::orient::pr_meta::PrMetadata;

/// Build the `SyncPrMeta` handoff prompt body.
///
/// `state` and `attest_path` together let the prompt pick the
/// right "why" preamble (`Drift` cites commit count; `NeverAttested`
/// states first-attestation) and surface the exact CLI invocation
/// the agent must run after updating PR meta.
///
/// `attest_path` is `Option` because the binary may run without
/// `--state-root`; the prompt then falls back to a placeholder
/// invocation that asks the agent to supply the path.
#[must_use]
pub fn build_sync_pr_meta_prompt(
    pr: PullRequestNumber,
    state: &PrMetadata,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let headline = "PR metadata sync needed.";
    let mut prompt = HandoffPrompt::new(headline);

    prompt.push_paragraph(format!(
        "{} The squash-merge will use the PR title and description as the \
         commit message, so they must reflect the commits at HEAD.",
        why_paragraph(state),
    ));

    prompt.push_paragraph(
        "Step 1 — update the PR title, description, and labels to match HEAD. \
         Refer to the repository's CONTRIBUTING.md for conventions. Keep it \
         tight — defend against rot and verbosity. Docs and comments rot \
         faster than code because they lack linters and compilers."
            .to_string(),
    );

    prompt.push_paragraph(format!(
        "Step 2 — run the attestation CLI:\n\n    {}\n\nThis reads HEAD and \
         writes the attestation file (SHA, timestamp, schema version) \
         atomically. You do not construct JSON or look up the SHA yourself.",
        cli_invocation(pr, attest_path),
    ));

    prompt
}

fn why_paragraph(state: &PrMetadata) -> &'static str {
    match state {
        PrMetadata::Drift { .. } => {
            "The PR title, description, and labels are currently out of sync \
             with HEAD."
        }
        PrMetadata::NeverAttested => {
            "The PR title, description, and labels have not yet been attested \
             as synced with HEAD."
        }
        PrMetadata::Synced => {
            "PR metadata is already synced; re-attesting because a downstream \
             axis requested it."
        }
    }
}

fn cli_invocation(pr: PullRequestNumber, attest_path: Option<&Path>) -> String {
    match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => format!(
            "ooda-attest pr-meta --pr-id {pr} --state-root {}",
            state_root.display()
        ),
        None => format!(
            "ooda-attest pr-meta --pr-id {pr} --state-root <absolute path to \
             OODA state root; report back if you do not know it — this \
             invocation was started without --state-root>"
        ),
    }
}

/// Recover the state-root directory from the attestation path —
/// `<state-root>/<pr-id>/pr_meta_attest.json`. Returns `None` if
/// the structure does not match.
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

    fn drift() -> PrMetadata {
        PrMetadata::Drift {
            attested_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            commits_behind: 4,
        }
    }

    #[test]
    fn prompt_for_drift_explains_out_of_sync() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pr_meta_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(s.contains("out of sync with HEAD"), "{s}");
        assert!(s.starts_with("PR metadata sync needed."), "{s}");
    }

    #[test]
    fn prompt_for_never_attested_uses_attestation_language() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pr_meta_prompt(pr(), &PrMetadata::NeverAttested, Some(&path));
        let s = p.to_string();
        assert!(s.contains("not yet been attested"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pr_meta_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest pr-meta --pr-id 753 --state-root /state"),
            "{s}",
        );
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let p = build_sync_pr_meta_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest pr-meta --pr-id 753 --state-root"),
            "{s}"
        );
        assert!(s.contains("report back if you do not know it"), "{s}");
        assert!(s.contains("without --state-root"), "{s}");
    }

    #[test]
    fn prompt_mentions_contributing_md() {
        let p = build_sync_pr_meta_prompt(pr(), &drift(), None);
        assert!(p.to_string().contains("CONTRIBUTING.md"));
    }

    #[test]
    fn prompt_explains_squash_merge_rationale() {
        let p = build_sync_pr_meta_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("squash-merge"), "{s}");
        assert!(s.contains("commit message"), "{s}");
    }

    #[test]
    fn prompt_warns_against_verbosity() {
        let p = build_sync_pr_meta_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("Keep it tight"), "{s}");
        assert!(s.contains("rot and verbosity"), "{s}");
    }

    #[test]
    fn prompt_explains_binary_writes_atomically_without_json_or_sha_work() {
        let p = build_sync_pr_meta_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("writes the attestation file"), "{s}");
        assert!(s.contains("atomically"), "{s}");
        assert!(s.contains("do not construct JSON"), "{s}");
    }

    #[test]
    fn prompt_orders_command_immediately_after_step_2_header() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let s = build_sync_pr_meta_prompt(pr(), &drift(), Some(&path)).to_string();
        let step2 = s.find("Step 2").expect("step 2 present");
        let command = s.find("ooda-attest pr-meta").expect("command present");
        let automatic = s
            .find("writes the attestation file")
            .expect("automatic explanation present");
        assert!(step2 < command, "command should follow Step 2 header");
        assert!(
            command < automatic,
            "automatic explanation should follow the command, not precede it",
        );
    }
}
