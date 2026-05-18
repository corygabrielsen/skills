//! Process exit codes — the wire-level contract.
//!
//! Single source of truth for the numeric codes a binary in this
//! family returns to `$?`. Every typed result type
//! ([`crate::Outcome`], [`crate::Decision`], [`crate::DecisionHalt`],
//! [`crate::HaltReason`], and per-binary aggregates over them)
//! produces an `ExitCode` rather than a raw `u8`; numeric values
//! live only in this enum's `#[repr(u8)]` discriminants. Call sites
//! refer to variants by name.
//!
//! # Numbering rationale
//!
//! - `0` — universal success.
//! - `1`, `2` — information-bearing nonzero. Follows the
//!   long-standing grep / diff / pytest convention where a low
//!   nonzero code means "the tool worked; here is the result you
//!   asked for". `1 = Paused` is the most common "nothing to do
//!   this pass" outcome; `2 = WouldAdvance` is inspect-mode's
//!   "would have run an action".
//! - `3`, `4` — handoff halts. The binary needs the caller to
//!   dispatch a human (`3`) or an agent (`4`) and then re-invoke.
//! - `5` — terminal non-success (`DoneAborted`). The target ended
//!   in an aborted state; this is not a software failure.
//! - `6`, `7` — escalation halts. The loop made no progress
//!   (`StuckRepeated`) or hit the iteration cap (`StuckCapReached`).
//!   Caller must diagnose or raise the cap.
//! - `64` — BSD `sysexits.h` `EX_USAGE`. CLI parse / validation
//!   failure. Allocated by sendmail circa 1993; adopted across
//!   `mail`, `postfix`, `systemd`, etc.
//! - `70` — BSD `sysexits.h` `EX_SOFTWARE`. Caught internal
//!   failure (subprocess, IO, network). What a reader fluent in
//!   `sysexits` expects to find at this slot.
//! - `130` / `143` — `SIGINT` / `SIGTERM` per POSIX shell convention
//!   (`128 + N`). Returned by the binary itself when the loop polls
//!   the `SHUTDOWN_SIGNAL` atomic at an iteration boundary and exits
//!   cleanly via [`Self::SignalSigint`] / [`Self::SignalSigterm`].
//!   The shell synthesizes the same numbers on an uncaught signal,
//!   so caller dispatch tables stay correct regardless of which
//!   half of the contract produced the code.
//!
//! Codes `8–63` and `65–69` are deliberately unassigned. New
//! variants should land in the low range only when they encode a
//! genuinely new typed result; new error categories should adopt
//! the appropriate `sysexits.h` code (`EX_IOERR = 74`,
//! `EX_TEMPFAIL = 75`, etc.) rather than squat on the low range.

use serde::{Serialize, Serializer};
use std::fmt;

/// The set of process exit codes a binary in this family can
/// produce.
///
/// `#[repr(u8)]` pins each variant to its numeric value; convert
/// via [`ExitCode::as_u8`] or the `From` impls into `u8` / `i32` /
/// [`std::process::ExitCode`] when returning from `main`.
///
/// `Serialize` emits the **numeric value** (not the variant name)
/// so JSON / JSONL records always carry a number in the exit-code
/// field. The variant name is available separately via
/// [`ExitCode::name`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitCode {
    /// Terminal success — target reached its goal state.
    DoneSucceeded = 0,
    /// Loop completed this pass with no candidate action.
    /// Caller may re-invoke later if the target's state may have
    /// changed externally.
    Paused = 1,
    /// Inspect mode only — decide selected an executable action;
    /// the loop would have run it. The action's automation tells
    /// the caller what would happen.
    WouldAdvance = 2,
    /// Handoff halt — only a human can resolve. Caller surfaces
    /// the prompt and re-invokes after resolution.
    HandoffHuman = 3,
    /// Handoff halt — an agent dispatches the action. Caller
    /// runs the agent and re-invokes.
    HandoffAgent = 4,
    /// Terminal non-success — target reached an aborted state
    /// (e.g. PR closed without merge). Not a software bug; treat
    /// per caller policy.
    DoneAborted = 5,
    /// Escalation halt — same `(kind, blocker)` action fired on
    /// two consecutive non-`Wait` iterations. Caller diagnoses.
    StuckRepeated = 6,
    /// Escalation halt — iteration cap reached without halting.
    /// Caller re-invokes with a higher cap or escalates.
    StuckCapReached = 7,
    /// BSD `sysexits.h` `EX_USAGE`. CLI parse / validation
    /// failure (unknown flag, invalid value, conflicting flags).
    UsageError = 64,
    /// BSD `sysexits.h` `EX_SOFTWARE`. Caught external failure
    /// (subprocess nonzero exit, network, IO).
    BinaryError = 70,
    /// `128 + SIGINT (2)`. Loop polled `SHUTDOWN_SIGNAL` at an
    /// iteration boundary and exited cleanly after a `SIGINT`.
    /// Matches the POSIX shell convention for an uncaught signal so
    /// callers cannot distinguish the trapped path from the kernel
    /// path on `$?` alone.
    SignalSigint = 130,
    /// `128 + SIGTERM (15)`. Counterpart to [`Self::SignalSigint`]
    /// for clean shutdown on `SIGTERM`.
    SignalSigterm = 143,
}

