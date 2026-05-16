//! Pull-request lifecycle state — `Open` vs `Terminal(_)`.
//!
//! Modeled as a sum so the "open vs done" partition is structural:
//! every site that branches on lifecycle reaches the terminal case
//! through one arm (`PullRequestState::Terminal(t)`), and the inner
//! `TerminalState` carries the success/abort distinction.
//!
//! Wire format mirrors the GitHub GraphQL `PullRequestState` enum:
//! deserializes from `"OPEN"` / `"MERGED"` / `"CLOSED"` and
//! serializes back to those flat strings, so on-disk recorder
//! state and JSONL records stay byte-identical across the family.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Terminal(TerminalState),
}

/// PR terminal lifecycle states. `Merged` is the success terminal;
/// `Closed` is the abort terminal. Lifted out of `PullRequestState` so the
/// "this PR is done" check is `matches!(state, PullRequestState::Terminal(_))`
/// without enumerating arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Merged,
    Closed,
}

impl<'de> Deserialize<'de> for PullRequestState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "OPEN" => Ok(PullRequestState::Open),
            "MERGED" => Ok(PullRequestState::Terminal(TerminalState::Merged)),
            "CLOSED" => Ok(PullRequestState::Terminal(TerminalState::Closed)),
            other => Err(serde::de::Error::custom(format!(
                "unknown PR state: {other}"
            ))),
        }
    }
}

impl Serialize for PullRequestState {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            PullRequestState::Open => "OPEN",
            PullRequestState::Terminal(TerminalState::Merged) => "MERGED",
            PullRequestState::Terminal(TerminalState::Closed) => "CLOSED",
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
            serde_json::from_str::<PullRequestState>("\"OPEN\"").unwrap(),
            PullRequestState::Open
        );
        assert_eq!(
            serde_json::from_str::<PullRequestState>("\"MERGED\"").unwrap(),
            PullRequestState::Terminal(TerminalState::Merged)
        );
        assert_eq!(
            serde_json::from_str::<PullRequestState>("\"CLOSED\"").unwrap(),
            PullRequestState::Terminal(TerminalState::Closed)
        );
    }

    #[test]
    fn rejects_unknown_state() {
        assert!(serde_json::from_str::<PullRequestState>("\"DRAFT\"").is_err());
    }

    #[test]
    fn serializes_to_graphql_strings() {
        assert_eq!(
            serde_json::to_string(&PullRequestState::Open).unwrap(),
            "\"OPEN\""
        );
        assert_eq!(
            serde_json::to_string(&PullRequestState::Terminal(TerminalState::Merged)).unwrap(),
            "\"MERGED\""
        );
        assert_eq!(
            serde_json::to_string(&PullRequestState::Terminal(TerminalState::Closed)).unwrap(),
            "\"CLOSED\""
        );
    }
}
