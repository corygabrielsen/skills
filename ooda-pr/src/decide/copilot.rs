//! Copilot candidates: wait when Copilot is mid-cycle, advance
//! tier when it has reviewed but more is achievable.

use crate::ids::BlockerKey;
use std::time::Duration;

use crate::orient::copilot::{CopilotActivity, CopilotReport, CopilotTier};

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};

pub fn candidates(report: &CopilotReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    match &report.activity {
        CopilotActivity::Idle => {
            // Absence of signal is not a blocker — don't emit anything.
        }
        CopilotActivity::Requested { .. } => {
            out.push(Action {
                kind: ActionKind::WaitForCopilotAck,
                automation: Automation::Wait { interval: Duration::from_secs(15) },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: "Waiting for Copilot to start reviewing".into(),
                blocker: BlockerKey::tag("copilot_not_acked"),
            });
        }
        CopilotActivity::Working { .. } => {
            out.push(Action {
                kind: ActionKind::WaitForCopilotReview,
                automation: Automation::Wait { interval: Duration::from_secs(60) },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: "Waiting for Copilot to finish reviewing".into(),
                blocker: BlockerKey::tag("copilot_reviewing"),
            });
        }
        CopilotActivity::Reviewed { latest } => {
            if report.tier == CopilotTier::Platinum {
                return out;
            }
            // Unresolved threads are the reviews axis's job, not ours.
            if report.threads.unresolved > 0 {
                return out;
            }
            let stale = report.threads.stale;
            let not_at_head = !report.fresh;
            let suppressed = latest.comments_suppressed;

            // Rerequest dominates when stale OR not-fresh — a fresh
            // pass clears stale replies AND may resolve suppressed
            // findings in one shot.
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
                    kind: ActionKind::RerequestCopilot,
                    automation: Automation::Full,
                    target_effect: TargetEffect::Advances,
                    urgency: Urgency::Critical,
                    description: desc,
                    blocker: BlockerKey::tag(format!("copilot_tier_{}", report.tier.slug())),
                });
            } else if report.tier == CopilotTier::Silver && suppressed > 0 {
                out.push(Action {
                    kind: ActionKind::AddressCopilotSuppressed { count: suppressed },
                    automation: Automation::Agent,
                    target_effect: TargetEffect::Advances,
                    urgency: Urgency::Advancing,
                    description: format!(
                        "Copilot flagged {}. Investigate and push fixes for any \
                         that are real — the next review may clear them.",
                        crate::text::count(suppressed as usize, "low-confidence finding"),
                    ),
                    blocker: BlockerKey::tag("copilot_tier_silver"),
                });
            }
        }
    }

    out
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

    fn report(activity: CopilotActivity, tier: CopilotTier, threads: BotThreadSummary, fresh: bool) -> CopilotReport {
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
    fn requested_emits_wait_for_ack() {
        let r = report(
            CopilotActivity::Requested {
                requested_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            },
            CopilotTier::Bronze,
            BotThreadSummary::default(),
            false,
        );
        let cs = candidates(&r);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCopilotAck));
        assert!(matches!(cs[0].automation, Automation::Wait { .. }));
    }

    #[test]
    fn platinum_at_head_yields_no_candidates() {
        let r = report(
            CopilotActivity::Reviewed { latest: round_at_head() },
            CopilotTier::Platinum,
            BotThreadSummary::default(),
            true,
        );
        assert!(candidates(&r).is_empty());
    }

    #[test]
    fn gold_not_fresh_emits_rerequest() {
        let r = report(
            CopilotActivity::Reviewed { latest: round_at_head() },
            CopilotTier::Gold,
            BotThreadSummary::default(),
            false, // not at HEAD
        );
        let cs = candidates(&r);
        assert!(matches!(cs[0].kind, ActionKind::RerequestCopilot));
        assert_eq!(cs[0].automation, Automation::Full);
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
        assert_eq!(cs[0].automation, Automation::Agent);
    }

    #[test]
    fn unresolved_threads_block_tier_advancement_at_copilot_layer() {
        let r = report(
            CopilotActivity::Reviewed { latest: round_at_head() },
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
}
