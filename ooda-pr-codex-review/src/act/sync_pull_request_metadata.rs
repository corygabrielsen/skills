//! Handoff-prompt composition for the PR-metadata sync candidate.
//!
//! This candidate's effect is agent-handoff; no driver-side action
//! runs. The module produces the prompt body the agent receives.

use std::path::Path;

use ooda_core::HandoffPrompt;

use crate::ids::PullRequestNumber;
use crate::orient::pull_request_metadata::PullRequestMetadata;

/// Build the PR-metadata sync handoff prompt body.
///
/// `state` selects the why-preamble that situates the work for the
/// reader. `attest_path` is `Option` because the binary may run
/// without a configured state root; when present it determines a
/// literal CLI invocation, when absent the prompt falls back to a
/// placeholder that asks the agent to supply the path.
#[must_use]
pub(crate) fn build_sync_pull_request_metadata_prompt(
    pr: PullRequestNumber,
    state: &PullRequestMetadata,
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
         Refer to the repository's CONTRIBUTING.md for conventions. Keep it tight."
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

fn why_paragraph(state: &PullRequestMetadata) -> &'static str {
    match state {
        PullRequestMetadata::Drift { .. } => {
            "The PR title, description, and labels are currently out of sync \
             with HEAD."
        }
        PullRequestMetadata::NeverAttested => {
            "The PR title, description, and labels have not yet been attested \
             as synced with HEAD."
        }
        PullRequestMetadata::Synced => {
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

/// Recover the state-root directory from the per-axis attestation
/// path. The path layout nests the attestation under a per-PR
/// directory under the state root; `None` is returned when the
/// structure does not match.
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

    fn drift() -> PullRequestMetadata {
        PullRequestMetadata::Drift {
            attested_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            commits_behind: Some(4),
        }
    }

    #[test]
    fn prompt_for_drift_explains_out_of_sync() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(s.contains("out of sync with HEAD"), "{s}");
        assert!(s.starts_with("# PR metadata sync needed."), "{s}");
    }

    #[test]
    fn prompt_for_never_attested_uses_attestation_language() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pull_request_metadata_prompt(
            pr(),
            &PullRequestMetadata::NeverAttested,
            Some(&path),
        );
        let s = p.to_string();
        assert!(s.contains("not yet been attested"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest pr-meta --pr-id 753 --state-root /state"),
            "{s}",
        );
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), None);
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
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), None);
        assert!(p.to_string().contains("CONTRIBUTING.md"));
    }

    #[test]
    fn prompt_explains_squash_merge_rationale() {
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("squash-merge"), "{s}");
        assert!(s.contains("commit message"), "{s}");
    }

    #[test]
    fn prompt_keeps_step_1_tight_and_defers_to_contributing_md() {
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("Keep it tight"), "{s}");
        assert!(s.contains("CONTRIBUTING.md"), "{s}");
        assert!(!s.contains("rot and verbosity"), "{s}");
    }

    #[test]
    fn prompt_explains_binary_writes_atomically_without_json_or_sha_work() {
        let p = build_sync_pull_request_metadata_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(s.contains("writes the attestation file"), "{s}");
        assert!(s.contains("atomically"), "{s}");
        assert!(s.contains("do not construct JSON"), "{s}");
    }

    #[test]
    fn prompt_orders_command_immediately_after_step_2_header() {
        let path = std::path::PathBuf::from("/state/753/pr_meta_attest.json");
        let s = build_sync_pull_request_metadata_prompt(pr(), &drift(), Some(&path)).to_string();
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
