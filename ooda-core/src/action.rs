//! `Action` and its companion enums.
//!
//! Each `Action<K>` carries:
//!   * `kind`: a domain-specific enum naming the action and its
//!     payload — supplied by the binary as the type parameter `K`.
//!   * `automation`: who executes (us, an agent, a human, just wait).
//!   * `target_effect`: how this action changes blocker/tier state.
//!   * `urgency`: declared sort priority for candidate ordering.
//!   * `description`: human-readable prompt material for handoff.
//!   * `blocker`: stable `(kind, blocker)` stall key.
//!
//! `K` must implement `ActionKindName` so the loop can render variant
//! names for the per-iteration `[iter N] action: <name>` log line
//! without exposing payload internals.

use crate::blocker::BlockerKey;
use crate::handoff_prompt::HandoffPrompt;
use crate::polling_interval::PollingInterval;
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action<K> {
    pub kind: K,
    pub automation: Automation,
    pub target_effect: TargetEffect,
    /// Declared sort priority — replaces an emergent tuple-comparator
    /// rule. Each action names its urgency at construction; the sort
    /// is `urgency as u8` ascending.
    pub urgency: Urgency,
    /// Caller-facing payload. [`ActionPayload::Logged`] is a plain
    /// trace message for `Full` / `Wait` actions (appears in iter
    /// logs and trace files; never surfaced to a caller-side agent
    /// or human). [`ActionPayload::Prompt`] is a structured
    /// [`HandoffPrompt`] for `Agent` / `Human` actions (surfaced
    /// to the caller via the stderr prompt block and recorded for
    /// audit).
    pub payload: ActionPayload,
    /// Stable iteration key — `run_loop` detects stalls by comparing
    /// `(kind, blocker)` against the prior iteration. The
    /// [`BlockerKey`] newtype prevents accidental confusion with
    /// `payload` (also human-readable) and documents that the
    /// value MUST NOT embed varying counts or progress markers.
    pub blocker: BlockerKey,
}

/// Dual-purpose human-readable payload on an [`Action`].
///
/// The distinction between the two variants is the rendering
/// audience:
///
/// * `Logged` is *trace material* — printed to iter-log lines and
///   the `trace.md` summary inside the recorder tree. The recipient
///   is a human reading the audit trail later, not the caller's
///   agent / human currently in the loop. Free-form `String`
///   (may be multi-line).
///
/// * `Prompt` is *handoff material* — the structured body the
///   caller surfaces when the binary halts on an Agent or Human
///   action. The [`HandoffPrompt`] type gives this material
///   compositional structure (headline + sections) so renderers
///   and recorders see the shape, not just bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionPayload {
    Logged(String),
    Prompt(HandoffPrompt),
}

impl ActionPayload {
    /// Borrow the inner `HandoffPrompt` if this is a `Prompt`
    /// payload. Returns `None` for `Logged`.
    pub fn as_prompt(&self) -> Option<&HandoffPrompt> {
        match self {
            Self::Prompt(p) => Some(p),
            Self::Logged(_) => None,
        }
    }

    /// Mutable borrow of the inner `HandoffPrompt`. Used by the
    /// boundary `decorate_handoff_*` decorators that append
    /// context lines to a handoff's prompt.
    pub fn as_prompt_mut(&mut self) -> Option<&mut HandoffPrompt> {
        match self {
            Self::Prompt(p) => Some(p),
            Self::Logged(_) => None,
        }
    }
}

impl fmt::Display for ActionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Logged(s) => f.write_str(s),
            Self::Prompt(p) => fmt::Display::fmt(p, f),
        }
    }
}

impl<K> Action<K> {
    /// Render the payload as a single `String` regardless of
    /// variant. Call sites that just need "the human-readable
    /// payload as text" (comment renderer, JSONL emission,
    /// stderr prompt block) use this; sites that match on
    /// structure (decorate_handoff_*) use `payload` directly.
    pub fn rendered_payload(&self) -> String {
        self.payload.to_string()
    }
}

/// Stable, finite, single-token rendering for the action-kind
/// variant. Used in the per-iteration log line and in stderr-header
/// payloads where exposing full payloads would break the
/// one-line-per-iteration diagnostic invariant.
pub trait ActionKindName {
    fn name(&self) -> &'static str;
}

/// Sort order for candidate actions. Lower variants are higher
/// priority. The split between `BlockingFix` / `BlockingWait` /
/// `BlockingHuman` encodes the "active fix beats passive handoff"
/// rule directly in the enum rather than a comparator tuple — adding
/// a new urgency tier is a single enum addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Urgency {
    /// Full-automation actions. The loop runs them without halting,
    /// so they MUST preempt any blocking handoff — picking a Full
    /// action is free progress, while a blocking Wait/Human means
    /// the iteration ends with the target still gated.
    Critical,
    /// Active fixes for blocking issues — Agent automation that
    /// addresses the blocker.
    BlockingFix,
    /// Passive waits for blocking issues — Wait automation, the
    /// loop sleeps and re-observes.
    BlockingWait,
    /// Human handoffs for blocking issues — only a human can
    /// resolve.
    BlockingHuman,
    /// Active advancement that doesn't unblock but improves the
    /// target. Non-Full Advances actions.
    Advancing,
    /// Non-blocking metadata cleanup. Always sorts last regardless
    /// of automation.
    Hygiene,
}

/// What dispatches the action. `Wait` carries the poll cadence as
/// a [`PollingInterval`] (strictly-positive newtype), so both
/// "Wait without a sleep duration" *and* "Wait with a zero
/// duration" are unrepresentable — the latter would busy-loop the
/// runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Automation {
    /// We have the exact command and run it directly.
    Full,
    /// Hand off to an agent with `description` as prompt.
    Agent,
    /// Wait for an external signal — poll after `interval` and
    /// re-iterate.
    Wait { interval: PollingInterval },
    /// Halt and surface to a human — only they can resolve.
    Human,
}

/// What dispatching this action would do to the blocker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TargetEffect {
    /// Action is the path past a current blocker.
    Blocks,
    /// Action moves the target to a higher tier without unblocking.
    Advances,
    /// Action is informational; no blocker/tier impact.
    Neutral,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    enum TestKind {
        Foo,
        Bar,
    }

    impl ActionKindName for TestKind {
        fn name(&self) -> &'static str {
            match self {
                Self::Foo => "Foo",
                Self::Bar => "Bar",
            }
        }
    }

    fn dummy(kind: TestKind) -> Action<TestKind> {
        Action {
            kind,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            payload: crate::ActionPayload::Logged("test".into()),
            blocker: BlockerKey::tag("test:blocker"),
        }
    }

    #[test]
    fn urgency_sorts_critical_first() {
        let mut us = [
            Urgency::Hygiene,
            Urgency::Critical,
            Urgency::BlockingHuman,
            Urgency::BlockingFix,
            Urgency::Advancing,
            Urgency::BlockingWait,
        ];
        us.sort();
        assert_eq!(
            us,
            [
                Urgency::Critical,
                Urgency::BlockingFix,
                Urgency::BlockingWait,
                Urgency::BlockingHuman,
                Urgency::Advancing,
                Urgency::Hygiene,
            ]
        );
    }

    #[test]
    fn action_carries_typed_kind() {
        let a = dummy(TestKind::Foo);
        assert_eq!(a.kind.name(), "Foo");
        assert_eq!(a.kind, TestKind::Foo);
    }

    #[test]
    fn action_kind_name_is_payload_free() {
        assert_eq!(TestKind::Foo.name(), "Foo");
        assert_eq!(TestKind::Bar.name(), "Bar");
    }
}
