//! Pull-request lifecycle state — `Open` vs `Terminal(_)`.
//!
//! Modeled as a sum so the "open vs done" partition is structural:
//! every site that branches on lifecycle reaches the terminal case
//! through one arm (`PrState::Terminal(t)`), and the inner
//! `TerminalState` carries the success/abort distinction.
//!
//! Wire format mirrors the GitHub GraphQL `PullRequestState` enum:
//! deserializes from `"OPEN"` / `"MERGED"` / `"CLOSED"` and
//! serializes back to those flat strings, so on-disk recorder
//! state and JSONL records stay byte-identical across the family.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Terminal(TerminalState),
}

/// PR terminal lifecycle states. `Merged` is the success terminal;
/// `Closed` is the abort terminal. Lifted out of `PrState` so the
/// "this PR is done" check is `matches!(state, PrState::Terminal(_))`
/// without enumerating arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Merged,
    Closed,
}

impl<'de> Deserialize<'de> for PrState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "OPEN" => Ok(PrState::Open),
            "MERGED" => Ok(PrState::Terminal(TerminalState::Merged)),
            "CLOSED" => Ok(PrState::Terminal(TerminalState::Closed)),
            other => Err(serde::de::Error::custom(format!(
                "unknown PR state: {other}"
            ))),
        }
    }
}

impl Serialize for PrState {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            PrState::Open => "OPEN",
            PrState::Terminal(TerminalState::Merged) => "MERGED",
            PrState::Terminal(TerminalState::Closed) => "CLOSED",
        };
        ser.serialize_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_graphql_strings() {
        assert_eq!(
            serde_json::from_str::<PrState>("\"OPEN\"").unwrap(),
            PrState::Open
        );
        assert_eq!(
            serde_json::from_str::<PrState>("\"MERGED\"").unwrap(),
            PrState::Terminal(TerminalState::Merged)
        );
        assert_eq!(
            serde_json::from_str::<PrState>("\"CLOSED\"").unwrap(),
            PrState::Terminal(TerminalState::Closed)
        );
    }

    #[test]
    fn rejects_unknown_state() {
        assert!(serde_json::from_str::<PrState>("\"DRAFT\"").is_err());
    }

    #[test]
    fn serializes_to_graphql_strings() {
        assert_eq!(serde_json::to_string(&PrState::Open).unwrap(), "\"OPEN\"");
        assert_eq!(
            serde_json::to_string(&PrState::Terminal(TerminalState::Merged)).unwrap(),
            "\"MERGED\""
        );
        assert_eq!(
            serde_json::to_string(&PrState::Terminal(TerminalState::Closed)).unwrap(),
            "\"CLOSED\""
        );
    }
}
