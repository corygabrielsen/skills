//! Pure verdict extraction and classification over `codex review`
//! log text. No I/O.
//!
//! `codex review` streams interleaved `thinking`/`exec`/`codex`
//! blocks. The actual review result is the LAST block whose first
//! line is `codex` (exact match — not a substring).

use serde::Serialize;

/// Extract the verdict block — everything after the LAST line that
/// is exactly `codex` (no surrounding whitespace, no suffix).
/// Returns `None` when the marker is absent.
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
    Clean,
    HasIssues,
}

/// Classify an extracted verdict body. Conservative: anything not
/// recognized as an explicit "clean" phrasing is treated as
/// has-issues.
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
        || normalized.contains("no discrete regression")
        || normalized.contains("no discrete correctness issues")
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
    normalized.contains("review comment:")
        || normalized.contains("full review comments:")
        || normalized.contains("\n- [p")
        || normalized.starts_with("- [p")
        || normalized.contains("\n[p")
        || normalized.starts_with("[p")
        || normalized.contains(" but ")
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
    fn classify_empty_is_clean() {
        assert_eq!(classify(""), VerdictClass::Clean);
    }

    #[test]
    fn classify_explicit_no_issues_phrasings_are_clean() {
        assert_eq!(classify("No issues found"), VerdictClass::Clean);
        assert_eq!(classify("Looks good."), VerdictClass::Clean);
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
    fn classify_review_comment_is_has_issues() {
        assert_eq!(
            classify("Review comment: src/foo.rs:42\nUse a parameterized query.\n"),
            VerdictClass::HasIssues
        );
    }
}
