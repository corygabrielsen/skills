//! `Action` and its companion enums.
//!
//! Each `Action<K>` carries:
//!   * `kind`: a domain-specific enum naming the action and its
//!     payload — supplied by the binary as the type parameter `K`.
//!   * `effect`: who executes (us / agent / human / wait) AND the
//!     correlated human-readable payload. The two are fused into a
//!     single tagged enum so the "Agent/Human carry a prompt,
//!     Full/Wait carry a log line" class invariant is structural
//!     rather than enforced at construction.
//!   * `target_effect`: how this action changes blocker/tier state.
//!   * `urgency`: declared sort priority for candidate ordering.
//!   * `blocker`: stable `(kind, blocker)` stall key.
//!
//! `K` must implement `ActionKindName` so the loop can render variant
//! names for the per-iteration `[iter N] action: <name>` log line
//! without exposing payload internals.

use crate::blocker::BlockerKey;
use crate::handoff_prompt::HandoffPrompt;
use crate::polling_interval::PollingInterval;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action<K> {
    pub kind: K,
    /// Fused automation kind + caller-facing payload. The four
    /// variants encode the only valid (automation, payload) pairings;
    /// any other combination is uninhabitable.
    pub effect: ActionEffect,
    pub target_effect: TargetEffect,
    /// Declared sort priority — replaces an emergent tuple-comparator
    /// rule. Each action names its urgency at construction; the sort
    /// is `urgency as u8` ascending.
    pub urgency: Urgency,
    /// Stable iteration key — `run_loop` detects stalls by comparing
    /// `(kind, blocker)` against the prior iteration. The
    /// [`BlockerKey`] newtype prevents accidental confusion with the
    /// effect's payload (also human-readable) and documents that the
    /// value MUST NOT embed varying counts or progress markers.
    pub blocker: BlockerKey,
}

/// The handoff projection of an [`Action`] — the payload of
/// `Outcome::HandoffAgent` / `Outcome::HandoffHuman` and
/// `DecisionHalt::AgentNeeded` / `HumanNeeded`.
///
/// Structurally, this is `Action<K>` with the `effect` field
/// replaced by a direct `prompt: HandoffPrompt`. The Agent-vs-
/// Human distinction is conveyed by which enum variant wraps
/// the value (`HandoffAgent` vs `HandoffHuman`); the inner
/// shape itself doesn't carry the distinction.
///
/// Replacing `Action<K>`'s `effect` with `prompt` makes "this
/// halt variant carries a prompt" structurally true — there's
/// no `match` on a sum type to discriminate Logged-vs-Prompt or
/// Full/Wait/Agent/Human. Decorators that previously had to
/// `match action.effect { Human { prompt } => prompt, _ =>
/// unreachable!() }` now just access `handoff_action.prompt`
/// directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandoffAction<K> {
    pub kind: K,
    pub prompt: HandoffPrompt,
    pub target_effect: TargetEffect,
    pub urgency: Urgency,
    pub blocker: BlockerKey,
}

/// What dispatches the action AND the human-readable payload that
/// goes with it. The four variants are the diagonal of the
/// Cartesian product `Automation × Payload` — the only reachable
/// pairings. Encoding them as a single sum type makes the class
/// invariant ("Agent/Human carry a `HandoffPrompt`; Full/Wait
/// carry a log line") structural rather than runtime-checked.
///
/// `Wait` carries the poll cadence as a [`PollingInterval`]
/// (strictly-positive newtype), so both "Wait without a sleep
/// duration" *and* "Wait with a zero duration" are unrepresentable
/// — the latter would busy-loop the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionEffect {
    /// Loop dispatches the action directly. The log line appears in
    /// iter logs and trace files; never surfaced to a caller-side
    /// agent or human.
    Full { log: String },
    /// Loop sleeps for `interval` then re-iterates. Same audience
    /// for `log` as [`Self::Full`].
    Wait {
        interval: PollingInterval,
        log: String,
    },
    /// Loop halts and surfaces the prompt to an agent.
    Agent { prompt: HandoffPrompt },
    /// Loop halts and surfaces the prompt to a human.
    Human { prompt: HandoffPrompt },
}

impl ActionEffect {
    /// Borrow the inner `HandoffPrompt` if this is a handoff
    /// variant. Returns `None` for `Full` / `Wait`.
    #[must_use]
    pub fn prompt(&self) -> Option<&HandoffPrompt> {
        match self {
            Self::Agent { prompt } | Self::Human { prompt } => Some(prompt),
            Self::Full { .. } | Self::Wait { .. } => None,
        }
    }

