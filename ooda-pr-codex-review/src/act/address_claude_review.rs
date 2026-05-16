//! Compose the `AddressClaudeReview` handoff prompt.
//!
//! `AddressClaudeReview` is `ActionEffect::Agent` — `act()` never
//! executes it; the runner converts it to `Outcome::HandoffAgent` and
//! exits. This module only builds the prompt body the recipient agent
//! reads.

use std::fmt::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use ooda_core::{HandoffPrompt, NonEmpty, SingleLineString, Witness};

use crate::ids::PullRequestNumber;

/// Build the `AddressClaudeReview` handoff prompt body.
///
/// The latest Claude review body is inlined as a [`Witness`] so the
/// recipient agent does not need a `gh` round-trip to read the
/// review material. `attest_path` is recovered to the state-root for
/// the literal CLI invocation. `attest_path` is `Option` because the
/// binary may run without `--state-root`.
#[must_use]
pub fn build_address_claude_review_prompt(
    pr: PullRequestNumber,
    latest_claude_at: DateTime<Utc>,
    latest_claude_body: &str,
    latest_claude_url: &str,
    inline_thread_count: usize,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let headline = "Claude review needs addressing.";
    let mut prompt = HandoffPrompt::new(headline);

    prompt.push_paragraph(
        "Claude has posted review content past the last attestation. \
         Address the threads and main review body, then re-attest at HEAD."
            .to_string(),
    );

    prompt.push_paragraph(format!(
        "Step 1 — run `/loop-address-pr-feedback --pr {pr}`. This skill \
         polls all three GitHub surfaces where Claude lands (issue \
         comments, review comments, review threads), triages each \
         finding, fixes valid ones, replies, resolves threads, and \
         re-requests review. The main Claude review body is inlined \
         below; inline threads (if any) are addressed automatically \
         by the skill."
    ));

    let label = SingleLineString::new(format!(
        "claude main review @ {latest_claude_at} ({})",
        thread_count_label(inline_thread_count),
    ));
    let witness_body = witness_body(latest_claude_body, latest_claude_url);
    prompt.push_witnesses(NonEmpty::singleton(Witness {
        label,
        body: witness_body,
    }));

    prompt.push_paragraph(format!(
        "Step 2 — run the attestation CLI:\n\n    {}\n\nThis reads HEAD \
         and writes the attestation file atomically. State-root is \
         resolved via the same env chain as ooda-pr (OODA_PR_STATE_HOME \
         → XDG_STATE_HOME/ooda-pr → HOME/.local/state/ooda-pr) if \
         --state-root is omitted.",
        cli_invocation(pr, attest_path),
    ));

    prompt
}

fn thread_count_label(n: usize) -> String {
    if n == 1 {
        "1 inline thread".to_string()
    } else {
        format!("{n} inline threads")
    }
}

fn witness_body(body: &str, url: &str) -> String {
    let mut s = String::new();
    if !url.is_empty() {
        let _ = write!(s, "URL: {url}\n\n");
    }
    if body.trim().is_empty() {
        s.push_str("(review body was empty)");
    } else {
        s.push_str(body);
    }
    s
}

fn cli_invocation(pr: PullRequestNumber, attest_path: Option<&Path>) -> String {
    match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => format!(
            "ooda-attest claude-review --pr-id {pr} --state-root {}",
            state_root.display()
        ),
        None => format!(
            "ooda-attest claude-review --pr-id {pr} --state-root <absolute path \
             to OODA state root; report back if you do not know it — this \
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

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn prompt_starts_with_headline() {
        let p = build_address_claude_review_prompt(pr(), ts(), "body", "url", 1, None);
        assert!(p.to_string().starts_with("Claude review needs addressing."));
    }

    #[test]
    fn prompt_includes_witness_with_body() {
        let path = std::path::PathBuf::from("/state/753/claude_review_attest.json");
        let s =
            build_address_claude_review_prompt(pr(), ts(), "🔴 important", "url", 1, Some(&path))
                .to_string();
        assert!(s.contains("🔴 important"), "{s}");
    }

    #[test]
    fn prompt_includes_witness_label_with_thread_count() {
        let s = build_address_claude_review_prompt(pr(), ts(), "body", "url", 2, None).to_string();
        assert!(s.contains("2 inline threads"), "{s}");
    }

    #[test]
    fn prompt_includes_witness_url_when_present() {
        let s =
            build_address_claude_review_prompt(pr(), ts(), "body", "https://example/r/1", 0, None)
                .to_string();
        assert!(s.contains("https://example/r/1"), "{s}");
    }

    #[test]
    fn prompt_includes_loop_skill_invocation_with_pr() {
        let s = build_address_claude_review_prompt(pr(), ts(), "body", "url", 0, None).to_string();
        assert!(s.contains("/loop-address-pr-feedback --pr 753"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/claude_review_attest.json");
        let s = build_address_claude_review_prompt(pr(), ts(), "body", "url", 0, Some(&path))
            .to_string();
        assert!(
            s.contains("ooda-attest claude-review --pr-id 753 --state-root /state"),
            "{s}",
        );
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let s = build_address_claude_review_prompt(pr(), ts(), "body", "url", 0, None).to_string();
        assert!(
            s.contains("ooda-attest claude-review --pr-id 753 --state-root"),
            "{s}",
        );
        assert!(s.contains("report back if you do not know it"), "{s}");
        assert!(s.contains("without --state-root"), "{s}");
    }

    #[test]
    fn prompt_orders_command_immediately_after_step_2_header() {
        let path = std::path::PathBuf::from("/state/753/claude_review_attest.json");
        let s = build_address_claude_review_prompt(pr(), ts(), "body", "url", 0, Some(&path))
            .to_string();
        let step2 = s.find("Step 2").expect("step 2 present");
        let command = s
            .find("ooda-attest claude-review")
            .expect("command present");
        let automatic = s
            .find("writes the attestation file")
            .expect("automatic explanation present");
        assert!(step2 < command, "command should follow Step 2 header");
        assert!(
            command < automatic,
            "automatic explanation should follow the command",
        );
    }

    #[test]
    fn prompt_empty_body_renders_placeholder() {
        let s = build_address_claude_review_prompt(pr(), ts(), "  ", "", 0, None).to_string();
        assert!(s.contains("(review body was empty)"), "{s}");
    }

    #[test]
    fn thread_count_label_singular_vs_plural() {
        assert_eq!(thread_count_label(0), "0 inline threads");
        assert_eq!(thread_count_label(1), "1 inline thread");
        assert_eq!(thread_count_label(2), "2 inline threads");
    }
}
