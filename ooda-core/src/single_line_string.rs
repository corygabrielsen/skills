//! Single-line string newtype.
//!
//! The OODA binaries' stderr header invariant is "the first line
//! is the variant header; nothing else appears on that line except
//! the variant's documented payload format". Two `Outcome`
//! variants — `BinaryError(_)` and `UsageError(_)` — carry a
//! free-form message in their payload. If that message contains a
//! newline, the header line silently splits and the regex-based
//! header parser callers use against stderr breaks.
//!
//! [`SingleLineString`] makes the no-newlines invariant
//! structural: every constructor flattens newlines at the
//! boundary, so it is impossible to produce a value of this type
//! containing `\n`. The variant payloads can then be typed
//! `SingleLineString` instead of `String`, and the
//! `flatten_one_line` helper that used to live in `outcome.rs`
//! disappears.
//!
//! `Display` and `Serialize` both emit the inner string as-is
//! (which is, by construction, single-line), so call sites that
//! used `format!("{msg}")` or `json!({"msg": msg})` keep working.

use serde::{Serialize, Serializer};
use std::fmt;

/// A `String` guaranteed by construction to contain no `\n`.
/// `\r` and other control bytes are not modified — only `\n` is
/// significant for stderr-header parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SingleLineString(String);

impl SingleLineString {
    /// Construct from any `Into<String>`. Any `\n` in the input is
    /// replaced with a single space; other characters pass
    /// through unchanged.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        if s.contains('\n') {
            Self(s.replace('\n', " "))
        } else {
            Self(s)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for SingleLineString {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SingleLineString {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl fmt::Display for SingleLineString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SingleLineString {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_single_line_through_unchanged() {
        assert_eq!(SingleLineString::new("hello").as_str(), "hello");
        assert_eq!(SingleLineString::new(String::from("hi")).as_str(), "hi");
    }

    #[test]
    fn flattens_embedded_newlines_to_spaces() {
        assert_eq!(
            SingleLineString::new("line one\nline two").as_str(),
            "line one line two"
        );
        assert_eq!(SingleLineString::new("a\nb\nc\nd").as_str(), "a b c d");
    }

    #[test]
    fn handles_leading_and_trailing_newlines() {
        assert_eq!(SingleLineString::new("\nfoo\n").as_str(), " foo ");
    }

    #[test]
    fn preserves_other_whitespace_and_control_bytes() {
        // Only \n is touched; tabs, CR, etc. survive unchanged.
        assert_eq!(SingleLineString::new("a\tb\r c").as_str(), "a\tb\r c");
    }

    #[test]
    fn empty_input_is_allowed() {
        // The "non-empty" property is not part of this type's
        // contract — the OODA caller may produce a zero-length
        // header payload (e.g. an empty UsageError msg) and we
        // pass it through.
        assert_eq!(SingleLineString::new("").as_str(), "");
    }

    #[test]
    fn from_impls_round_trip() {
        let s: SingleLineString = "hello".into();
        assert_eq!(s.as_str(), "hello");
        let s: SingleLineString = String::from("hi\nthere").into();
        assert_eq!(s.as_str(), "hi there");
    }

    #[test]
    fn display_emits_inner_string() {
        assert_eq!(format!("{}", SingleLineString::new("foo bar")), "foo bar");
    }

    #[test]
    fn serializes_as_a_json_string_not_a_struct() {
        let s = SingleLineString::new("hello\nworld");
        let json = serde_json::to_string(&s).unwrap();
        // String, not {"0": "..."}: the newtype is transparent
        // to serde.
        assert_eq!(json, "\"hello world\"");
    }
}