    /// Mutable borrow of the inner `HandoffPrompt`. Used by the
    /// boundary `decorate_handoff_*` decorators that append
    /// context lines to a handoff's prompt.
    pub fn prompt_mut(&mut self) -> Option<&mut HandoffPrompt> {
        match self {
            Self::Agent { prompt } | Self::Human { prompt } => Some(prompt),
            Self::Full { .. } | Self::Wait { .. } => None,
        }
    }

    /// `true` iff this is the `Wait` variant. Used by the runner's
    /// stall detector to skip the equality check on `Wait`
    /// iterations (polling for the same blocker isn't a stall).
    #[must_use]
    pub fn is_wait(&self) -> bool {
        matches!(self, Self::Wait { .. })
    }

    /// `true` iff this effect requires halting the loop and
    /// handing off to an external actor (agent or human).
    #[must_use]
    pub fn is_handoff(&self) -> bool {
        matches!(self, Self::Agent { .. } | Self::Human { .. })
    }

    /// Render the payload as a single `String` regardless of variant.
    /// Call sites that just need "the human-readable payload as text"
    /// (comment renderer, JSONL emission, handoff.md body) use
    /// this; sites that match on structure (`decorate_handoff`_*) use
    /// [`Self::prompt_mut`].
    #[must_use]
    pub fn rendered_message(&self) -> String {
        match self {
            Self::Full { log } | Self::Wait { log, .. } => log.clone(),
            Self::Agent { prompt } | Self::Human { prompt } => prompt.to_string(),
        }
    }
}

impl<K> Action<K> {
    /// Convenience pass-through — same as `self.effect.rendered_message()`.
    #[must_use]
    pub fn rendered_payload(&self) -> String {
        self.effect.rendered_message()
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
    /// Non-blocking metadata cleanup. Sorts ahead of [`Self::Closeout`]
    /// so per-axis hygiene attestations clear before the final-state
    /// closeout gate.
    Hygiene,
    /// Final pre-handoff sign-off. Strictly the least-urgent tier —
    /// emitted only by the Closeout axis. The reducer selects it only
    /// when every other axis's candidate set is empty, making
    /// `HandoffHuman` conditional on an agent-signed attestation at
    /// current HEAD.
    Closeout,
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
            effect: ActionEffect::Full { log: "test".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::from_static("test:blocker"),
        }
    }

    #[test]
    fn urgency_sorts_critical_first() {
        let mut us = [
            Urgency::Closeout,
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
                Urgency::Closeout,
            ]
        );
    }

    #[test]
    fn closeout_is_strictly_least_urgent() {
        assert!(Urgency::Hygiene < Urgency::Closeout);
        assert!(Urgency::Advancing < Urgency::Closeout);
        assert!(Urgency::BlockingHuman < Urgency::Closeout);
        assert!(Urgency::BlockingWait < Urgency::Closeout);
        assert!(Urgency::BlockingFix < Urgency::Closeout);
        assert!(Urgency::Critical < Urgency::Closeout);
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

    #[test]
    fn effect_classifies_handoff_variants() {
        let agent = ActionEffect::Agent {
            prompt: HandoffPrompt::new("p"),
        };
        let human = ActionEffect::Human {
            prompt: HandoffPrompt::new("p"),
        };
        let full = ActionEffect::Full { log: "x".into() };
        let wait = ActionEffect::Wait {
            interval: PollingInterval::from_secs(30),
            log: "x".into(),
        };
        assert!(agent.is_handoff());
        assert!(human.is_handoff());
        assert!(!full.is_handoff());
        assert!(!wait.is_handoff());
        assert!(wait.is_wait());
        assert!(!full.is_wait());
        assert!(!agent.is_wait());
    }

    #[test]
    fn effect_prompt_mut_returns_some_only_for_handoff() {
        let mut a = ActionEffect::Agent {
            prompt: HandoffPrompt::new("p"),
        };
        let mut f = ActionEffect::Full { log: "x".into() };
        assert!(a.prompt_mut().is_some());
        assert!(f.prompt_mut().is_none());
    }

    #[test]
    fn effect_rendered_message_dispatches_to_payload() {
        let f = ActionEffect::Full {
            log: "fulled".into(),
        };
        let a = ActionEffect::Agent {
            prompt: HandoffPrompt::new("agented"),
        };
        assert_eq!(f.rendered_message(), "fulled");
        assert_eq!(a.rendered_message(), "agented");
    }
}
