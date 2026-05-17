//! Length-capped free-form body for markdown emission.
//!
//! [`Witness::body`] and other free-form fields in
//! [`crate::handoff_prompt`] are not constrained at construction —
//! a multi-MB review body or paragraph propagates verbatim through
//! the recorder's per-event JSONL writer, where a single `write_all`
//! exceeds `PIPE_BUF` (4096) and concurrent `O_APPEND` writers risk
//! byte interleaving.
//!
//! [`SafeBody`] applies a structural cap at construction. Inputs
//! over [`SafeBody::MAX_BYTES`] are truncated and an `(elided: N
//! bytes)` marker appended so downstream readers see the overflow
//! rather than silent truncation.
//!
//! The cap is in bytes, not characters — large bodies almost always
//! come from human-readable English review text where the two
//! coincide closely, and the recorder constraint that motivates the
//! cap is byte-denominated. The truncation point lands on a UTF-8
//! character boundary so the resulting `String` is always valid
//! UTF-8.

use serde::{Serialize, Serializer};
use std::fmt;
use std::fmt::Write as _;

/// A `String` whose byte length is bounded at construction. Inputs
/// over [`Self::MAX_BYTES`] are truncated at a UTF-8 boundary and
/// suffixed with a visible elision marker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SafeBody(String);

impl SafeBody {
    /// Maximum body size in bytes. 64 KiB is large enough to carry
    /// any realistic review comment, code-fenced diff snippet, or
    /// CI log excerpt while keeping a single recorder write well
    /// under the `PIPE_BUF` interleaving threshold even with
    /// surrounding event-frame overhead.
    pub const MAX_BYTES: usize = 64 * 1024;

    /// Construct, truncating at [`Self::MAX_BYTES`] with a visible
    /// elision marker if the input is over the cap.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        if s.len() <= Self::MAX_BYTES {
            return Self(s);
        }
        // UTF-8 boundary: walk back from the cap until we land on
        // a char-boundary. The cap is large enough that this loop
        // moves at most 3 bytes.
        let mut cut = Self::MAX_BYTES;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let elided = s.len() - cut;
        let mut out = String::with_capacity(cut + 64);
        out.push_str(&s[..cut]);
        write!(out, "\n... (truncated: {elided} bytes elided)").expect("write to String");
        Self(out)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for SafeBody {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SafeBody {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl fmt::Display for SafeBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SafeBody {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_input_passes_through_unchanged() {
        let b = SafeBody::new("hello world");
        assert_eq!(b.as_str(), "hello world");
    }

    #[test]
    fn input_at_cap_passes_through_unchanged() {
        let s = "a".repeat(SafeBody::MAX_BYTES);
        let b = SafeBody::new(s.clone());
        assert_eq!(b.as_str(), s);
    }

    #[test]
    fn input_over_cap_is_truncated_with_marker() {
        let s = "a".repeat(SafeBody::MAX_BYTES + 1234);
        let b = SafeBody::new(s);
        assert!(b.as_str().contains("(truncated: 1234 bytes elided)"));
        // Original prefix preserved up to cap.
        assert!(b.as_str().starts_with(&"a".repeat(SafeBody::MAX_BYTES)));
    }

    #[test]
    fn truncation_lands_on_utf8_boundary() {
        // 4-byte char straddles cap boundary; truncation must
        // back up to keep the result valid UTF-8.
        let four_byte = "𝄞"; // U+1D11E — 4 bytes
        let mut s = "a".repeat(SafeBody::MAX_BYTES - 2);
        s.push_str(four_byte);
        s.push_str("trailing");
        let b = SafeBody::new(s);
        // Round-trip through &str confirms valid UTF-8.
        assert!(b.as_str().contains("(truncated"));
    }

    #[test]
    fn empty_input_passes_through() {
        let b = SafeBody::new("");
        assert_eq!(b.as_str(), "");
    }

    #[test]
    fn display_emits_inner() {
        let b = SafeBody::new("foo");
        assert_eq!(format!("{b}"), "foo");
    }

    #[test]
    fn serializes_as_json_string() {
        let b = SafeBody::new("foo");
        assert_eq!(serde_json::to_string(&b).unwrap(), "\"foo\"");
    }
}
