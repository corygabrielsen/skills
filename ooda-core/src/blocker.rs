//! Stall comparator key.
//!
//! `BlockerKey` is the second half of the `(kind, blocker)` tuple
//! `run_loop` uses to detect stalls. Newtype prevents accidental
//! confusion with `Action::description` (also `String`-shaped) and
//! documents the invariant that the value MUST NOT embed varying
//! counts or other progress markers — two iterations addressing
//! "5 threads" and "4 threads" share the blocker key
//! `threads:address`, not separate keys.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerKeyError(String);

impl fmt::Display for BlockerKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid blocker key: {}", self.0)
    }
}

impl std::error::Error for BlockerKeyError {}

/// Stable iteration key. Non-empty by construction.
///
/// No `Deserialize` — `BlockerKey` is constructed and consumed
/// entirely inside the decide / runner layers; nothing parses it
/// from external input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct BlockerKey(String);

impl BlockerKey {
    /// Validating constructor for arbitrary input. Use when the
    /// value comes from user-controlled or computed text where
    /// emptiness is possible.
    ///
    /// # Errors
    ///
    /// Returns [`BlockerKeyError`] if the input is empty or
    /// whitespace-only. Whitespace-only inputs are rejected because
    /// stall-comparator equality is positional on the inner string
    /// — `" "` and `"  "` would compare unequal, defeating stall
    /// detection.
    pub fn parse(s: impl Into<String>) -> Result<Self, BlockerKeyError> {
        let s = s.into();
        if s.trim().is_empty() {
            return Err(BlockerKeyError("empty or whitespace-only".into()));
        }
        Ok(Self(s))
    }

    /// Infallible constructor for known-non-empty construction —
    /// literal prefixes joined with typed payloads inside the
    /// decide layer. Panics on empty or whitespace-only input (a
    /// programmer error in the caller, not user input). `Self`
    /// return signals "construction is intended to succeed" where
    /// `parse(...).expect(...)` would suggest a fallible op.
    ///
    /// # Panics
    ///
    /// Panics if the input is empty or whitespace-only.
    #[must_use]
    pub fn tag(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(
            !s.trim().is_empty(),
            "BlockerKey::tag called with empty or whitespace-only string"
        );
        Self(s)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlockerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_empty() {
        assert!(BlockerKey::parse("").is_err());
    }

    #[test]
    fn parse_rejects_whitespace_only() {
        assert!(BlockerKey::parse(" ").is_err());
        assert!(BlockerKey::parse("   ").is_err());
        assert!(BlockerKey::parse("\t").is_err());
        assert!(BlockerKey::parse("\t\n  ").is_err());
    }

    #[test]
    fn parse_accepts_non_empty() {
        assert_eq!(BlockerKey::parse("ci:fix").unwrap().as_str(), "ci:fix");
    }

    #[test]
    fn tag_constructs_from_literal() {
        assert_eq!(
            BlockerKey::tag("threads:address").as_str(),
            "threads:address"
        );
    }

    #[test]
    #[should_panic(expected = "BlockerKey::tag called with empty or whitespace-only string")]
    fn tag_panics_on_empty() {
        let _ = BlockerKey::tag("");
    }

    #[test]
    #[should_panic(expected = "BlockerKey::tag called with empty or whitespace-only string")]
    fn tag_panics_on_whitespace_only() {
        let _ = BlockerKey::tag("   ");
    }

    #[test]
    fn display_matches_as_str() {
        let k = BlockerKey::tag("ci:wait");
        assert_eq!(format!("{k}"), "ci:wait");
    }
}
