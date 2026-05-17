//! `CommonMark` inline-escape helper for free-form text bound to
//! markdown-rendered surfaces (PR comments, dashboard headers).
//!
//! Backslash-escapes the `CommonMark` inline-control set so a free
//! `String` reaching a renderer can't close a code span, emit a
//! stray emphasis, or open a link. The set is the punctuation list
//! from `CommonMark` §2.4 ("ASCII punctuation") restricted to the
//! subset that has inline structural meaning: `\``*_{}[]<>`.
//!
//! Use at the rendering boundary — not at construction. A value
//! that is sometimes rendered inline (PR comment header) and
//! sometimes rendered literally (logs) should not be pre-escaped
//! at construction; escaping is the renderer's contract.

/// Backslash-escape the `CommonMark` inline-control set. Idempotent
/// under repeated application is intentionally not provided — the
/// escape is applied once at the renderer boundary, never on
/// already-escaped input.
#[must_use]
pub fn md_inline_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '<' | '>' | '#' | '+' | '-' | '.'
            | '!' | '|' | '(' | ')' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(md_inline_escape("hello world"), "hello world");
    }

    #[test]
    fn escapes_backtick() {
        assert_eq!(md_inline_escape("a `b` c"), "a \\`b\\` c");
    }

    #[test]
    fn escapes_asterisk_and_underscore() {
        assert_eq!(md_inline_escape("*bold* _em_"), "\\*bold\\* \\_em\\_");
    }

    #[test]
    fn escapes_brackets() {
        assert_eq!(md_inline_escape("[link](x)"), "\\[link\\]\\(x\\)");
    }

    #[test]
    fn escapes_angle_brackets() {
        assert_eq!(md_inline_escape("<script>"), "\\<script\\>");
    }

    #[test]
    fn escapes_backslash() {
        assert_eq!(md_inline_escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn passes_unicode_through() {
        assert_eq!(md_inline_escape("héllo 🦀"), "héllo 🦀");
    }
}
