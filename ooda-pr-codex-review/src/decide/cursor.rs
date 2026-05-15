//! Cursor candidates: wait when Bugbot is mid-review.
//!
//! Cursor has no rerequest API; tier advancement past the current
//! state requires a code push (Bugbot auto-runs on push). So
//! "Reviewed at non-HEAD" doesn't yield an action — we just wait
//! for the user's next push to trigger Bugbot.

use crate::ids::BlockerKey;

use crate::orient::cursor::{CursorActivity, CursorReport};

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

pub fn candidates(report: &CursorReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    if matches!(report.activity, CursorActivity::Reviewing) {
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, Timestamp};
    use crate::orient::bot_threads::BotThreadSummary;
    use crate::orient::cursor::{
        CursorActivity, CursorReport, CursorReviewRound, CursorSeverityBreakdown, CursorTier,
    };

    fn round() -> CursorReviewRound {
        CursorReviewRound {
            round: 1,
            reviewed_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            commit: GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            findings_count: 0,
        }
    }

    fn report(activity: CursorActivity, tier: CursorTier) -> CursorReport {
        CursorReport {
            activity,
            rounds: vec![],
            threads: BotThreadSummary::default(),
            severity: CursorSeverityBreakdown::default(),
            tier,
            fresh: false,
        }
    }

    #[test]
    fn idle_yields_no_candidate() {
        assert!(candidates(&report(CursorActivity::Idle, CursorTier::Bronze)).is_empty());
    }

    #[test]
    fn clean_yields_no_candidate() {
        assert!(candidates(&report(CursorActivity::Clean, CursorTier::Platinum)).is_empty());
    }

    #[test]
    fn reviewing_emits_wait() {
        let cs = candidates(&report(CursorActivity::Reviewing, CursorTier::Silver));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCursorReview));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
    }

    #[test]
    fn reviewed_yields_no_candidate_no_rerequest_api() {
        let cs = candidates(&report(
            CursorActivity::Reviewed { latest: round() },
            CursorTier::Gold,
        ));
        assert!(cs.is_empty());
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // The cursor axis's class invariant: every `CursorActivity`
    // variant has a deterministic candidate-set fingerprint. Only
    // `Reviewing` emits a candidate (the WaitForCursorReview); all
    // other states are silent — Idle and Clean by intent, Reviewed
    // because Cursor has no rerequest API (tier advancement
    // requires a code push, which the agent does independently).
    //
    // The exhaustive match in `expected_cursor_axis_behavior` is
    // the contract. Adding a new `CursorActivity` variant fails to
    // compile here until the new arm is added.

    #[derive(Debug, PartialEq, Eq)]
    enum CursorAxisBehavior {
        NoCandidate,
        EmitWaitForReview,
    }

    fn expected_cursor_axis_behavior(activity: &CursorActivity) -> CursorAxisBehavior {
        match activity {
            // No Cursor activity recorded yet — nothing to wait on.
            CursorActivity::Idle => CursorAxisBehavior::NoCandidate,
            // A review is mid-flight — wait for it to finish.
            CursorActivity::Reviewing => CursorAxisBehavior::EmitWaitForReview,
            // Bugbot has reviewed; no rerequest API exists, so the
            // axis stays silent. Tier advancement requires the agent
            // to push code, which fires Bugbot automatically.
            CursorActivity::Reviewed { .. } => CursorAxisBehavior::NoCandidate,
            // Cursor reports a clean review with no findings.
            CursorActivity::Clean => CursorAxisBehavior::NoCandidate,
        }
    }

    fn all_cursor_activities() -> Vec<CursorActivity> {
        vec![
            CursorActivity::Idle,
            CursorActivity::Reviewing,
            CursorActivity::Reviewed { latest: round() },
            CursorActivity::Clean,
        ]
    }

    fn observed_cursor_axis_behavior(cs: &[Action]) -> CursorAxisBehavior {
        match cs {
            [] => CursorAxisBehavior::NoCandidate,
            [a] if matches!(a.kind, ActionKind::WaitForCursorReview)
                && matches!(a.effect, ActionEffect::Wait { .. }) =>
            {
                CursorAxisBehavior::EmitWaitForReview
            }
            multi => panic!(
                "cursor axis emitted unexpected candidate set ({} items): {multi:?}",
                multi.len()
            ),
        }
    }

    #[test]
    fn cursor_axis_property_holds_for_every_activity() {
        let activities = all_cursor_activities();
        assert_eq!(
            activities.len(),
            4,
            "`all_cursor_activities` must include one sample per \
             `CursorActivity` variant; adding a new variant requires \
             adding both an arm in `expected_cursor_axis_behavior` AND \
             a sample here.",
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
}
