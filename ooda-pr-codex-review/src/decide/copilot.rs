//! Bot-review axis candidates.
//!
//! Wait while the review is in flight; advance to a higher quality
//! tier when a review has landed but progress is still available;
//! escalate when the per-HEAD health budget is exhausted.

use crate::ids::BlockerKey;

use crate::orient::copilot::{
    CopilotActivity, CopilotReport, CopilotTier, InFlightHealth, Symptom,
};

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

// Flat decision table over the activity variants of this axis. The
// table's length is its specification: each arm carries action,
// blocker, and rationale inline. Factoring into helpers would split
// the per-arm mapping across files and weaken the audit surface.
//
// The Healthy / Degraded / Failed branching shape recurs on other
// bot axes; a future shared abstraction can lift it once three or
// more axes wear it.
#[allow(clippy::too_many_lines)]
pub(super) fn candidates(report: &CopilotReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    match &report.activity {
        CopilotActivity::Idle => {
            // Absence of signal is not a gate.
        }
        CopilotActivity::Requested {
            health: InFlightHealth::Healthy,
            ..
        } => {
            out.push(Action {
                kind: ActionKind::WaitForCopilotAck,
                effect: ActionEffect::Wait {
                    interval: ooda_core::PollingInterval::from_secs(15),
                    log: "Waiting for Copilot to start reviewing".into(),
                },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::Mid(MidTier::BlockingWait),
                blocker: BlockerKey::from_static("copilot_not_acked"),
            });
        }
        CopilotActivity::Working {
            health: InFlightHealth::Healthy,
            ..
        } => {
            out.push(Action {
                kind: ActionKind::WaitForCopilotReview,
                effect: ActionEffect::Wait {
                    interval: ooda_core::PollingInterval::from_secs(60),
                    log: "Waiting for Copilot to finish reviewing".into(),
                },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::Mid(MidTier::BlockingWait),
                blocker: BlockerKey::from_static("copilot_reviewing"),
            });
        }
        CopilotActivity::Requested {
            health: InFlightHealth::Degraded,
            ..
        } => {
            out.push(degraded_rerequest(Symptom::StartTimeout));
        }
        CopilotActivity::Working {
            health: InFlightHealth::Degraded,
            ..
        } => {
            out.push(degraded_rerequest(Symptom::ReviewTimeout));
        }
        CopilotActivity::Requested {
            requested_at,
            health: InFlightHealth::Failed,
        } => {
            out.push(failed_escalation(
                Symptom::StartTimeout,
                FailedTiming {
                    requested_at: *requested_at,
                    ack_at: None,
                },
                report,
            ));
        }
        CopilotActivity::Working {
            requested_at,
            ack_at,
            health: InFlightHealth::Failed,
        } => {
            out.push(failed_escalation(
                Symptom::ReviewTimeout,
                FailedTiming {
                    requested_at: *requested_at,
                    ack_at: Some(*ack_at),
                },
                report,
            ));
        }
        CopilotActivity::Reviewed { latest } => {
            if report.tier == CopilotTier::Platinum {
                return out;
            }
            // Per-thread feedback is owned by the reviews axis.
            if report.threads.unresolved > 0 {
                return out;
            }
            let stale = report.threads.stale;
            let not_at_head = !report.fresh;
            let suppressed = latest.comments_suppressed;

            // A fresh re-request dominates the staleness and
            // not-at-HEAD conditions: one pass clears both and may
            // resolve suppressed findings as a side effect.
            if stale > 0 || not_at_head {
                let desc = if stale > 0 {
                    format!(
                        "Re-request Copilot review so it sees {}",
                        crate::text::count(stale as usize, "new reply"),
                    )
                } else {
                    "Re-request Copilot review on HEAD to reach platinum".into()
                };
                out.push(Action {
                    kind: ActionKind::RerequestCopilot { symptom: None },
                    effect: ActionEffect::Full { log: desc },
                    target_effect: TargetEffect::Advances,
                    urgency: Urgency::Mid(MidTier::Critical),
                    blocker: BlockerKey::typed("copilot_tier", &report.tier),
                });
            } else if report.tier == CopilotTier::Silver && suppressed > 0 {
                out.push(Action {
                    kind: ActionKind::AddressCopilotSuppressed { count: suppressed },
                    effect: ActionEffect::Agent {
                        prompt: ooda_core::HandoffPrompt::new(format!(
                            "Copilot flagged {}. Investigate and push fixes for any \
                         that are real — the next review may clear them.",
                            crate::text::count(suppressed as usize, "low-confidence finding"),
                        )),
                    },
                    target_effect: TargetEffect::Advances,
                    urgency: Urgency::Mid(MidTier::Advancing),
                    blocker: BlockerKey::from_static("copilot_tier_silver"),
                });
            }
        }
    }

    out
}

