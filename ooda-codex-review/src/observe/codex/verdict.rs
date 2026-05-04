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
    match normalized {
        "no issues found" | "no issues" | "looks good" => VerdictClass::Clean,
        _ => VerdictClass::HasIssues,
    }
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
}
