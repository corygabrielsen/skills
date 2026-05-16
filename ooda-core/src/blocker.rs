//! Stall comparator key.
//!
//! `BlockerKey` is the second half of the `(kind, blocker)` tuple
//! `run_loop` uses to detect stalls. Newtype prevents accidental
//! confusion with `Action::description` (also `String`-shaped) and
//! documents the invariant that the value MUST NOT embed varying
//! counts or other progress markers — two iterations addressing
//! "5 threads" and "4 threads" share the blocker key
//! `threads:address`, not separate keys.
//!
//! ## Construction discipline
//!
//! The type pushes back on accidental volatility via two narrow
//! constructors:
//!
//! * [`BlockerKey::from_static`] takes `&'static str`. The static
//!   lifetime is the stability witness: only string literals (or
//!   `&'static str` values selected at runtime from a fixed set,
//!   e.g. an enum's `name()` returning `&'static str`) can flow in.
//! * [`BlockerKey::typed`] takes a `&'static str` category plus a
//!   typed identifier implementing [`GateIdentity`]. The trait is a
//!   marker: implementors assert "same gate → same `Display`
//!   output across iterations." `String`, `usize`, and `Vec<_>`
//!   deliberately do NOT implement it; if you find yourself wanting
//!   to format a count or a comma-list into a blocker key, the type
//!   system is correctly pushing back. Move the cohort onto the
//!   action payload; let the renderer extract it from there.
//!
//! [`BlockerKey::parse`] remains for external/deserialized input
//! where the producer's stability is inherited.

use crate::single_line_string::SingleLineString;
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

/// Marker trait: implementor asserts that [`fmt::Display`] produces
/// the same output for the same underlying gate across iterations,
/// and different output for distinct gates.
///
/// **Implement only for typed wrappers whose value is bound to gate
/// identity** — for example a `CheckName` for "blocked on this
/// specific check," or an enum's variant whose `Display` returns a
/// `&'static str` per variant. Do NOT implement for `String`,
/// primitive numbers, or collection types — those typically vary
/// independently of gate identity and would defeat the stall
/// comparator.
pub trait GateIdentity: fmt::Display {}

/// Stable iteration key. Non-empty by construction.
///
/// No `Deserialize` — `BlockerKey` is constructed and consumed
/// entirely inside the decide / runner layers; nothing parses it
/// from external input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct BlockerKey(String);

impl BlockerKey {
    /// Categorical key from a literal or `&'static str`. The
    /// static lifetime is the stability witness — the only values
    /// that satisfy `&'static str` are compile-time string
    /// literals or runtime-selected entries from a fixed set
    /// (`enum::name() -> &'static str`, `const`s, etc.).
    ///
    /// # Panics
    ///
    /// Panics if `s` is empty or whitespace-only — both are
    /// programmer errors in the caller, not user input.
    #[must_use]
    pub fn from_static(s: &'static str) -> Self {
        assert!(
            !s.trim().is_empty(),
            "BlockerKey::from_static called with empty or whitespace-only string"
        );
        Self(s.to_owned())
    }

    /// Category + typed identifier. The category is a static-
    /// lifetime string (stable by lifetime); the identifier
    /// implements [`GateIdentity`] (stable by trait contract).
    /// Renders as `"<category>:<id>"`.
    ///
    /// # Panics
    ///
    /// Panics if `category` is empty or whitespace-only.
    #[must_use]
    pub fn typed<I: GateIdentity>(category: &'static str, id: &I) -> Self {
        assert!(
            !category.trim().is_empty(),
            "BlockerKey::typed called with empty category"
        );
        Self(format!("{category}:{id}"))
    }

    /// Validating constructor for external / deserialized input.
    /// Use when the value comes from a recorder file or other
    /// previously-written source where stability was the producer's
    /// responsibility.
    ///
    /// # Errors
    ///
    /// Returns [`BlockerKeyError`] if the input is empty or
    /// whitespace-only.
    pub fn parse(s: impl Into<String>) -> Result<Self, BlockerKeyError> {
        let s = s.into();
        if s.trim().is_empty() {
            return Err(BlockerKeyError("empty or whitespace-only".into()));
        }
        Ok(Self(s))
    }

    /// Test-only escape hatch — accepts arbitrary content for
    /// fixture construction where the strong-typed constructors
    /// would require boilerplate. Marked `#[doc(hidden)]`;
    /// production callers should use [`Self::from_static`] or
    /// [`Self::typed`] instead.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(s: impl Into<String>) -> Self {
        Self(s.into())
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

// ── GateIdentity impls for ooda-core types ──────────────────────────
//
// Add impls here as call sites in PR-side / codex-side decide layers
// adopt `BlockerKey::typed(prefix, &id)` with these types. The
// impls are intentionally minimal — implement only when an actual
// caller needs the type as a blocker identifier.

impl GateIdentity for SingleLineString {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_static_accepts_literal() {
        assert_eq!(BlockerKey::from_static("ci:fix").as_str(), "ci:fix");
    }

    #[test]
    #[should_panic(expected = "BlockerKey::from_static called with empty")]
    fn from_static_panics_on_empty() {
        let _ = BlockerKey::from_static("");
    }

    #[test]
    #[should_panic(expected = "BlockerKey::from_static called with empty")]
    fn from_static_panics_on_whitespace_only() {
        let _ = BlockerKey::from_static("   ");
    }

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
    fn typed_renders_prefix_colon_id() {
        struct FakeId;
        impl fmt::Display for FakeId {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the-id")
            }
        }
        impl GateIdentity for FakeId {}
        assert_eq!(BlockerKey::typed("cat", &FakeId).as_str(), "cat:the-id");
    }

    #[test]
    fn display_matches_as_str() {
        let k = BlockerKey::from_static("ci:wait");
        assert_eq!(format!("{k}"), "ci:wait");
    }
}
