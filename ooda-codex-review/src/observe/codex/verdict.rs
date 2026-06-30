//! Pure verdict extraction and classification over review log
//! text. No I/O.
//!
//! The review CLI streams interleaved blocks; the verdict body is
//! everything after the LAST marker line (exact string `codex` on
//! its own line — substring match is incorrect). The marker line's
//! presence is also the completion signal for the polling protocol.

use serde::Serialize;

/// Extract the verdict body — everything after the LAST marker
/// line (exact string `codex` on its own line). Returns `None`
/// when the marker is absent.
pub(crate) fn extract_verdict(log: &str) -> Option<String> {
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

/// Verdict classification (ternary algebra).
///
/// - Structural issue markers (the CLI's own grammar — priority
///   bullets, review-comment headers) are authoritative.
/// - Clean is recognized via a phrasing list calibrated against
///   the CLI's observed clean verdicts.
/// - Anything else is `Indeterminate`: the classifier abstains
///   rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VerdictClass {
    /// Empty body or recognized clean phrasing.
    Clean,
    /// Structural issue markers present.
    HasIssues,
    /// No structural markers, no recognized clean phrasing.
    /// Operationally routed like `HasIssues`; surfaced
    /// distinctly so post-hoc tooling can see the abstention.
    Indeterminate,
    /// Slot was abandoned: the reviewer subprocess didn't finish
    /// before the loop gave up (cap reached, or all pending slots
    /// went idle past the alive threshold). Routed like `HasIssues`
    /// so the loop never silently claims fixed-point on a partial
    /// sample. Surfaced distinctly so post-hoc tooling and human
    /// resolvers can distinguish "review ran and was inconclusive"
    /// from "review never completed".
    Abandoned,
}

/// Classify an extracted verdict body.
///
/// Evaluation order: structural markers before prose
/// phrase-matching. A priority bullet in an otherwise
/// clean-leaning summary still classifies as `HasIssues`.
pub(crate) fn classify(verdict: &str) -> VerdictClass {
    let body = verdict.trim();
    if body.is_empty() {
        return VerdictClass::Clean;
    }
    let normalized = body.to_ascii_lowercase();
    let normalized = normalized.trim_end_matches('.').trim();

    if has_issue_signal(normalized) {
        return VerdictClass::HasIssues;
    }
    if matches_clean_phrasing(normalized) {
        return VerdictClass::Clean;
    }
    VerdictClass::Indeterminate
}

fn has_issue_signal(s: &str) -> bool {
    // Line-anchored to avoid mid-sentence false positives
    // (a prose mention of "review comment" must not match).
    // Priority bullets and the review-comment header are the
    // CLI's ground-truth schema for issue markers.
    s.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("- [p1]")
            || t.starts_with("- [p2]")
            || t.starts_with("- [p3]")
            || t.starts_with("[p1]")
            || t.starts_with("[p2]")
            || t.starts_with("[p3]")
            || t.starts_with("review comment:")
            || t.starts_with("full review comments:")
    })
}

fn matches_clean_phrasing(normalized: &str) -> bool {
    // Phrasing list calibrated against the CLI's observed clean
    // verdicts. Hedging conjunctions (` but `, ` however `) are
    // intentionally absent: they over-trigger on hedged clean
    // phrasings ("...is clean however ...").
    //
    // "no issues" stays exact-match-only — two words is too short
    // to contains-match safely (mid-sentence "previously had no
    // issues, but now broken" would false-positive).
    matches!(
        normalized,
        "no issues found" | "no issues" | "looks good" | "no actionable findings"
    ) || normalized.contains("no issues found")
        || normalized.contains("looks good")
        || normalized.contains("no actionable issues")
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
    fn classify_indeterminate_for_prose_without_signal() {
        // Real-shape: summary paragraph with no markers AND no
        // recognized clean phrasing. Codex's prose is too irregular
        // to classify; the honest answer is to abstain.
        let body = "I reviewed the diff against master. The structural changes \
                    introduce a new component layer, and the tests touch the \
                    relevant code paths.";
        assert_eq!(classify(body), VerdictClass::Indeterminate);
    }

    #[test]
    fn classify_p1_marker_is_has_issues() {
        let body =
            "<summary>\n\nReview comment:\n\n- [P1] Critical bug — src/foo.rs:1\n  details\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_p2_marker_is_has_issues() {
        let body = "<summary>\n\nReview comment:\n\n- [P2] Issue — src/foo.rs:1\n  details\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_p3_marker_is_has_issues() {
        let body = "<summary>\n\nReview comment:\n\n- [P3] Nit — src/foo.rs:1\n  details\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_full_review_comments_header_with_bullets_is_has_issues() {
        let body = "<summary>\n\nFull review comments:\n\n- [P1] First — src/a.rs:1\n  desc\n\n\
                    - [P2] Second — src/b.rs:2\n  desc\n";
        assert_eq!(classify(body), VerdictClass::HasIssues);
    }

    #[test]
    fn classify_review_comment_substring_in_prose_is_not_has_issues() {
        // Line-anchored matching — mid-sentence "review comment"
        // should not trigger the structural marker rule. The body
        // ends with a recognized clean phrasing so it lands Clean
        // rather than Indeterminate.
        let body = "The earlier review comment was addressed and I did not find any \
                    new correctness issues.";
        assert_eq!(classify(body), VerdictClass::Clean);
    }

    #[test]
    fn classify_marker_takes_precedence_over_clean_phrasing() {
        // Real shape: clean-leaning summary + an explicit P-marker.
        // The marker is authoritative; classifier returns HasIssues.
        let body = "I did not find any major issues in the diff overall.\n\n\
                    Review comment:\n\n- [P2] One thing — src/foo.rs:1\n  details\n";
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

    #[test]
    fn classify_canonical_phrase_with_trailing_prose_is_clean() {
        // Short canonical clean phrasings followed by trailing prose
        // must still classify Clean — contains-match for the safe
        // long-enough phrasings ("no issues found", "looks good",
        // "no actionable findings").
        assert_eq!(
            classify("No issues found, but consider renaming foo."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify("I think it looks good overall."),
            VerdictClass::Clean
        );
        assert_eq!(
            classify("No actionable findings; the patch is straightforward."),
            VerdictClass::Clean
        );
    }

    #[test]
    fn classify_bare_no_issues_short_phrase_is_clean() {
        // Exact-match-only kept for "no issues" — too short to
        // contains-match safely.
        assert_eq!(classify("no issues"), VerdictClass::Clean);
    }
}