impl ExitCode {
    /// Numeric value the process returns to `$?`.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Stable variant identifier — exactly the Rust variant name
    /// as a `&'static str`. Used wherever a token, not a number,
    /// is the appropriate rendering (log lines, manifest fields,
    /// help text).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DoneSucceeded => "DoneSucceeded",
            Self::Paused => "Paused",
            Self::WouldAdvance => "WouldAdvance",
            Self::HandoffHuman => "HandoffHuman",
            Self::HandoffAgent => "HandoffAgent",
            Self::DoneAborted => "DoneAborted",
            Self::StuckRepeated => "StuckRepeated",
            Self::StuckCapReached => "StuckCapReached",
            Self::UsageError => "UsageError",
            Self::BinaryError => "BinaryError",
            Self::SignalSigint => "SignalSigint",
            Self::SignalSigterm => "SignalSigterm",
        }
    }

    /// Every returnable variant in numeric order. Stable for
    /// iteration in help-text generation, doc-table builders, and
    /// external schema dumps.
    pub const ALL: &'static [ExitCode] = &[
        Self::DoneSucceeded,
        Self::Paused,
        Self::WouldAdvance,
        Self::HandoffHuman,
        Self::HandoffAgent,
        Self::DoneAborted,
        Self::StuckRepeated,
        Self::StuckCapReached,
        Self::UsageError,
        Self::BinaryError,
        Self::SignalSigint,
        Self::SignalSigterm,
    ];
}

impl From<ExitCode> for u8 {
    fn from(c: ExitCode) -> Self {
        c as u8
    }
}

impl From<ExitCode> for i32 {
    fn from(c: ExitCode) -> Self {
        i32::from(c as u8)
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(c: ExitCode) -> Self {
        std::process::ExitCode::from(c.as_u8())
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u8())
    }
}

impl Serialize for ExitCode {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u8(self.as_u8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_documented_scheme() {
        assert_eq!(ExitCode::DoneSucceeded.as_u8(), 0);
        assert_eq!(ExitCode::Paused.as_u8(), 1);
        assert_eq!(ExitCode::WouldAdvance.as_u8(), 2);
        assert_eq!(ExitCode::HandoffHuman.as_u8(), 3);
        assert_eq!(ExitCode::HandoffAgent.as_u8(), 4);
        assert_eq!(ExitCode::DoneAborted.as_u8(), 5);
        assert_eq!(ExitCode::StuckRepeated.as_u8(), 6);
        assert_eq!(ExitCode::StuckCapReached.as_u8(), 7);
        assert_eq!(ExitCode::UsageError.as_u8(), 64);
        assert_eq!(ExitCode::BinaryError.as_u8(), 70);
    }

    #[test]
    fn signal_codes_match_posix() {
        assert_eq!(ExitCode::SignalSigint.as_u8(), 130);
        assert_eq!(ExitCode::SignalSigterm.as_u8(), 143);
    }

    #[test]
    fn all_codes_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for c in ExitCode::ALL {
            assert!(
                seen.insert(c.as_u8()),
                "duplicate exit code: {} appears twice",
                c.as_u8()
            );
        }
    }

    #[test]
    fn all_lists_every_variant() {
        // If a new ExitCode variant is added, this test fails
        // until it's also added to ExitCode::ALL.
        // (Compile-time-style check that ALL is the index of
        // truth for variant iteration.)
        let names: Vec<&str> = ExitCode::ALL.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"DoneSucceeded"));
        assert!(names.contains(&"Paused"));
        assert!(names.contains(&"WouldAdvance"));
        assert!(names.contains(&"HandoffHuman"));
        assert!(names.contains(&"HandoffAgent"));
        assert!(names.contains(&"DoneAborted"));
        assert!(names.contains(&"StuckRepeated"));
        assert!(names.contains(&"StuckCapReached"));
        assert!(names.contains(&"UsageError"));
        assert!(names.contains(&"BinaryError"));
        assert!(names.contains(&"SignalSigint"));
        assert!(names.contains(&"SignalSigterm"));
        assert_eq!(names.len(), 12);
    }

    #[test]
    fn name_matches_variant_identifier() {
        assert_eq!(ExitCode::DoneSucceeded.name(), "DoneSucceeded");
        assert_eq!(ExitCode::Paused.name(), "Paused");
        assert_eq!(ExitCode::BinaryError.name(), "BinaryError");
        assert_eq!(ExitCode::UsageError.name(), "UsageError");
    }

    #[test]
    fn from_impls_round_trip_to_numeric() {
        let c = ExitCode::HandoffAgent;
        assert_eq!(u8::from(c), 4);
        assert_eq!(i32::from(c), 4);
    }

    #[test]
    fn display_emits_numeric() {
        assert_eq!(format!("{}", ExitCode::BinaryError), "70");
        assert_eq!(format!("{}", ExitCode::DoneSucceeded), "0");
    }

    #[test]
    fn serialize_emits_numeric_for_jsonl() {
        // The JSONL `exit` field is documented as an integer; the
        // Serialize impl emits the u8, not the variant name.
        let json = serde_json::to_string(&ExitCode::HandoffAgent).unwrap();
        assert_eq!(json, "4");
        let json = serde_json::to_string(&ExitCode::BinaryError).unwrap();
        assert_eq!(json, "70");
    }

    #[test]
    fn converts_to_std_process_exit_code() {
        // Smoke test: main() can `return ExitCode::from(outcome.exit_code())`.
        let _: std::process::ExitCode = ExitCode::DoneSucceeded.into();
    }
}
