//! Pure verdict extraction and classification over `codex review`
//! log text. No I/O.
//!
//! `codex review` streams interleaved `thinking`/`exec`/`codex`
//! blocks. The actual review result is the LAST block whose first
//! line is `codex` (exact match — not a substring). The polling
//! protocol uses the presence of that marker line to detect
//! completion (loop-codex-review/SKILL.md → "Reading Review
//! Results").

use serde::Serialize;

/// Extract the verdict block — everything after the LAST line that
/// is exactly `codex` (no surrounding whitespace, no suffix).
/// Returns `None` when the marker is absent (review still streaming
/// thinking/exec output).
///
/// Mirrors the reference `awk` from loop-codex-review SKILL.md:
/// `awk '/^codex$/{found=1; block=""; next} found{block=block $0
/// "\n"} END{printf "%s", block}'` — last marker wins, body is
/// everything after it.
pub fn extract_verdict(log: &str) -> Option<String> {
    let mut after_last_marker: Option<usize> = None;
    let mut offset = 0usize;
    for line in log.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if trimmed == "codex" {
            after_last_marker = Some(offset + line.len());
        }
        offset += line.len();
    }
    after_last_marker.map(|i| log[i..].to_string())
}

/// Did the reviewer find anything to flag?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictClass {
    /// Empty body or "No issues found" — the review is clean.
    Clean,
    /// Body contains issue descriptions; needs an AddressBatch
    /// halt to verify and fix.
    HasIssues,
}

/// Classify an extracted verdict body. Conservative: anything not
/// recognized as an explicit "clean" phrasing is treated as
/// has-issues. The decide layer can defer to LLM disambiguation
/// when the heuristic returns the wrong answer (rare in practice).
pub fn classify(verdict: &str) -> VerdictClass {
    let body = verdict.trim();
    if body.is_empty() {
        return VerdictClass::Clean;
    }
    let normalized = body.to_ascii_lowercase();
    let normalized = normalized.trim_end_matches('.').trim();
    if has_issue_markers(normalized) {
        return VerdictClass::HasIssues;
    }
    if matches!(
        normalized,
        "no issues found" | "no issues" | "looks good" | "no actionable findings"
    ) || normalized.contains("no actionable issues")
        || normalized.contains("no actionable correctness issues")
        || normalized.contains("no actionable findings")
        || normalized.contains("did not find any")
        || normalized.contains("didn't find any")
        || normalized.contains("did not identify any")
        || normalized.contains("did not find actionable")
        || normalized.contains("did not find correctness")
        || normalized.contains("i found no correctness issues")
        || normalized.contains("no prioritized, actionable correctness issues")
        || normalized.contains("no prioritized, actionable correctness issue")
        || normalized.contains("no discrete, actionable correctness issues")
        || normalized.contains("no discrete, actionable correctness issue")
        || normalized.contains("no discrete correctness")
        || normalized.contains("no discrete regression")
        || normalized.contains("no discrete correctness issues")
        || normalized.contains("appear to preserve existing restart")
        || (normalized.contains("appears to preserve") && normalized.contains("checks pass"))
        || normalized.contains("without introducing regressions")
        || normalized.contains("without introducing an obvious")
        || normalized.contains("without introducing an evident")
        || normalized.contains("without introducing any evident")
        || normalized.contains("without introducing observable")
        || normalized.contains("without introducing observable breakage")
        || normalized.contains("without introducing observable regressions")
    {
        VerdictClass::Clean
    } else {
        VerdictClass::HasIssues
    }
}

