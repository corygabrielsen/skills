//! Wire-token vocabulary shared across per-binary recorders.
//!
//! # Why this lives in `ooda-state`
//!
//! `events.jsonl` is the wire. Every recorder writes the same on-disk
//! format; consumers (cockpit, SSE readers, audit tooling) parse one
//! schema regardless of which binary produced the run. The Outcome
//! token written on `RunHalted`, the `kind_suffix` on
//! `DomainSpecific`, the `decision_kind` on `IterationDecided`, and
//! the `variant` on `IterationHandoff` are wire symbols — leaving
//! their literals in each recorder lets them drift independently,
//! which is exactly what happened.
//!
//! The recorder-side artifacts live here so the wire vocabulary
//! travels with the on-disk schema, not the per-binary glue.
//!
//! # Domain neutrality stays domain neutrality
//!
//! `ooda-state` knows about two domains because the wire token
//! `"DoneSucceeded"` means different things in each — PR-side calls
//! it `"DoneMerged"`, codex-review calls it `"DoneFixedPoint"`. The
//! mapping is per-domain by construction; the two are the only
//! production domains. Adding a third domain requires adding a
//! `Domain` impl here (one match arm per [`OutcomeKind`] variant) —
//! by design, not by accident.

use crate::{BlobRef, EventBody};

// ── Outcome discriminator ────────────────────────────────────────────

/// Payload-stripped projection of a binary's `Outcome` variant. The
/// 10 variants exhaustively cover `ooda_core::Outcome<K>`; this enum
/// is the cross-crate handshake so `ooda-state` can name the wire
/// token without depending on `ooda-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutcomeKind {
    DoneSucceeded,
    DoneAborted,
    Paused,
    WouldAdvance,
    HandoffHuman,
    HandoffAgent,
    StuckRepeated,
    StuckCapReached,
    UsageError,
    BinaryError,
}

impl OutcomeKind {
    /// Stable single-token identifier for the variant itself —
    /// matches the Rust source name. Used by the
    /// [`EventBody::IterationHandoff`] `variant` field so consumers
    /// can pivot on the same string the stderr header carries.
    #[must_use]
    pub fn variant_name(self) -> &'static str {
        match self {
            Self::DoneSucceeded => "DoneSucceeded",
            Self::DoneAborted => "DoneAborted",
            Self::Paused => "Paused",
            Self::WouldAdvance => "WouldAdvance",
            Self::HandoffHuman => "HandoffHuman",
            Self::HandoffAgent => "HandoffAgent",
            Self::StuckRepeated => "StuckRepeated",
            Self::StuckCapReached => "StuckCapReached",
            Self::UsageError => "UsageError",
            Self::BinaryError => "BinaryError",
        }
    }
}

// ── Domain overlay ───────────────────────────────────────────────────

/// Per-domain wire-token table. One method per outcome variant; the
/// trait shape forces every domain to map every variant — adding a
/// new `OutcomeKind` is a compile error in every domain until handled.
pub trait Domain {
    /// Stable identifier for the domain itself; surfaced as the
    /// `domain` field on [`EventBody::RunStarted`].
    fn name(&self) -> &'static str;

    /// Domain-specific wire token for an outcome variant. Written
    /// onto [`EventBody::RunHalted::outcome`] and (with the leading
    /// header) onto the stderr boundary line.
    fn outcome_token(&self, kind: OutcomeKind) -> &'static str;
}

/// PR-side recorder domain: ooda-pr / ooda-prs / ooda-pr-codex-review.
/// Success ⇔ "the PR merged"; abort ⇔ "the PR closed without merge".
#[derive(Debug, Clone, Copy, Default)]
pub struct PrDomain;

impl Domain for PrDomain {
    fn name(&self) -> &'static str {
        "pr"
    }

    fn outcome_token(&self, kind: OutcomeKind) -> &'static str {
        match kind {
            OutcomeKind::DoneSucceeded => "DoneMerged",
            OutcomeKind::DoneAborted => "DoneClosed",
            OutcomeKind::Paused => "Paused",
            OutcomeKind::WouldAdvance => "WouldAdvance",
            OutcomeKind::HandoffHuman => "HandoffHuman",
            OutcomeKind::HandoffAgent => "HandoffAgent",
            OutcomeKind::StuckRepeated => "StuckRepeated",
            OutcomeKind::StuckCapReached => "StuckCapReached",
            OutcomeKind::UsageError => "UsageError",
            OutcomeKind::BinaryError => "BinaryError",
        }
    }
}

