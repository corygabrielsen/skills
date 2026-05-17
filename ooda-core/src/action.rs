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

/// Sort order for candidate actions, three-phase. `Ord` ascending =
/// selection order; lower variants are picked first. The merge
/// function is `argmin` over the lex order: `Pre < Mid(_) < Post`,
/// and within `Mid` the `MidTier` enum's declaration order.
///
/// The three-phase structure separates *cycle-position* from
/// *cycle-internal priority*. Pre and Post are book-ends — agent
/// work that must precede or follow the iterative cycle. Mid is
/// the iterative cycle itself, with its own internal ordering.
///
/// Adding a new Mid-cycle priority class is a single `MidTier`
/// extension. Adding a new phase is an `Urgency` extension; phases
/// are deliberately scarce — three is the natural count for any
/// task with setup + work + sign-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Urgency {
    /// Pre-cycle gate — agent precondition work. Strictly the
    /// most-urgent phase. Emitted by axes whose witness must be in
    /// place before any state-mutating action triggers downstream
    /// (e.g., SHA-keyed attestations on a fresh commit; reviewers
    /// trigger on push, so the agent's witness must precede them).
    Pre,
    /// In-cycle iteration. Holds the existing six-tier priority
    /// lattice; cross-axis interleaving happens via the inner
    /// `MidTier` ordering.
    Mid(MidTier),
    /// Post-cycle gate — agent postcondition / sign-off work.
    /// Strictly the least-urgent phase. Wins only when no Mid-phase
    /// axis emits anything (the empty-set arm collapses through
    /// Mid into Post if Post emits).
    Post,
}

/// In-cycle priority lattice. Used only inside [`Urgency::Mid`];
/// `Pre` and `Post` are singletons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum MidTier {
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
    /// Non-blocking metadata cleanup. Sorts last within `Mid`;
    /// `Urgency::Post` still wins overall ordering below `Mid`.
    Hygiene,
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
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("test:blocker"),
        }
    }

    #[test]
    fn urgency_sorts_pre_first_post_last() {
        let mut us = [
            Urgency::Post,
            Urgency::Mid(MidTier::Hygiene),
            Urgency::Mid(MidTier::Critical),
            Urgency::Mid(MidTier::BlockingHuman),
            Urgency::Mid(MidTier::BlockingFix),
            Urgency::Mid(MidTier::Advancing),
            Urgency::Mid(MidTier::BlockingWait),
            Urgency::Pre,
        ];
        us.sort();
        assert_eq!(
            us,
            [
                Urgency::Pre,
                Urgency::Mid(MidTier::Critical),
                Urgency::Mid(MidTier::BlockingFix),
                Urgency::Mid(MidTier::BlockingWait),
                Urgency::Mid(MidTier::BlockingHuman),
                Urgency::Mid(MidTier::Advancing),
                Urgency::Mid(MidTier::Hygiene),
                Urgency::Post,
            ]
        );
    }

    #[test]
    fn pre_is_strictly_most_urgent() {
        assert!(Urgency::Pre < Urgency::Mid(MidTier::Critical));
        assert!(Urgency::Pre < Urgency::Mid(MidTier::BlockingFix));
        assert!(Urgency::Pre < Urgency::Mid(MidTier::BlockingWait));
        assert!(Urgency::Pre < Urgency::Mid(MidTier::BlockingHuman));
        assert!(Urgency::Pre < Urgency::Mid(MidTier::Advancing));
        assert!(Urgency::Pre < Urgency::Mid(MidTier::Hygiene));
        assert!(Urgency::Pre < Urgency::Post);
    }

    #[test]
    fn post_is_strictly_least_urgent() {
        assert!(Urgency::Mid(MidTier::Hygiene) < Urgency::Post);
        assert!(Urgency::Mid(MidTier::Advancing) < Urgency::Post);
        assert!(Urgency::Mid(MidTier::BlockingHuman) < Urgency::Post);
        assert!(Urgency::Mid(MidTier::BlockingWait) < Urgency::Post);
        assert!(Urgency::Mid(MidTier::BlockingFix) < Urgency::Post);
        assert!(Urgency::Mid(MidTier::Critical) < Urgency::Post);
        assert!(Urgency::Pre < Urgency::Post);
    }

    #[test]
    fn mid_tier_internal_order_critical_first() {
        let mut tiers = [
            MidTier::Hygiene,
            MidTier::Critical,
            MidTier::BlockingHuman,
            MidTier::BlockingFix,
            MidTier::Advancing,
            MidTier::BlockingWait,
        ];
        tiers.sort();
        assert_eq!(
            tiers,
            [
                MidTier::Critical,
                MidTier::BlockingFix,
                MidTier::BlockingWait,
                MidTier::BlockingHuman,
                MidTier::Advancing,
                MidTier::Hygiene,
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
