//! Reviewer-verdict obligations: which verdict assessments still
//! need an answer, and which answers wait on a human.
//!
//! # Invariants
//!
//! - **Latest reviewed round only**: the pending obligation is drawn
//!   from the last round that carries a review, at HEAD or not. A
//!   newer review supersedes an unanswered older one; a request still
//!   in flight supersedes nothing.
//! - **Id-keyed discharge**: an entry discharges exactly the review it
//!   names. A review submitted after an entry was written is never
//!   discharged by it.
//! - **Escalations persist**: an escalated entry stays an obligation
//!   across later reviews until the same review is re-recorded with
//!   another disposition.
//! - **Fail closed**: only an approval-stance verdict carries no
//!   obligation.
//! - **Pure projection**: no clock, no network. Same input → same
//!   output.

use ooda_core::attest::{CopilotVerdictAttestation, CopilotVerdictEntry};
use serde::Serialize;

use crate::observe::github::copilot_verdict_attest::CopilotVerdictObservation;
use crate::orient::copilot::{CopilotReport, CopilotVerdict, VerdictStance};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct CopilotVerdictReport {
    /// Verdict of the latest reviewed round, any stance.
    pub latest: Option<CopilotVerdict>,
    /// Recorded disposition for `latest`, if any.
    pub latest_entry: Option<CopilotVerdictEntry>,
    /// Escalated entries, each with the verdict it answers when that
    /// review is still among the observed rounds.
    pub escalated: Vec<Escalation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Escalation {
    pub entry: CopilotVerdictEntry,
    pub verdict: Option<CopilotVerdict>,
}

impl CopilotVerdictReport {
    /// The latest verdict, when it is a concern no entry answers.
    #[must_use]
    pub(crate) fn pending(&self) -> Option<&CopilotVerdict> {
        self.latest
            .as_ref()
            .filter(|v| v.stance == VerdictStance::Concern && self.latest_entry.is_none())
    }
}

/// Join the latest reviewed round's verdict and every escalation
/// against the recorded entries.
#[must_use]
pub(crate) fn orient_copilot_verdict(
    obs: &CopilotVerdictObservation,
    copilot: Option<&CopilotReport>,
) -> CopilotVerdictReport {
    let rounds = copilot.map_or(&[][..], |c| c.rounds.as_slice());
    let latest = rounds
        .iter()
        .rev()
        .find(|r| r.reviewed_at.is_some())
        .and_then(|r| r.verdict.clone());
    let attestation = obs.attestation.as_ref();
    let latest_entry = latest
        .as_ref()
        .and_then(|v| attestation.and_then(|a| a.entry(v.review_id)))
        .cloned();
    let escalated = attestation
        .map(CopilotVerdictAttestation::escalated)
        .into_iter()
        .flatten()
        .map(|entry| Escalation {
            verdict: rounds
                .iter()
                .filter_map(|r| r.verdict.as_ref())
                .find(|v| v.review_id == entry.review_id)
                .cloned(),
            entry: entry.clone(),
        })
        .collect();
    CopilotVerdictReport {
        latest,
        latest_entry,
        escalated,
    }
}

/// Dashboard projection. `None` when no verdict exists and nothing is
/// escalated.
pub(crate) fn copilot_verdict_signal(
    report: &CopilotVerdictReport,
) -> Option<crate::dashboard::AxisSignal> {
    use crate::dashboard::{AxisName, AxisSignal, SignalIcon};
    let (icon, summary) = if let Some(e) = report.escalated.first() {
        (
            SignalIcon::Failed,
            format!(
                "{} escalated to a human (review {})",
                crate::text::count(report.escalated.len(), "verdict"),
                e.entry.review_id,
            ),
        )
    } else {
        let v = report.latest.as_ref()?;
        match (&report.latest_entry, v.stance) {
            (None, VerdictStance::Approve) => (SignalIcon::Ok, v.heading.clone()),
            (None, VerdictStance::Concern) => {
                (SignalIcon::Warn, format!("{} — unanswered", v.heading))
            }
            (Some(entry), _) => (
                SignalIcon::Ok,
                format!("{} — {}: {}", v.heading, entry.disposition, entry.note),
            ),
        }
    };
    Some(AxisSignal {
        axis: AxisName::CopilotVerdict,
        icon,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, Timestamp};
    use crate::orient::bot_threads::BotThreadSummary;
    use crate::orient::copilot::{
        CopilotActivity, CopilotRepoConfig, CopilotReviewRound, CopilotTier,
    };
    use crate::orient::thread::SeverityBreakdown;
    use ooda_core::attest::VerdictDisposition;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn verdict(id: u64, stance: VerdictStance) -> CopilotVerdict {
        CopilotVerdict {
            review_id: id,
            url: format!("https://github.com/o/r/pull/1#pullrequestreview-{id}"),
            stance,
            heading: match stance {
                VerdictStance::Approve => "\u{1f7e2} Approval recommended".into(),
                VerdictStance::Concern => "\u{1f535} Needs a closer look".into(),
            },
            assessment: format!("assessment {id}"),
        }
    }

    fn round(n: u32, reviewed: bool, v: Option<CopilotVerdict>) -> CopilotReviewRound {
        let at = Timestamp::parse("2026-10-28T10:00:00Z").unwrap();
        CopilotReviewRound {
            round: n,
            requested_at: at,
            ack_at: None,
            reviewed_at: reviewed.then_some(at),
            commit: Some(GitCommitSha::parse(SHA).unwrap()),
            comments_visible: 0,
            comments_suppressed: 0,
            suppressed_comments: Vec::new(),
            severity: SeverityBreakdown::default(),
            verdict: v,
        }
    }

    fn report(rounds: Vec<CopilotReviewRound>) -> CopilotReport {
        CopilotReport {
            config: CopilotRepoConfig {
                enabled: true,
                review_on_push: true,
                review_draft_pull_requests: false,
            },
            activity: CopilotActivity::Idle,
            rounds,
            threads: BotThreadSummary::default(),
            tier: CopilotTier::Platinum,
            fresh: true,
        }
    }

    fn entry(id: u64, disposition: VerdictDisposition) -> CopilotVerdictEntry {
        CopilotVerdictEntry {
            review_id: id,
            disposition,
            note: "grounds".into(),
            attested_sha: SHA.into(),
            attested_at: chrono::Utc::now(),
        }
    }

    fn obs(entries: Vec<CopilotVerdictEntry>) -> CopilotVerdictObservation {
        CopilotVerdictObservation {
            attestation: Some(CopilotVerdictAttestation {
                version: ooda_core::attest::COPILOT_VERDICT_SCHEMA_VERSION,
                entries,
            }),
            attest_path: None,
        }
    }

    #[test]
    fn unanswered_concern_is_pending() {
        let c = report(vec![round(
            1,
            true,
            Some(verdict(7, VerdictStance::Concern)),
        )]);
        let r = orient_copilot_verdict(&obs(vec![]), Some(&c));
        assert_eq!(r.pending().unwrap().review_id, 7);
    }

    #[test]
    fn approval_is_never_pending() {
        let c = report(vec![round(
            1,
            true,
            Some(verdict(7, VerdictStance::Approve)),
        )]);
        let r = orient_copilot_verdict(&obs(vec![]), Some(&c));
        assert!(r.pending().is_none());
        assert!(r.latest.is_some());
    }

    #[test]
    fn entry_discharges_only_the_review_it_names() {
        let c = report(vec![
            round(1, true, Some(verdict(7, VerdictStance::Concern))),
            round(2, true, Some(verdict(8, VerdictStance::Concern))),
        ]);
        let r = orient_copilot_verdict(&obs(vec![entry(7, VerdictDisposition::Patched)]), Some(&c));
        assert_eq!(r.pending().unwrap().review_id, 8);

        let r =
            orient_copilot_verdict(&obs(vec![entry(8, VerdictDisposition::NoChange)]), Some(&c));
        assert!(r.pending().is_none());
    }

    #[test]
    fn in_flight_round_does_not_supersede_last_reviewed_verdict() {
        let c = report(vec![
            round(1, true, Some(verdict(7, VerdictStance::Concern))),
            round(2, false, None),
        ]);
        let r = orient_copilot_verdict(&obs(vec![]), Some(&c));
        assert_eq!(r.pending().unwrap().review_id, 7);
    }

    #[test]
    fn newer_review_supersedes_older_unanswered_concern() {
        let c = report(vec![
            round(1, true, Some(verdict(7, VerdictStance::Concern))),
            round(2, true, Some(verdict(8, VerdictStance::Approve))),
        ]);
        let r = orient_copilot_verdict(&obs(vec![]), Some(&c));
        assert!(r.pending().is_none());
    }

    #[test]
    fn escalation_persists_across_later_reviews() {
        let c = report(vec![
            round(1, true, Some(verdict(7, VerdictStance::Concern))),
            round(2, true, Some(verdict(8, VerdictStance::Concern))),
        ]);
        let r = orient_copilot_verdict(
            &obs(vec![
                entry(7, VerdictDisposition::Escalate),
                entry(8, VerdictDisposition::Patched),
            ]),
            Some(&c),
        );
        assert!(r.pending().is_none());
        assert_eq!(r.escalated.len(), 1);
        assert_eq!(r.escalated[0].entry.review_id, 7);
        assert_eq!(
            r.escalated[0].verdict.as_ref().unwrap().assessment,
            "assessment 7"
        );
    }

    #[test]
    fn escalated_latest_is_not_pending() {
        let c = report(vec![round(
            1,
            true,
            Some(verdict(7, VerdictStance::Concern)),
        )]);
        let r =
            orient_copilot_verdict(&obs(vec![entry(7, VerdictDisposition::Escalate)]), Some(&c));
        assert!(r.pending().is_none());
        assert_eq!(r.escalated.len(), 1);
    }

    #[test]
    fn no_copilot_and_no_attestation_is_empty() {
        let r = orient_copilot_verdict(
            &CopilotVerdictObservation {
                attestation: None,
                attest_path: None,
            },
            None,
        );
        assert_eq!(r, CopilotVerdictReport::default());
        assert!(copilot_verdict_signal(&r).is_none());
    }

    #[test]
    fn signal_tracks_obligation_state() {
        use crate::dashboard::SignalIcon;
        let c = report(vec![round(
            1,
            true,
            Some(verdict(7, VerdictStance::Concern)),
        )]);
        let pending = orient_copilot_verdict(&obs(vec![]), Some(&c));
        let s = copilot_verdict_signal(&pending).unwrap();
        assert_eq!(s.icon, SignalIcon::Warn);
        assert!(s.summary.ends_with("unanswered"), "{}", s.summary);

        let answered =
            orient_copilot_verdict(&obs(vec![entry(7, VerdictDisposition::NoChange)]), Some(&c));
        let s = copilot_verdict_signal(&answered).unwrap();
        assert_eq!(s.icon, SignalIcon::Ok);
        assert!(s.summary.contains("no-change: grounds"), "{}", s.summary);

        let escalated =
            orient_copilot_verdict(&obs(vec![entry(7, VerdictDisposition::Escalate)]), Some(&c));
        assert_eq!(
            copilot_verdict_signal(&escalated).unwrap().icon,
            SignalIcon::Failed
        );
    }
}
