//! Stall comparator key.
//!
//! The stall detector compares `(kind_name, blocker)` across
//! iterations; `BlockerKey` is the second component. The newtype
//! enforces one invariant:
//!
//! **Gate-stability**: two iterations gated by the same underlying
//! condition produce equal `BlockerKey` values; varying counts or
//! progress markers are forbidden in the key (they belong on the
//! action payload).
//!
//! Construction discipline establishes the invariant structurally:
//!
//! * [`BlockerKey::from_static`] requires `&'static str`. The static
//!   lifetime is the stability witness — only compile-time-known
//!   tokens (literals or per-variant `&'static str` identifiers)
//!   satisfy the type.
//! * [`BlockerKey::typed`] requires a `&'static str` category plus
//!   a typed identifier implementing [`GateIdentity`], whose
//!   contract is "same gate → same `Display` output across
//!   iterations." Types whose value typically varies independently
//!   of gate identity (counts, dynamic strings, collections) MUST
//!   NOT implement the marker; the type system then pushes back on
//!   any attempt to format such a value into a key.
//! * [`BlockerKey::parse`] accepts dynamic input for deserialization
//!   only; gate-stability is inherited from the producer.

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

/// Gate-stability marker. Implementors assert: [`fmt::Display`]
/// is a function of gate identity alone — same gate ⇒ same output
/// across iterations; distinct gates ⇒ distinct output.
///
/// Sound implementors are typed wrappers whose value is bound to
/// gate identity (e.g. a name-of-the-blocking-thing newtype, or a
/// closed enum whose `Display` returns a per-variant `&'static
/// str`). Implementing the marker for types whose value varies
/// independently of gate identity (free-form strings, counts,
/// collections) defeats the stall comparator and is forbidden.
pub trait GateIdentity: fmt::Display {}

/// Stable iteration key. Non-empty by construction; gate-stable
/// by the construction-discipline invariants above.
///
/// No `Deserialize` impl: the key is produced and consumed within
/// the decide-and-loop pipeline; nothing parses it from external
/// wire input.
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

    /// Test-only escape hatch for fixture construction. Bypasses
    /// the gate-stability discipline of [`Self::from_static`] /
    /// [`Self::typed`]; production call sites MUST use those.
    ///
    /// Gated behind `cfg(test)` (within this crate) or the
    /// `test-support` Cargo feature (for cross-crate fixtures), so
    /// release builds of dependent binaries cannot reach it.
    #[cfg(any(test, feature = "test-support"))]
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

// GateIdentity impls for types defined in this crate. Domain
// crates add their own impls for domain-specific identifiers.

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
