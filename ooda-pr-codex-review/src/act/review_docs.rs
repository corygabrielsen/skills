//! Handoff-prompt composition for the doc-review candidate.
//!
//! This candidate's effect is agent-handoff; no driver-side action
//! runs. The module produces the prompt body the agent receives.

use std::path::Path;

use ooda_core::HandoffPrompt;

use crate::ids::PullRequestNumber;
use crate::orient::doc_review::DocReview;

/// Build the doc-review handoff prompt body.
///
/// `state` selects the why-preamble. `attest_path` is `Option`
/// because the binary may run without a configured state root;
/// when present the prompt surfaces a literal CLI invocation,
/// when absent the prompt asks the agent to supply the path.
#[must_use]
pub(crate) fn build_review_docs_prompt(
    pr: PullRequestNumber,
    state: &DocReview,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let headline = "Doc/comment review needed.";
    let mut prompt = HandoffPrompt::new(headline);

    prompt.push_paragraph(why_paragraph(state).to_string());

    prompt.push_paragraph(
        "Step 1 — review the full PR diff for doc and comment hygiene. \
         Run `git diff $merge_base..HEAD --` against the PR's working tree. \
         Inspect every changed `.rs` file's added or modified comments \
         (`///`, `//`, `//!`) and module docs."
            .to_string(),
    );

    prompt.push_paragraph(
        "Apply these principles (repo convention; also see CONTRIBUTING.md \
         if present): default to NO comments — code should be self-documenting \
         where possible; when a comment IS written, use specification voice — \
         state what the code does or guarantees, not why-history or motivation; \
         no examples in doc comments unless they illustrate a non-obvious \
         invariant; no multi-paragraph narration — one short line per concept; \
         comments must say something the code can't say — outdated comments \
         rot faster than code because they lack linters. If you find slop: \
         tighten or delete it in place. Commit your edits before Step 2."
            .to_string(),
    );

    prompt.push_paragraph(format!(
        "Step 2 — run the attestation CLI:\n\n    {}\n\nThis reads HEAD and \
         writes the attestation file atomically. State-root is resolved via \
         the same env chain as ooda-pr (OODA_PR_STATE_HOME → \
         XDG_STATE_HOME/ooda-pr → HOME/.local/state/ooda-pr).",
        cli_invocation(pr, attest_path),
    ));

    prompt
}

fn why_paragraph(state: &DocReview) -> &'static str {
    match state {
        DocReview::Drift { .. } => {
            "The PR diff has advanced past the last reviewed-for-docs attestation."
        }
        DocReview::NeverAttested => "The PR has not yet been reviewed for doc/comment hygiene.",
        DocReview::Synced => {
            "Doc review is already synced; re-attesting because a downstream \
             axis requested it."
        }
    }
}

fn cli_invocation(pr: PullRequestNumber, attest_path: Option<&Path>) -> String {
    match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => format!(
            "ooda-attest doc-review --pr-id {pr} --state-root {}",
            state_root.display()
        ),
        None => format!(
            "ooda-attest doc-review --pr-id {pr} --state-root <absolute path to \
             OODA state root; report back if you do not know it — this \
             invocation was started without --state-root>"
        ),
    }
}

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

    fn drift() -> DocReview {
        DocReview::Drift {
            attested_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            commits_behind: Some(4),
        }
    }

    #[test]
    fn prompt_for_drift_explains_advance_past_attestation() {
        let path = std::path::PathBuf::from("/state/753/doc_review_attest.json");
        let p = build_review_docs_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(s.contains("advanced past"), "{s}");
        assert!(s.starts_with("Doc/comment review needed."), "{s}");
    }

    #[test]
    fn prompt_for_never_attested_uses_first_review_language() {
        let path = std::path::PathBuf::from("/state/753/doc_review_attest.json");
        let p = build_review_docs_prompt(pr(), &DocReview::NeverAttested, Some(&path));
        let s = p.to_string();
        assert!(s.contains("not yet been reviewed"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/doc_review_attest.json");
        let p = build_review_docs_prompt(pr(), &drift(), Some(&path));
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest doc-review --pr-id 753 --state-root /state"),
            "{s}",
        );
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let p = build_review_docs_prompt(pr(), &drift(), None);
        let s = p.to_string();
        assert!(
            s.contains("ooda-attest doc-review --pr-id 753 --state-root"),
            "{s}"
        );
        assert!(s.contains("report back if you do not know it"), "{s}");
        assert!(s.contains("without --state-root"), "{s}");
    }

    #[test]
    fn prompt_mentions_contributing_md() {
        let p = build_review_docs_prompt(pr(), &drift(), None);
        assert!(p.to_string().contains("CONTRIBUTING.md"));
    }

    #[test]
    fn prompt_references_git_diff_merge_base() {
        let p = build_review_docs_prompt(pr(), &drift(), None);
        assert!(p.to_string().contains("git diff $merge_base..HEAD"));
    }

    #[test]
    fn prompt_orders_command_immediately_after_step_2_header() {
        let path = std::path::PathBuf::from("/state/753/doc_review_attest.json");
        let s = build_review_docs_prompt(pr(), &drift(), Some(&path)).to_string();
        let step2 = s.find("Step 2").expect("step 2 present");
        let command = s.find("ooda-attest doc-review").expect("command present");
        let automatic = s
            .find("writes the attestation file")
            .expect("automatic explanation present");
        assert!(step2 < command, "command should follow Step 2 header");
        assert!(
            command < automatic,
            "automatic explanation should follow the command, not precede it",
        );
    }

    #[test]
    fn prompt_step_1_orders_review_principles_after_diff_invocation() {
        let p = build_review_docs_prompt(pr(), &drift(), None);
        let s = p.to_string();
        let step1 = s.find("Step 1").expect("step 1 present");
        let diff = s.find("git diff $merge_base").expect("diff present");
        let principles = s
            .find("default to NO comments")
            .expect("principles present");
        assert!(step1 < diff);
        assert!(diff < principles);
    }
}