/// Health-driven re-request. The blocker key carries the symptom so
/// the stall comparator distinguishes timeout classes and the
/// per-iteration trace records which class fired.
fn degraded_rerequest(symptom: Symptom) -> Action {
    let (tag, log) = match symptom {
        Symptom::StartTimeout => (
            "copilot_degraded_start_timeout",
            "Re-requesting Copilot — never started within the start timeout",
        ),
        Symptom::ReviewTimeout => (
            "copilot_degraded_review_timeout",
            "Re-requesting Copilot — started but no review within the review timeout",
        ),
    };
    Action {
        kind: ActionKind::RerequestCopilot {
            symptom: Some(symptom),
        },
        effect: ActionEffect::Full { log: log.into() },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingFix),
        blocker: BlockerKey::for_test(tag),
    }
}

/// Per-HEAD health budget exhausted. Human handoff with no driver
/// side effect — the runner translates this directly to its terminal
/// handoff outcome.
///
/// `timing` and `report` are the prompt's enrichment surface:
/// per-round timestamps replace generic "investigate" instructions
/// with the concrete request and acknowledgement times, and the
/// attempt count and tier label situate the failure in the axis's
/// progress lattice.
fn failed_escalation(symptom: Symptom, timing: FailedTiming, report: &CopilotReport) -> Action {
    let (tag, headline) = match symptom {
        Symptom::StartTimeout => (
            "copilot_failed_start_timeout",
            "Copilot has not started reviewing after repeated requests at this HEAD.",
        ),
        Symptom::ReviewTimeout => (
            "copilot_failed_review_timeout",
            "Copilot started but failed to submit a review after repeated requests \
             at this HEAD.",
        ),
    };
    let mut prompt = ooda_core::HandoffPrompt::new(headline);

    prompt.push_paragraph(
        "Step 1 — check the GitHub Copilot service status \
         (https://www.githubstatus.com) to confirm the stall is upstream rather \
         than per-PR."
            .to_string(),
    );

    prompt.push_paragraph(
        "Step 2 — once the underlying issue is resolved, re-request the Copilot \
         review manually from the PR's Reviewers panel on GitHub."
            .to_string(),
    );

    prompt.push_paragraph(format!("Requested at: {}.", timing.requested_at));
    if let Some(ack_at) = timing.ack_at {
        prompt.push_paragraph(format!("Ack at: {ack_at}."));
    }
    prompt.push_paragraph(format!(
        "Attempt count at this HEAD: {} (tier: {}).",
        report.rounds.len(),
        report.tier.slug(),
    ));
    Action {
        kind: ActionKind::EscalateCopilotFailed { symptom },
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::for_test(tag),
    }
}

