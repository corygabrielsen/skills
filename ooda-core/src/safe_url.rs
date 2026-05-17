//! Scheme-restricted URL newtype.
//!
//! Markdown renderers and downstream HTML viewers expose URL fields
//! as link targets. Without a construction-time guard, a free
//! `String` URL field accepts `javascript:`, `data:`, or
//! `\n`-injected attacker payloads. [`SafeUrl`] forbids all of these
//! by accepting only `http://` and `https://` and rejecting any
//! input containing a `\r`, `\n`, or NUL byte.
//!
//! No external `url` dep — the validation surface is the scheme
//! prefix + control-byte rejection. A future enrichment can add a
//! `url::Url` round-trip without breaking the constructor signature.
//!
//! `Display` and `Serialize` emit the inner string verbatim.

use serde::{Serialize, Serializer};
use std::fmt;

/// URL constrained to the `http(s)` scheme with no embedded
/// control bytes. Constructors reject anything else, so a value
/// of this type is safe to interpolate into a markdown link target
/// or an HTML `href` without further normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SafeUrl(String);

/// Reason a [`SafeUrl`] construction was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeUrlError {
    /// Scheme was not `http://` or `https://`.
    DisallowedScheme,
    /// Input contained `\r`, `\n`, or NUL — a header-injection
    /// or fence-escape vector.
    EmbeddedControlByte,
    /// Input was empty.
    Empty,
}

impl fmt::Display for SafeUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisallowedScheme => f.write_str("URL scheme must be http or https"),
            Self::EmbeddedControlByte => {
                f.write_str("URL contains a forbidden control byte (\\r, \\n, or NUL)")
            }
            Self::Empty => f.write_str("URL is empty"),
        }
    }
}

impl std::error::Error for SafeUrlError {}

impl SafeUrl {
    /// Construct, rejecting non-http(s) schemes and inputs with
    /// embedded control bytes. The check is structural; no parsing
    /// dep is pulled in.
    ///
    /// # Errors
    ///
    /// Returns [`SafeUrlError::Empty`] for an empty input,
    /// [`SafeUrlError::EmbeddedControlByte`] for inputs containing
    /// `\r`, `\n`, or NUL, and [`SafeUrlError::DisallowedScheme`]
    /// for anything outside `http://` and `https://`.
    pub fn parse(s: impl Into<String>) -> Result<Self, SafeUrlError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SafeUrlError::Empty);
        }
        if s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(SafeUrlError::EmbeddedControlByte);
        }
        let lower_prefix = s
            .get(..8)
            .map_or_else(|| s.to_ascii_lowercase(), str::to_ascii_lowercase);
        if !(lower_prefix.starts_with("http://") || lower_prefix.starts_with("https://")) {
            return Err(SafeUrlError::DisallowedScheme);
        }
        Ok(Self(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SafeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SafeUrl {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https() {
        assert!(SafeUrl::parse("http://example.com/foo").is_ok());
        assert!(SafeUrl::parse("https://example.com/foo?bar=1").is_ok());
    }

    #[test]
    fn rejects_javascript_scheme() {
        assert_eq!(
            SafeUrl::parse("javascript:alert(1)"),
            Err(SafeUrlError::DisallowedScheme),
        );
    }

    #[test]
    fn rejects_data_scheme() {
        assert_eq!(
            SafeUrl::parse("data:text/html,<script>1</script>"),
            Err(SafeUrlError::DisallowedScheme),
        );
    }

    #[test]
    fn rejects_embedded_newline() {
        assert_eq!(
            SafeUrl::parse("https://example.com/\nInjected: header"),
            Err(SafeUrlError::EmbeddedControlByte),
        );
    }

    #[test]
    fn rejects_embedded_cr() {
        assert_eq!(
            SafeUrl::parse("https://example.com/\rfoo"),
            Err(SafeUrlError::EmbeddedControlByte),
        );
    }

    #[test]
    fn rejects_nul() {
        assert_eq!(
            SafeUrl::parse("https://example.com/\0foo"),
            Err(SafeUrlError::EmbeddedControlByte),
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(SafeUrl::parse(""), Err(SafeUrlError::Empty));
    }

    #[test]
    fn scheme_check_is_case_insensitive() {
        assert!(SafeUrl::parse("HTTPS://EXAMPLE.com/").is_ok());
        assert!(SafeUrl::parse("HtTp://example.com/").is_ok());
    }

    #[test]
    fn display_emits_inner_verbatim() {
        let u = SafeUrl::parse("https://example.com/x").unwrap();
        assert_eq!(format!("{u}"), "https://example.com/x");
    }

    #[test]
    fn serializes_as_string() {
        let u = SafeUrl::parse("https://example.com/x").unwrap();
        assert_eq!(
            serde_json::to_string(&u).unwrap(),
            "\"https://example.com/x\""
        );
    }
}
