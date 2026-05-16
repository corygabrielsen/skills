//! Cursor candidates.
//!
//! Six logical arms over the (`CursorActivity` × `InFlightHealth`) cross
//! product:
//!
//!   `NotApplicable`          → no candidate (delegate)
//!   Skipped(_)             → no candidate (delegate; reason in JSONL)
//!   InFlight(Healthy)      → `WaitForCursorReview` (60s)
//!   InFlight(Failed)       → `EscalateCursorStalled` (Human handoff)
//!   Reviewed(Clean)        → no candidate (delegate)
//!   Reviewed(HasFindings)  → no candidate (the generic `AddressThreads`
//!                            on the reviews axis owns thread
//!                            remediation; Cursor stays silent here)
//!
//! Cursor has no rerequest API and no Degraded intermediate — see
//! `orient/cursor.rs` for the rationale.

use crate::ids::BlockerKey;

use crate::orient::cursor::{CursorActivity, CursorReport, InFlightHealth, ReviewedState};

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

pub fn candidates(report: &CursorReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    // Intentional exhaustive match per axis pattern; arms are kept
    // distinct for spec clarity even when several emit no candidate.
    #[allow(clippy::match_same_arms)]
    match &report.activity {
        CursorActivity::NotApplicable => {
            // Cursor not active in this repo. Nothing to do.
        }
        CursorActivity::Skipped(_) => {
            // Cursor declined this PR (bot-class author, repo opt-out,
            // or unknown). The variant is preserved on the report for
            // JSONL but the decide layer has no remediation.
        }
        CursorActivity::InFlight(InFlightHealth::Healthy) => {
            out.push(Action {
                kind: ActionKind::WaitForCursorReview,
                effect: ActionEffect::Wait {
                    interval: ooda_core::PollingInterval::from_secs(60),
                    log: "Waiting for Cursor Bugbot to finish reviewing".into(),
                },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                blocker: BlockerKey::tag("cursor_reviewing"),
            });
        }
        CursorActivity::InFlight(InFlightHealth::Failed) => {
            out.push(failed_escalation(report));
        }
        CursorActivity::Reviewed(ReviewedState::Clean) => {
            // Cursor reviewed and reports no findings.
        }
        CursorActivity::Reviewed(ReviewedState::HasFindings) => {
            // Findings exist as review threads — the reviews axis's
            // generic AddressThreads candidate owns remediation; the
            // Cursor axis stays silent to avoid double-emission.
        }
    }
    out
}