/// Timestamps the failed-escalation prompt enriches with.
/// `ack_at` is present iff the bot acknowledged before failing.
#[derive(Clone, Copy)]
struct FailedTiming {
    requested_at: crate::ids::Timestamp,
    ack_at: Option<crate::ids::Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, Timestamp};
    use crate::orient::bot_threads::BotThreadSummary;
    use crate::orient::copilot::{CopilotRepoConfig, CopilotReviewRound};

    fn enabled() -> CopilotRepoConfig {
        CopilotRepoConfig {
            enabled: true,
            review_on_push: false,
            review_draft_pull_requests: false,
        }
    }

    fn round_at_head() -> CopilotReviewRound {
        CopilotReviewRound {
            round: 1,
            requested_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            ack_at: Some(Timestamp::parse("2026-04-23T10:01:00Z").unwrap()),
            reviewed_at: Some(Timestamp::parse("2026-04-23T10:05:00Z").unwrap()),
            commit: Some(GitCommitSha::parse(&"a".repeat(40)).unwrap()),
            comments_visible: 0,
            comments_suppressed: 0,
        }
    }

    fn report(
        activity: CopilotActivity,
        tier: CopilotTier,
        threads: BotThreadSummary,
        fresh: bool,
    ) -> CopilotReport {
        CopilotReport {
            config: enabled(),
            activity,
            rounds: vec![],
            threads,
            tier,
            fresh,
        }
    }

    #[test]
    fn idle_yields_no_candidates() {
        let r = report(
            CopilotActivity::Idle,
            CopilotTier::Bronze,
            BotThreadSummary::default(),
            false,
        );
        assert!(candidates(&r).is_empty());
    }

    #[test]
    fn requested_healthy_emits_wait_for_ack() {
        let r = report(
            CopilotActivity::Requested {
                requested_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                health: InFlightHealth::Healthy,
            },
            CopilotTier::Bronze,
            BotThreadSummary::default(),
            false,
        );
        let cs = candidates(&r);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCopilotAck));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
    }

    #[test]
    fn platinum_at_head_yields_no_candidates() {
        let r = report(
            CopilotActivity::Reviewed {
                latest: round_at_head(),
            },
            CopilotTier::Platinum,
            BotThreadSummary::default(),
            true,
        );
        assert!(candidates(&r).is_empty());
    }

    #[test]
    fn gold_not_fresh_emits_rerequest() {
        let r = report(
            CopilotActivity::Reviewed {
                latest: round_at_head(),
            },
            CopilotTier::Gold,
            BotThreadSummary::default(),
            false, // not at HEAD
        );
        let cs = candidates(&r);
        assert!(matches!(
            cs[0].kind,
            ActionKind::RerequestCopilot { symptom: None }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Full { .. }));
    }

    #[test]
    fn silver_with_suppressed_emits_address_when_no_stale() {
        let mut latest = round_at_head();
        latest.comments_suppressed = 2;
        let r = report(
            CopilotActivity::Reviewed { latest },
            CopilotTier::Silver,
            BotThreadSummary::default(),
            true,
        );
        let cs = candidates(&r);
        assert!(matches!(
            cs[0].kind,
            ActionKind::AddressCopilotSuppressed { count: 2 }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn unresolved_threads_block_tier_advancement_at_copilot_layer() {
        let r = report(
            CopilotActivity::Reviewed {
                latest: round_at_head(),
            },
            CopilotTier::Bronze,
            BotThreadSummary {
                total: 1,
                resolved: 0,
                unresolved: 1,
                outdated: 0,
                stale: 0,
            },
            true,
        );
        // Reviews axis handles unresolved threads; Copilot stays silent.
        assert!(candidates(&r).is_empty());
    }

    // ─── per-variant baseline property ────────────────────────────
    //
    // Class invariant: in the baseline configuration (Bronze tier,
    // fresh at HEAD, no thread or suppression backlog) each
    // (Activity × Health) pair maps to a deterministic candidate
    // shape. Sub-conditions on the Reviewed branch are pinned by
    // the scenario tests above; the cross-product is enumerated
    // exhaustively here. A new variant in either enum fails to
    // compile at the exhaustive match below.

    #[derive(Debug, PartialEq, Eq)]
    enum CopilotBaselineBehavior {
        NoCandidate,
        EmitWaitForAck,
        EmitWaitForReview,
        EmitDegradedRerequest(Symptom),
        EmitFailedEscalation(Symptom),
    }

    /// Baseline projection: one expected behaviour per (Activity ×
    /// Health) pair. Structurally exhaustive — the match below is
    /// the contract.
    fn expected_copilot_baseline_behavior(activity: &CopilotActivity) -> CopilotBaselineBehavior {
        // Intentional exhaustive match per axis pattern; arms are
        // duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
        match activity {
            CopilotActivity::Idle => CopilotBaselineBehavior::NoCandidate,
            CopilotActivity::Requested {
                health: InFlightHealth::Healthy,
                ..
            } => CopilotBaselineBehavior::EmitWaitForAck,
            CopilotActivity::Working {
                health: InFlightHealth::Healthy,
                ..
            } => CopilotBaselineBehavior::EmitWaitForReview,
            CopilotActivity::Requested {
                health: InFlightHealth::Degraded,
                ..
            } => CopilotBaselineBehavior::EmitDegradedRerequest(Symptom::StartTimeout),
            CopilotActivity::Working {
                health: InFlightHealth::Degraded,
                ..
            } => CopilotBaselineBehavior::EmitDegradedRerequest(Symptom::ReviewTimeout),
            CopilotActivity::Requested {
                health: InFlightHealth::Failed,
                ..
            } => CopilotBaselineBehavior::EmitFailedEscalation(Symptom::StartTimeout),
            CopilotActivity::Working {
                health: InFlightHealth::Failed,
                ..
            } => CopilotBaselineBehavior::EmitFailedEscalation(Symptom::ReviewTimeout),
            // Baseline Reviewed has nothing to gate on; advancement
            // happens through either a new code push or per-thread
            // remediation owned by another axis.
            CopilotActivity::Reviewed { .. } => CopilotBaselineBehavior::NoCandidate,
        }
    }

    fn all_copilot_activities() -> Vec<CopilotActivity> {
        let req_at = Timestamp::parse("2026-04-23T10:00:00Z").unwrap();
        let ack_at = Timestamp::parse("2026-04-23T10:01:00Z").unwrap();
        let req = |health| CopilotActivity::Requested {
            requested_at: req_at,
            health,
        };
        let work = |health| CopilotActivity::Working {
            requested_at: req_at,
            ack_at,
            health,
        };
        vec![
            CopilotActivity::Idle,
            req(InFlightHealth::Healthy),
            req(InFlightHealth::Degraded),
            req(InFlightHealth::Failed),
            work(InFlightHealth::Healthy),
            work(InFlightHealth::Degraded),
            work(InFlightHealth::Failed),
            CopilotActivity::Reviewed {
                latest: round_at_head(),
            },
        ]
    }

    fn observed_copilot_baseline_behavior(cs: &[Action]) -> CopilotBaselineBehavior {
        match cs {
            [] => CopilotBaselineBehavior::NoCandidate,
            [a] => match (&a.kind, &a.effect) {
                (ActionKind::WaitForCopilotAck, ActionEffect::Wait { .. }) => {
                    CopilotBaselineBehavior::EmitWaitForAck
                }
                (ActionKind::WaitForCopilotReview, ActionEffect::Wait { .. }) => {
                    CopilotBaselineBehavior::EmitWaitForReview
                }
                (
                    ActionKind::RerequestCopilot {
                        symptom: Some(symptom),
                    },
                    ActionEffect::Full { .. },
                ) => CopilotBaselineBehavior::EmitDegradedRerequest(*symptom),
                (ActionKind::EscalateCopilotFailed { symptom }, ActionEffect::Human { .. }) => {
                    CopilotBaselineBehavior::EmitFailedEscalation(*symptom)
                }
                (kind, effect) => panic!(
                    "copilot axis emitted unexpected (kind, effect) in baseline: \
                     {kind:?}, {effect:?}",
                ),
            },
            multi => panic!(
                "copilot axis emitted unexpected candidate count in baseline: {} items",
                multi.len()
            ),
        }
    }

    #[test]
    fn copilot_axis_property_holds_for_every_activity_baseline() {
        let activities = all_copilot_activities();
        assert_eq!(
            activities.len(),
            8,
            "Sample enumeration must cover every (Activity × Health) \
             case: Idle (1) + Requested×3 + Working×3 + Reviewed (1) = 8. \
             A new variant in either enum requires updating the sample \
             list, the exhaustive match, and this length sentinel.",
        );
        for activity in activities {
            let r = report(
                activity.clone(),
                CopilotTier::Bronze,
                BotThreadSummary::default(),
                true,
            );
            let cs = candidates(&r);
            let actual = observed_copilot_baseline_behavior(&cs);
            let expected = expected_copilot_baseline_behavior(&activity);
            assert_eq!(
                actual, expected,
                "copilot baseline contract violated for activity = {activity:?}",
            );
        }
    }

    // ── prompt-enrichment tests ─────────────────────────────────────

    fn report_with_rounds(
        activity: CopilotActivity,
        tier: CopilotTier,
        rounds: Vec<CopilotReviewRound>,
    ) -> CopilotReport {
        CopilotReport {
            config: enabled(),
            activity,
            rounds,
            threads: BotThreadSummary::default(),
            tier,
            fresh: false,
        }
    }

    #[test]
    fn escalate_copilot_failed_start_timeout_surfaces_requested_at_and_tier() {
        let req_at = Timestamp::parse("2026-05-15T09:00:00Z").unwrap();
        let r = report_with_rounds(
            CopilotActivity::Requested {
                requested_at: req_at,
                health: InFlightHealth::Failed,
            },
            CopilotTier::Bronze,
            vec![round_at_head(), round_at_head()],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Copilot has not started reviewing"));
        assert!(
            rendered.contains("Requested at: 2026-05-15T09:00:00+00:00"),
            "missing requested_at: {rendered}",
        );
        // StartTimeout has no Ack — must not render an Ack paragraph.
        assert!(!rendered.contains("Ack at:"));
        assert!(
            rendered.contains("Attempt count at this HEAD: 2 (tier: bronze)"),
            "missing attempt count + tier: {rendered}",
        );
    }

    #[test]
    fn escalate_copilot_failed_review_timeout_surfaces_ack_at() {
        let req_at = Timestamp::parse("2026-05-15T09:00:00Z").unwrap();
        let ack_at = Timestamp::parse("2026-05-15T09:02:00Z").unwrap();
        let r = report_with_rounds(
            CopilotActivity::Working {
                requested_at: req_at,
                ack_at,
                health: InFlightHealth::Failed,
            },
            CopilotTier::Silver,
            vec![round_at_head()],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Copilot started but failed to submit a review"));
        assert!(rendered.contains("Requested at: 2026-05-15T09:00:00+00:00"));
        assert!(
            rendered.contains("Ack at: 2026-05-15T09:02:00+00:00"),
            "missing ack_at: {rendered}",
        );
        assert!(rendered.contains("Attempt count at this HEAD: 1 (tier: silver)"));
    }

    #[test]
    fn escalate_copilot_failed_prompt_uses_step_form_for_actions() {
        let req_at = Timestamp::parse("2026-05-15T09:00:00Z").unwrap();
        let r = report_with_rounds(
            CopilotActivity::Requested {
                requested_at: req_at,
                health: InFlightHealth::Failed,
            },
            CopilotTier::Bronze,
            vec![round_at_head()],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Step 1"), "missing Step 1: {rendered}");
        assert!(rendered.contains("Step 2"), "missing Step 2: {rendered}");
        assert!(
            rendered.contains("githubstatus.com"),
            "step 1 must surface the GitHub status URL: {rendered}",
        );
        assert!(
            rendered.contains("re-request the Copilot review"),
            "step 2 must direct the user to re-request the review: {rendered}",
        );
        let step1 = rendered.find("Step 1").expect("step 1 present");
        let step2 = rendered.find("Step 2").expect("step 2 present");
        assert!(step1 < step2, "Step 1 must precede Step 2");
    }
}
