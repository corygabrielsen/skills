//! Cursor candidates: wait when Bugbot is mid-review.
//!
//! Cursor has no rerequest API; tier advancement past the current
//! state requires a code push (Bugbot auto-runs on push). So
//! "Reviewed at non-HEAD" doesn't yield an action — we just wait
//! for the user's next push to trigger Bugbot.

use crate::ids::BlockerKey;
use std::time::Duration;

use crate::orient::cursor::{CursorActivity, CursorReport};

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};

pub fn candidates(report: &CursorReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    if matches!(report.activity, CursorActivity::Reviewing) {
        out.push(Action {
            kind: ActionKind::WaitForCursorReview,
            automation: Automation::Wait { interval: Duration::from_secs(60) },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: "Waiting for Cursor Bugbot to finish reviewing".into(),
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
        CursorActivity, CursorReport, CursorReviewRound, CursorSeverityBreakdown,
        CursorTier,
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
        assert!(matches!(cs[0].automation, Automation::Wait { .. }));
    }

    #[test]
    fn reviewed_yields_no_candidate_no_rerequest_api() {
        let cs = candidates(&report(
            CursorActivity::Reviewed { latest: round() },
            CursorTier::Gold,
        ));
        assert!(cs.is_empty());
    }
}