/// codex-review recorder domain: ooda-codex-review. Success ⇔
/// "ladder reached fixed point at ceiling"; the `Paused` variant
/// surfaces as `"Idle"` to match the stderr header the orchestrator
/// dispatches on.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexReviewDomain;

impl Domain for CodexReviewDomain {
    fn name(&self) -> &'static str {
        "codex-review"
    }

    fn outcome_token(&self, kind: OutcomeKind) -> &'static str {
        match kind {
            OutcomeKind::DoneSucceeded => "DoneFixedPoint",
            OutcomeKind::DoneAborted => "DoneAborted",
            OutcomeKind::Paused => "Idle",
            OutcomeKind::WouldAdvance => "WouldAdvance",
            OutcomeKind::HandoffHuman => "HandoffHuman",
            OutcomeKind::HandoffAgent => "HandoffAgent",
            OutcomeKind::StuckRepeated => "StuckRepeated",
            OutcomeKind::StuckCapReached => "StuckCapReached",
            OutcomeKind::UsageError => "UsageError",
            OutcomeKind::BinaryError => "BinaryError",
        }
    }
}

// ── kind_suffix vocabulary ──────────────────────────────────────────

/// Closed set of `kind_suffix` literals every PR-side recorder
/// emits. Lifting this to an enum makes a typo a compile error and
/// gives the mirror-check script one place to grep for coverage.
///
/// Per-binary extras (e.g. ooda-pr-codex-review's
/// `codex_review_config`) intentionally live outside this enum: the
/// recorder emits them via [`EventBody::DomainSpecific`] with a raw
/// string suffix. That stays per-binary by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainKind {
    ObserveStarted,
    ObserveFinished,
    StatusCommentRendered,
    StatusCommentResult,
    ActionStarted,
    ActionFinished,
    WaitStarted,
    WaitFinished,
    Outcome,
    ToolCallStarted,
    ToolCallFinished,
    TraceLine,
    IterationCandidates,
    IterationDashboard,
    IterationDecisionEnvelope,
}

impl DomainKind {
    /// Wire-stable `kind_suffix` literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObserveStarted => "observe_started",
            Self::ObserveFinished => "observe_finished",
            Self::StatusCommentRendered => "status_comment_rendered",
            Self::StatusCommentResult => "status_comment_result",
            Self::ActionStarted => "action_started",
            Self::ActionFinished => "action_finished",
            Self::WaitStarted => "wait_started",
            Self::WaitFinished => "wait_finished",
            Self::Outcome => "outcome",
            Self::ToolCallStarted => "tool_call_started",
            Self::ToolCallFinished => "tool_call_finished",
            Self::TraceLine => "trace_line",
            Self::IterationCandidates => "iteration_candidates",
            Self::IterationDashboard => "iteration_dashboard",
            Self::IterationDecisionEnvelope => "iteration_decision_envelope",
        }
    }
}

// ── Terminal event helper ────────────────────────────────────────────

/// Pick the terminal event variant for an outcome. Stall-class and
/// cap-class outcomes get the typed [`EventBody::RunStalled`] /
/// [`EventBody::RunCapReached`] events with the repeating action's
/// kind name; every other variant collapses to
/// [`EventBody::RunHalted`] carrying the domain's wire token + exit
/// code.
///
/// All four recorder binaries use this helper so the typed
/// stall/cap events are emitted uniformly — the wire reader can
/// match on `RunStalled` / `RunCapReached` without inspecting the
/// `Outcome` token to disambiguate.
#[must_use]
pub fn terminal_event(
    domain: &impl Domain,
    kind: OutcomeKind,
    exit_code: i32,
    last_action_kind: Option<&str>,
) -> EventBody {
    match kind {
        OutcomeKind::StuckRepeated => EventBody::RunStalled {
            last_action: last_action_kind.unwrap_or("").to_string(),
        },
        OutcomeKind::StuckCapReached => EventBody::RunCapReached {
            last_action: last_action_kind.unwrap_or("").to_string(),
        },
        _ => EventBody::RunHalted {
            outcome: domain.outcome_token(kind).to_string(),
            exit_code,
        },
    }
}

// ── DomainSpecific constructor sugar ─────────────────────────────────