/// Stall escalation. No symptom payload (Cursor has a single failure
/// mode) and no act-layer side effect — the runner consumes
/// `Outcome::HandoffHuman` and exits.
///
/// Surfaces the suite's `created_at` (and elapsed minutes since)
/// when known, plus the per-PR Cursor round count. Both come from the
/// already-projected `CursorReport` — no new observation needed.
fn failed_escalation(report: &CursorReport) -> Action {
    let stall_min = crate::orient::cursor::STALL_TIMEOUT.num_minutes();
    let headline = format!(
        "Cursor Bugbot has not produced a review within {stall_min} minutes of the suite \
         opening at this HEAD."
    );
    let mut prompt = ooda_core::HandoffPrompt::new(headline);

    prompt.push_paragraph(
        "Cursor's check_suite appears stalled on Cursor's backend. There is no \
         rerequest API that unsticks it — Cursor auto-runs on every push, so a \
         new commit is the only way to retry."
            .to_string(),
    );

    prompt.push_paragraph(
        "Step 1 — check Cursor's service status at https://cursor.com/status to \
         confirm the stall is upstream rather than per-PR."
            .to_string(),
    );

    prompt.push_paragraph(
        "Step 2 — once the underlying issue is resolved, push a new commit (any \
         no-op commit suffices); Cursor will auto-run against the new HEAD."
            .to_string(),
    );

    if let Some(created_at) = report.suite_created_at {
        prompt.push_paragraph(format!("Suite opened at: {created_at}."));
    }
    prompt.push_paragraph(format!(
        "Prior Cursor review rounds on this PR: {} (tier: {}).",
        report.rounds.len(),
        report.tier.slug(),
    ));

    Action {
        kind: ActionKind::EscalateCursorStalled,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingHuman,
        blocker: BlockerKey::tag("cursor_failed_stall"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orient::bot_threads::BotThreadSummary;
    use crate::orient::cursor::{
        CursorActivity, CursorReport, CursorSeverityBreakdown, CursorTier, InFlightHealth,
        ReviewedState, SkipReason,
    };

    fn report(activity: CursorActivity, tier: CursorTier) -> CursorReport {
        CursorReport {
            activity,
            rounds: vec![],
            threads: BotThreadSummary::default(),
            severity: CursorSeverityBreakdown::default(),
            tier,
            fresh: false,
            suite_created_at: None,
        }
    }

    #[test]
    fn not_applicable_yields_no_candidate() {
        assert!(candidates(&report(CursorActivity::NotApplicable, CursorTier::Bronze)).is_empty());
    }

    #[test]
    fn skipped_yields_no_candidate() {
        assert!(
            candidates(&report(
                CursorActivity::Skipped(SkipReason::AuthorClass),
                CursorTier::Bronze,
            ))
            .is_empty()
        );
    }

    #[test]
    fn in_flight_healthy_emits_wait() {
        let cs = candidates(&report(
            CursorActivity::InFlight(InFlightHealth::Healthy),
            CursorTier::Silver,
        ));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCursorReview));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
    }

    #[test]
    fn in_flight_failed_emits_escalation() {
        let cs = candidates(&report(
            CursorActivity::InFlight(InFlightHealth::Failed),
            CursorTier::Bronze,
        ));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::EscalateCursorStalled));
        assert!(matches!(cs[0].effect, ActionEffect::Human { .. }));
        assert_eq!(cs[0].urgency, Urgency::BlockingHuman);
        assert_eq!(cs[0].blocker.as_str(), "cursor_failed_stall");
    }

    #[test]
    fn reviewed_clean_yields_no_candidate() {
        assert!(
            candidates(&report(
                CursorActivity::Reviewed(ReviewedState::Clean),
                CursorTier::Platinum,
            ))
            .is_empty()
        );
    }

    #[test]
    fn reviewed_has_findings_yields_no_candidate_delegates_to_address_threads() {
        // Thread remediation belongs to the reviews axis (the generic
        // AddressThreads candidate). Cursor stays silent to avoid
        // double-emission.
        assert!(
            candidates(&report(
                CursorActivity::Reviewed(ReviewedState::HasFindings),
                CursorTier::Bronze,
            ))
            .is_empty()
        );
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // Exhaustive coverage over the (CursorActivity × InFlightHealth ×
    // ReviewedState × SkipReason) cross product. The match in
    // `expected_cursor_axis_behavior` is structurally exhaustive —
    // adding a variant to any of the four enums fails to compile here
    // until a new arm is added AND a sample is registered below.

    #[derive(Debug, PartialEq, Eq)]
    enum CursorAxisBehavior {
        NoCandidate,
        EmitWaitForReview,
        EmitEscalateStalled,
    }

    fn expected_cursor_axis_behavior(activity: &CursorActivity) -> CursorAxisBehavior {
        // Intentional exhaustive match per axis pattern; arms are
        // duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
        match activity {
            CursorActivity::NotApplicable => CursorAxisBehavior::NoCandidate,
            CursorActivity::Skipped(_) => CursorAxisBehavior::NoCandidate,
            CursorActivity::InFlight(InFlightHealth::Healthy) => {
                CursorAxisBehavior::EmitWaitForReview
            }
            CursorActivity::InFlight(InFlightHealth::Failed) => {
                CursorAxisBehavior::EmitEscalateStalled
            }
            CursorActivity::Reviewed(ReviewedState::Clean) => CursorAxisBehavior::NoCandidate,
            CursorActivity::Reviewed(ReviewedState::HasFindings) => CursorAxisBehavior::NoCandidate,
        }
    }

    fn all_cursor_activities() -> Vec<CursorActivity> {
        vec![
            CursorActivity::NotApplicable,
            CursorActivity::Skipped(SkipReason::AuthorClass),
            CursorActivity::Skipped(SkipReason::RepoConfig),
            CursorActivity::Skipped(SkipReason::Unknown),
            CursorActivity::InFlight(InFlightHealth::Healthy),
            CursorActivity::InFlight(InFlightHealth::Failed),
            CursorActivity::Reviewed(ReviewedState::Clean),
            CursorActivity::Reviewed(ReviewedState::HasFindings),
        ]
    }

    fn observed_cursor_axis_behavior(cs: &[Action]) -> CursorAxisBehavior {
        match cs {
            [] => CursorAxisBehavior::NoCandidate,
            [a] => match (&a.kind, &a.effect) {
                (ActionKind::WaitForCursorReview, ActionEffect::Wait { .. }) => {
                    CursorAxisBehavior::EmitWaitForReview
                }
                (ActionKind::EscalateCursorStalled, ActionEffect::Human { .. }) => {
                    CursorAxisBehavior::EmitEscalateStalled
                }
                (kind, effect) => {
                    panic!("cursor axis emitted unexpected (kind, effect): {kind:?}, {effect:?}")
                }
            },
            multi => panic!(
                "cursor axis emitted unexpected candidate count: {} items: {multi:?}",
                multi.len()
            ),
        }
    }

    #[test]
    fn cursor_axis_property_holds_for_every_activity() {
        let activities = all_cursor_activities();
        // NotApplicable (1) + Skipped×|SkipReason| (3) +
        // InFlight×|InFlightHealth| (2) + Reviewed×|ReviewedState| (2)
        // = 8. Adding a variant to any of those four enums requires
        // updating this length sentinel AND the exhaustive match in
        // `expected_cursor_axis_behavior` AND the sample list above.
        assert_eq!(
            activities.len(),
            8,
            "`all_cursor_activities` must enumerate every \
             (CursorActivity × InFlightHealth × ReviewedState × SkipReason) \
             case: NotApplicable (1) + Skipped×3 + InFlight×2 + Reviewed×2 = 8.",
        );
        for activity in activities {
            let r = report(activity.clone(), CursorTier::Bronze);
            let cs = candidates(&r);
            let actual = observed_cursor_axis_behavior(&cs);
            let expected = expected_cursor_axis_behavior(&activity);
            assert_eq!(
                actual, expected,
                "cursor-axis contract violated for activity = {activity:?}",
            );
        }
    }

    // ── prompt-enrichment tests ─────────────────────────────────────

    use crate::ids::{GitCommitSha, Timestamp};
    use crate::orient::cursor::CursorReviewRound;

    fn report_with(
        activity: CursorActivity,
        tier: CursorTier,
        suite_created_at: Option<Timestamp>,
        rounds: Vec<CursorReviewRound>,
    ) -> CursorReport {
        CursorReport {
            activity,
            rounds,
            threads: BotThreadSummary::default(),
            severity: CursorSeverityBreakdown::default(),
            tier,
            fresh: false,
            suite_created_at,
        }
    }

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn cursor_round(at: &str, sha: &str) -> CursorReviewRound {
        CursorReviewRound {
            round: 1,
            reviewed_at: ts(at),
            commit: GitCommitSha::parse(sha).unwrap(),
            findings_count: 0,
        }
    }

    #[test]
    fn escalate_cursor_stalled_prompt_surfaces_suite_age_and_round_count() {
        let r = report_with(
            CursorActivity::InFlight(InFlightHealth::Failed),
            CursorTier::Silver,
            Some(ts("2026-05-15T08:00:00Z")),
            vec![
                cursor_round("2026-05-10T10:00:00Z", &"a".repeat(40)),
                cursor_round("2026-05-12T10:00:00Z", &"b".repeat(40)),
            ],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Cursor Bugbot has not produced a review"));
        assert!(
            rendered.contains("Suite opened at: 2026-05-15T08:00:00+00:00"),
            "missing suite_created_at: {rendered}",
        );
        assert!(
            rendered.contains("Prior Cursor review rounds on this PR: 2 (tier: silver)"),
            "missing round count + tier: {rendered}",
        );
    }

    #[test]
    fn escalate_cursor_stalled_prompt_uses_step_form_for_actions() {
        let r = report_with(
            CursorActivity::InFlight(InFlightHealth::Failed),
            CursorTier::Bronze,
            Some(ts("2026-05-15T08:00:00Z")),
            vec![],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Step 1"), "missing Step 1: {rendered}");
        assert!(rendered.contains("Step 2"), "missing Step 2: {rendered}");
        assert!(rendered.contains("cursor.com/status"), "{rendered}");
        assert!(
            rendered.contains("push a new commit"),
            "step 2 must direct the user to push a new commit: {rendered}",
        );
        let step1 = rendered.find("Step 1").expect("step 1 present");
        let step2 = rendered.find("Step 2").expect("step 2 present");
        assert!(step1 < step2, "Step 1 must precede Step 2");
    }

    #[test]
    fn escalate_cursor_stalled_prompt_omits_suite_age_when_absent() {
        // Defensive path: a Failed in-flight activity normally implies
        // a suite exists, but the projection is optional so the prompt
        // must still render usefully without it.
        let r = report_with(
            CursorActivity::InFlight(InFlightHealth::Failed),
            CursorTier::Bronze,
            None,
            vec![],
        );
        let cs = candidates(&r);
        let rendered = cs[0].rendered_payload();
        assert!(!rendered.contains("Suite opened at:"));
        assert!(rendered.contains("Prior Cursor review rounds on this PR: 0 (tier: bronze)"));
    }
}
