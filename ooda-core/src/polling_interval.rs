//! Positive-duration newtype for polling waits.
//!
//! The runner's `ActionEffect::Wait { interval, .. }` puts the loop to
//! sleep for `interval` between observe passes. The interval is
//! intended to be a real wait (seconds to minutes for CI / bot
//! polling). [`PollingInterval`] makes that intent structural:
//! `Duration::ZERO` is unrepresentable, so a Wait action cannot
//! accidentally degenerate into a busy-loop. Constructors fail
//! fast on zero.
//!
//! No upper bound is enforced — interval policy (cap, jitter,
//! backoff) belongs to caller-chosen values, not the type. The
//! lower bound is the only structural guarantee.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::Duration;

/// `Duration` guaranteed to be strictly positive (`> 0`).
///
/// `Copy` matches `Duration`'s shape: the inner value is a 96-bit
/// pair (`u64` seconds + `u32` nanos) and copying is free.
///
/// Serializes transparently as the underlying `Duration` so
/// recorder state and JSONL records carrying interval fields are
/// byte-identical to the pre-newtype form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PollingInterval(Duration);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollingIntervalError;

impl fmt::Display for PollingIntervalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("polling interval must be > 0")
    }
}

impl std::error::Error for PollingIntervalError {}

impl PollingInterval {
    /// Validating constructor. Returns `Err` on `Duration::ZERO`;
    /// otherwise wraps the input.
    pub fn try_from_duration(d: Duration) -> Result<Self, PollingIntervalError> {
        if d.is_zero() {
            Err(PollingIntervalError)
        } else {
            Ok(Self(d))
        }
    }

    /// Construct from whole seconds. `secs` must be non-zero;
    /// panics otherwise. Matches the existing call sites that all
    /// use `Duration::from_secs(N)` with N ≥ 1.
    pub fn from_secs(secs: u64) -> Self {
        assert!(secs > 0, "PollingInterval::from_secs requires secs > 0");
        Self(Duration::from_secs(secs))
    }

    /// Construct from milliseconds. `millis` must be non-zero;
    /// panics otherwise.
    pub fn from_millis(millis: u64) -> Self {
        assert!(
            millis > 0,
            "PollingInterval::from_millis requires millis > 0"
        );
        Self(Duration::from_millis(millis))
    }

    /// Borrow the underlying `Duration`. Use this when handing
    /// the value to a `std::thread::sleep` or other API expecting
    /// `Duration`.
    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

impl From<PollingInterval> for Duration {
    fn from(p: PollingInterval) -> Self {
        p.0
    }
}

impl Serialize for PollingInterval {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(ser)
    }
}

impl<'de> Deserialize<'de> for PollingInterval {
    /// Mirrors [`Serialize`]: deserializes a `Duration` and
    /// validates strict positivity. Zero — which `Duration` does
    /// allow — fails with a serde error rather than silently
    /// reconstructing a degenerate value.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let dur = Duration::deserialize(d)?;
        Self::try_from_duration(dur).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_duration_rejects_zero() {
        assert!(PollingInterval::try_from_duration(Duration::ZERO).is_err());
    }

    #[test]
    fn try_from_duration_accepts_positive() {
        let p = PollingInterval::try_from_duration(Duration::from_secs(30)).unwrap();
        assert_eq!(p.as_duration(), Duration::from_secs(30));
    }

    #[test]
    fn from_secs_constructs_positive() {
        let p = PollingInterval::from_secs(60);
        assert_eq!(p.as_duration(), Duration::from_secs(60));
    }

    #[test]
    #[should_panic(expected = "PollingInterval::from_secs requires secs > 0")]
    fn from_secs_zero_panics() {
        let _ = PollingInterval::from_secs(0);
    }

    #[test]
    fn from_millis_constructs_positive() {
        let p = PollingInterval::from_millis(500);
        assert_eq!(p.as_duration(), Duration::from_millis(500));
    }

    #[test]
    #[should_panic(expected = "PollingInterval::from_millis requires millis > 0")]
    fn from_millis_zero_panics() {
        let _ = PollingInterval::from_millis(0);
    }

    #[test]
    fn convertible_to_duration_via_from() {
        let p = PollingInterval::from_secs(5);
        let d: Duration = p.into();
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn serialize_matches_underlying_duration() {
        let p = PollingInterval::from_secs(7);
        let d = Duration::from_secs(7);
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            serde_json::to_string(&d).unwrap()
        );
    }

    #[test]
    fn deserialize_roundtrips_positive() {
        let p = PollingInterval::from_secs(42);
        let json = serde_json::to_string(&p).unwrap();
        let back: PollingInterval = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn deserialize_rejects_zero() {
        let zero = Duration::ZERO;
        let json = serde_json::to_string(&zero).unwrap();
        assert!(serde_json::from_str::<PollingInterval>(&json).is_err());
    }
}