/// Construct an [`EventBody::DomainSpecific`] event from a
/// [`DomainKind`] discriminant + payload. Forces the `kind_suffix` to
/// come from the enum vocabulary — typos are compile errors, the
/// mirror-check script can grep `DomainKind::` to verify coverage
/// across the 3 PR-side recorders.
#[must_use]
pub fn domain_specific(kind: DomainKind, payload: serde_json::Value) -> EventBody {
    EventBody::DomainSpecific {
        kind_suffix: kind.as_str().to_string(),
        payload,
    }
}

// ── BlobRef path helper ──────────────────────────────────────────────

/// Compute the on-disk path of a blob given the state root and run
/// id. Wraps the layout the writer uses (`runs/<id>/blobs/<sha>.<ext>`)
/// so callers that need a path back from a `BlobRef` (e.g. handoff
/// pointers) don't re-derive the structure.
#[must_use]
pub fn blob_path(state_root: &std::path::Path, run_id: &str, blob: &BlobRef) -> std::path::PathBuf {
    state_root
        .join("runs")
        .join(run_id)
        .join("blobs")
        .join(format!("{}.{}", blob.sha, blob.ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_domain_renames_done_succeeded_to_done_merged() {
        let d = PrDomain;
        assert_eq!(d.outcome_token(OutcomeKind::DoneSucceeded), "DoneMerged");
        assert_eq!(d.outcome_token(OutcomeKind::DoneAborted), "DoneClosed");
        assert_eq!(d.name(), "pr");
    }

    #[test]
    fn codex_review_domain_renames_done_succeeded_to_done_fixed_point() {
        let d = CodexReviewDomain;
        assert_eq!(
            d.outcome_token(OutcomeKind::DoneSucceeded),
            "DoneFixedPoint"
        );
        assert_eq!(d.outcome_token(OutcomeKind::Paused), "Idle");
        assert_eq!(d.name(), "codex-review");
    }

    #[test]
    fn terminal_event_for_stuck_repeated_emits_run_stalled() {
        let evt = terminal_event(&PrDomain, OutcomeKind::StuckRepeated, 6, Some("Rebase"));
        match evt {
            EventBody::RunStalled { last_action } => assert_eq!(last_action, "Rebase"),
            other => panic!("expected RunStalled, got {other:?}"),
        }
    }

    #[test]
    fn terminal_event_for_stuck_cap_reached_emits_run_cap_reached() {
        let evt = terminal_event(&PrDomain, OutcomeKind::StuckCapReached, 7, Some("Wait"));
        match evt {
            EventBody::RunCapReached { last_action } => assert_eq!(last_action, "Wait"),
            other => panic!("expected RunCapReached, got {other:?}"),
        }
    }

    #[test]
    fn terminal_event_for_done_succeeded_emits_run_halted_with_domain_token() {
        let evt = terminal_event(&PrDomain, OutcomeKind::DoneSucceeded, 0, None);
        match evt {
            EventBody::RunHalted { outcome, exit_code } => {
                assert_eq!(outcome, "DoneMerged");
                assert_eq!(exit_code, 0);
            }
            other => panic!("expected RunHalted, got {other:?}"),
        }
        let evt = terminal_event(&CodexReviewDomain, OutcomeKind::DoneSucceeded, 0, None);
        match evt {
            EventBody::RunHalted { outcome, .. } => assert_eq!(outcome, "DoneFixedPoint"),
            other => panic!("expected RunHalted, got {other:?}"),
        }
    }

    #[test]
    fn domain_kind_round_trips_through_as_str() {
        assert_eq!(DomainKind::Outcome.as_str(), "outcome");
        assert_eq!(DomainKind::ToolCallFinished.as_str(), "tool_call_finished");
        assert_eq!(
            DomainKind::IterationDecisionEnvelope.as_str(),
            "iteration_decision_envelope"
        );
    }

    #[test]
    fn domain_specific_constructor_sets_kind_suffix_from_enum() {
        let evt = domain_specific(DomainKind::ActionStarted, serde_json::json!({"a": 1}));
        match evt {
            EventBody::DomainSpecific {
                kind_suffix,
                payload,
            } => {
                assert_eq!(kind_suffix, "action_started");
                assert_eq!(payload, serde_json::json!({"a": 1}));
            }
            other => panic!("expected DomainSpecific, got {other:?}"),
        }
    }
}