fn has_issue_markers(normalized: &str) -> bool {
    // Retained markers have semantic grounding: review-comment
    // headers and priority tags (`[P0]`/`[P1]`/...) are emitted by
    // `codex review` when it has something to flag.
    //
    // The conjunction words ` but ` and ` however ` were previously
    // listed here and over-triggered on hedging language in clean
    // verdicts ("did not find any issues, but consider X" / "looks
    // good. however, ..."), routing them to HasIssues. They are
    // not semantic markers — they are common English connectives —
    // so they were removed.
    normalized.contains("review comment:")
        || normalized.contains("full review comments:")
        || normalized.contains("\n- [p")
        || normalized.starts_with("- [p")
        || normalized.contains("\n[p")
        || normalized.starts_with("[p")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_returns_none_when_marker_absent() {
        let log = "thinking\n  reasoning...\nexec\n  ran cmd\n";
        assert_eq!(extract_verdict(log), None);
    }

    #[test]
    fn extract_returns_text_after_last_marker() {
        let log = "thinking\nfoo\ncodex\nfirst\nthinking\nbar\ncodex\nthe verdict\nmore\n";
        assert_eq!(extract_verdict(log).unwrap(), "the verdict\nmore\n");
    }

    #[test]
    fn extract_empty_when_log_ends_at_marker() {
        let log = "thinking\nfoo\ncodex\n";
        assert_eq!(extract_verdict(log).as_deref(), Some(""));
    }

    #[test]
    fn extract_does_not_match_codex_substring() {
        let log = "codex review running\nfoo\ncodex starting\n";
        assert_eq!(extract_verdict(log), None);
    }

    #[test]
    fn extract_does_not_match_indented_codex() {
        let log = "  codex\ncodex too\n";
        assert_eq!(extract_verdict(log), None);
    }

    #[test]
    fn extract_handles_marker_without_trailing_newline() {
        let log = "thinking\ncodex";
        assert_eq!(extract_verdict(log).as_deref(), Some(""));
    }

    #[test]
    fn classify_empty_is_clean() {
        assert_eq!(classify(""), VerdictClass::Clean);
        assert_eq!(classify("   \n\t  "), VerdictClass::Clean);
    }

    #[test]
    fn classify_explicit_no_issues_phrasings_are_clean() {
        assert_eq!(classify("No issues found"), VerdictClass::Clean);
        assert_eq!(classify("\nNo issues found.\n"), VerdictClass::Clean);
        assert_eq!(classify("NO ISSUES"), VerdictClass::Clean);
        assert_eq!(classify("Looks good."), VerdictClass::Clean);
    }

    #[test]
    fn classify_review_comment_is_has_issues() {
        let body = "Review comment: src/foo.rs:42\nUse a parameterized query.\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_long_body_with_clean_phrase_is_has_issues() {
        let body = "Earlier passes returned no issues found, but this iteration\n\
                    src/foo.rs:42 — null deref possible\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_common_long_clean_phrasings_are_clean() {
        assert_eq!(
            classify("I did not find any discrete regressions in this change."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify("No actionable findings. The patch looks consistent."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify("I did not identify any correctness issues."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify("No actionable correctness issues were found in the diff."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The TypeScript replacement appears to preserve the simultaneous deploy workflow \
                 and the supporting workflow/docs updates are consistent. TypeScript compilation \
                 and formatting checks pass for the changed scripts."
            ),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_observable_regression_phrasings_are_clean() {
        assert_eq!(
            classify(
                "The changes add the required CloudWatch Logs permission and safely improve SSM output visibility without introducing observable breakage. Type checking and tests pass."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The changes add the required CloudWatch Logs describe permission and safely surface SSM output without introducing observable regressions. Lint and tests pass for the modified repository."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The changes add the missing CloudWatch Logs permission and safely surface SSM output without introducing observable correctness regressions. Type checking passed for the GitHub scripts, and I did not find actionable bugs in the diff."
            ),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_evident_regression_phrasings_are_clean() {
        assert_eq!(
            classify(
                "The changes add the missing CloudWatch Logs permission and make SSM output handling safer and more observable without introducing regressions in the reviewed paths. TypeScript checking for the GitHub scripts passes."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The changes add the required CloudWatch Logs permission and make SSM/CloudWatch output safer and more useful without introducing an evident functional regression. Type checking, formatting, and diff checks pass for the touched files."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The changes add the needed CloudWatch Logs permission and safely surface fallback SSM output without introducing any evident correctness, security, or maintainability regressions. TypeScript compilation for the GitHub scripts succeeds."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The diff adds the missing CloudWatch Logs permission and safely falls back to terminal SSM output without introducing an obvious correctness or integration regression."
            ),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_did_not_find_issue_phrasings_are_clean() {
        assert_eq!(
            classify(
                "The changes are narrowly scoped to adding the required CloudWatch Logs permission and safely surfacing fallback SSM output. I did not find correctness, security, or operational issues introduced by the patch."
            ),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_no_discrete_issue_phrasings_are_clean() {
        assert_eq!(
            classify(
                "I found no discrete correctness, safety, or integration issues introduced by this diff. The new deploy log streaming path preserves SSM fallback behavior and the TypeScript changes typecheck under the repository lint command."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "I found no discrete, actionable correctness issues in the diff. The new deploy-log streaming path preserves the existing SSM fallback behavior and the TypeScript/shell changes appear consistent with the existing deployment flow."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "No prioritized, actionable correctness issues were found in the diff against master. The shell and TypeScript changes compile/parse cleanly and the deploy log fallback paths appear consistent with existing behavior."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "I found no correctness issues in the diff. The changes type-check, lint, and tests pass locally."
            ),
            VerdictClass::Clean
        );
        assert_eq!(
            classify(
                "The changes appear to preserve existing restart/convergence behavior while adding deploy log streaming with SSM fallback and non-blocking CloudWatch configuration updates."
            ),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_phrase_with_review_markers_is_has_issues() {
        let body = "I did not find any broad architectural concern.\n\
                    Full review comments:\n\
                    - [P2] Keep the retry timeout bounded";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_preserve_phrase_with_contrast_marker_is_has_issues() {
        let body = "The TypeScript replacement appears to preserve the deploy workflow. \
                    However, the external health check now exits before restoring state.";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_clean_hedged_with_but_is_clean() {
        // Hedged clean verdict: a recognized clean phrasing followed
        // by an English `but ...` continuation. The conjunction must
        // not flip the classification.
        assert_eq!(
            classify("I did not find any correctness issues, but consider renaming foo."),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_hedged_with_however_is_clean() {
        // Hedged clean verdict using "however" as the connective.
        assert_eq!(
            classify("No actionable findings. However, the naming of foo could be tightened."),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_clean_did_not_find_with_but_is_clean() {
        // Long-form clean verdict that contains ` but ` in body prose.
        assert_eq!(
            classify(
                "I did not identify any correctness regressions in this diff, \
                 but the helper module could be split for readability."
            ),
            VerdictClass::Clean
        );
    }
}
