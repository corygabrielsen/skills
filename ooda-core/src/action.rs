//! `Action` and its companion enums.
//!
//! An `Action<K>` is the unit decide prescribes. Its fields encode:
//!   * `kind` — domain-specific operation, supplied as the type
//!     parameter `K`.
//!   * `effect` — dispatch class AND the correlated caller-facing
//!     payload, fused so the "handoff variants carry a prompt,
//!     in-loop variants carry a log line" pairing is uninhabitable
//!     for any other combination.
//!   * `target_effect` — how dispatching changes blocker/tier state.
//!   * `urgency` — declared sort priority for candidate ordering.
//!   * `blocker` — stable `(kind, blocker)` stall key.
//!
//! `K` must implement [`ActionKindName`] so renderers can emit a
//! payload-free variant identifier without exposing payload internals.

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
    /// Declared sort priority. Each action names its urgency at
    /// construction; ordering is `Ord` on the enum (smallest first).
    pub urgency: Urgency,
    /// Stable iteration key. The stall detector compares
    /// `(kind_name, blocker)` across iterations; the [`BlockerKey`]
    /// newtype enforces the "MUST NOT embed varying counts or
    /// progress markers" invariant via its constructors.
    pub blocker: BlockerKey,
}

/// The handoff projection of an [`Action`] — what the handoff
/// halt variants of [`crate::Outcome`] and [`crate::DecisionHalt`]
/// carry.
///
/// Structurally, this is `Action<K>` with `effect` replaced by a
/// direct `prompt: HandoffPrompt`. The agent-vs-human distinction
/// is conveyed by the outer enum variant; the inner shape does
/// not carry it.
///
/// The projection makes "this halt variant carries a prompt"
/// structurally true: no decorator needs to pattern-match past an
/// uninhabitable arm to reach the prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandoffAction<K> {
    pub kind: K,
    pub prompt: HandoffPrompt,
    pub target_effect: TargetEffect,
    pub urgency: Urgency,
    pub blocker: BlockerKey,
}

/// Dispatch class fused with its caller-facing payload. The four
/// variants are the only reachable pairings on the
/// `dispatch × payload` product; fusing them in one sum type makes
/// the "handoff variants carry a prompt; in-loop variants carry a
/// log line" invariant structural.
///
/// `Wait` carries its cadence as a [`PollingInterval`] (strictly
/// positive by construction), so a Wait without a duration AND a
/// Wait with zero duration are both unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionEffect {
    /// Driven inside the loop without halting. The log line is a
    /// trace artifact, never surfaced to an external actor.
    Full { log: String },
    /// Driven inside the loop after sleeping `interval`. Same
    /// audience for `log` as [`Self::Full`].
    Wait {
        interval: PollingInterval,
        log: String,
    },
    /// Halts the loop and surfaces the prompt to an agent.
    Agent { prompt: HandoffPrompt },
    /// Halts the loop and surfaces the prompt to a human.
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

    /// Mutable borrow of the inner `HandoffPrompt`. Used by
    /// boundary decorators that append context to a handoff's
    /// prompt at the binary boundary.
    pub fn prompt_mut(&mut self) -> Option<&mut HandoffPrompt> {
        match self {
            Self::Agent { prompt } | Self::Human { prompt } => Some(prompt),
            Self::Full { .. } | Self::Wait { .. } => None,
        }
    }

    /// `true` iff this is the `Wait` variant. The stall detector
    /// exempts `Wait` iterations from equality comparison — polling
    /// the same blocker is forward progress by definition.
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
    /// Call sites that need only the textual payload use this;
    /// sites that mutate prompt structure use [`Self::prompt_mut`].
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

/// Payload-free identifier for an action-kind variant. The
/// `&'static str` lifetime is the stability witness — only
/// compile-time-known per-variant tokens satisfy the contract.
/// Used wherever a stable single-token rendering is needed without
/// exposing payload internals (log lines, stderr headers).
pub trait ActionKindName {
    fn name(&self) -> &'static str;
}

/// Sort order for candidate actions. `Ord` ascending = selection
/// order; lower variants are picked first. Each tier names a
/// distinct semantic class; adding a class is a single enum
/// extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Urgency {
    /// In-loop actions that make unconditional forward progress.
    /// MUST preempt any blocking handoff — passing one up to pick
    /// a Wait/Human halt ends the iteration with the target still
    /// gated despite progress being available.
    Critical,
    /// Active fix for a blocking issue (handoff to a dispatcher
    /// that can address the blocker).
    BlockingFix,
    /// Passive wait for a blocking issue (poll-and-re-observe).
    BlockingWait,
    /// Blocking issue only a human can resolve.
    BlockingHuman,
    /// Active advancement that does not unblock but raises the
    /// target's tier.
    Advancing,
    /// Non-blocking metadata cleanup. Sorts ahead of
    /// [`Self::Closeout`] so cleanup completes before the final
    /// sign-off gate fires.
    Hygiene,
    /// Strictly least-urgent tier — the final sign-off gate. By
    /// being last, its handoff is conditional on every other axis
    /// being silent.
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
