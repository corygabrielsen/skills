//! Review-class attestation axis. Content-keyed like the Claude-review
//! axis, but spanning every reviewer.
//!
//! # Invariants
//!
//! - **Content drift, not SHA drift**: the trigger is a review
//!   thread created after the last attestation. Resolution state is
//!   irrelevant — resolving a thread clears the reviewer's anchor,
//!   it does not prove the class is gone.
//! - **Every thread author counts**: bot and human threads alike
//!   re-arm the axis. Copilot's suppressed findings count too, on the
//!   same freshness gate the reviews axis uses to synthesise them.
//! - **Absence ≠ addressed**: threads with no attestation are
//!   `Fresh`, never `Attested`. A never-recorded sweep is not a
//!   completed one.
//! - **Pure projection**: no clock, no network. Same input → same
//!   output.

use chrono::{DateTime, Utc};
use ooda_core::attest::ReviewClassAttestation;
use serde::Serialize;

use crate::observe::github::review_class_attest::ReviewClassObservation;
use crate::orient::copilot::CopilotReport;
use crate::orient::thread::ReviewThread;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum ReviewClass {
    /// No review thread exists on the PR from any source.
    NoThreads,
    /// Every thread predates the attestation.
    Attested {
        attested_at: DateTime<Utc>,
        class_count: usize,
    },
    /// At least one thread is newer than the attestation, or threads
    /// exist and nothing has ever been attested.
    Fresh {
        /// Newest thread creation time across every source.
        latest_thread_at: DateTime<Utc>,
        /// Threads newer than the attestation (every thread when no
        /// attestation exists).
        fresh_thread_count: usize,
        /// The prior attestation, when one exists. Surfaced so the
        /// prompt can show which classes were already claimed swept.
        prior: Option<ReviewClassAttestation>,
    },
}

/// Project the attestation against every thread source.
///
/// `threads` are the projected host threads; `copilot` contributes
/// the latest round's suppressed findings under the same freshness
/// gate the reviews axis uses to synthesise them as threads.
#[must_use]
pub(crate) fn orient_review_class(
    obs: &ReviewClassObservation,
    threads: &[ReviewThread],
    copilot: Option<&CopilotReport>,
) -> ReviewClass {
    let mut times: Vec<DateTime<Utc>> = threads.iter().map(|t| t.created_at.at()).collect();
    if let Some(c) = copilot
        && c.fresh
        && let Some(round) = c.rounds.last()
        && !round.suppressed_comments.is_empty()
        && let Some(at) = round.reviewed_at
    {
        times.extend(std::iter::repeat_n(
            at.at(),
            round.suppressed_comments.len(),
        ));
    }
    let Some(latest_thread_at) = times.iter().copied().max() else {
        return ReviewClass::NoThreads;
    };
    match &obs.attestation {
        Some(att) if latest_thread_at <= att.attested_at => ReviewClass::Attested {
            attested_at: att.attested_at,
            class_count: att.classes.len(),
        },
        Some(att) => ReviewClass::Fresh {
            latest_thread_at,
            fresh_thread_count: times.iter().filter(|t| **t > att.attested_at).count(),
            prior: Some(att.clone()),
        },
        None => ReviewClass::Fresh {
            latest_thread_at,
            fresh_thread_count: times.len(),
            prior: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, Timestamp};
    use crate::orient::thread::{
        BotName, FilePath, ThreadAuthor, ThreadId, ThreadLocation, ThreadState,
    };
    use ooda_core::attest::{REVIEW_CLASS_SCHEMA_VERSION, ReviewClassEntry, ReviewSite};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn attestation(when: &str) -> ReviewClassAttestation {
        ReviewClassAttestation {
            attested_sha: HEAD_SHA.to_string(),
            attested_at: at(when),
            version: REVIEW_CLASS_SCHEMA_VERSION,
            classes: vec![ReviewClassEntry {
                class: "unwrap in library code".into(),
                sites: vec![ReviewSite {
                    path: "src/a.rs".into(),
                    line: 1,
                }],
            }],
        }
    }

    fn obs(attestation: Option<ReviewClassAttestation>) -> ReviewClassObservation {
        ReviewClassObservation {
            attestation,
            head_sha: head(),
            attest_path: None,
        }
    }

    fn thread(created: &str, state: ThreadState) -> ReviewThread {
        ReviewThread {
            id: ThreadId::new("t").unwrap(),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new("src/a.rs").unwrap(),
                line: Some(1),
            },
            body: "x".into(),
            state,
            created_at: Timestamp::parse(created).unwrap(),
            originating_comment_id: None,
        }
    }

    #[test]
    fn no_threads_yields_no_threads_regardless_of_attestation() {
        assert_eq!(
            orient_review_class(&obs(None), &[], None),
            ReviewClass::NoThreads
        );
        assert_eq!(
            orient_review_class(&obs(Some(attestation("2026-05-02T10:00:00Z"))), &[], None),
            ReviewClass::NoThreads
        );
    }

    #[test]
    fn threads_without_attestation_are_fresh() {
        let threads = vec![
            thread("2026-05-01T10:00:00Z", ThreadState::Resolved),
            thread("2026-05-02T10:00:00Z", ThreadState::Live),
        ];
        match orient_review_class(&obs(None), &threads, None) {
            ReviewClass::Fresh {
                latest_thread_at,
                fresh_thread_count,
                prior,
            } => {
                assert_eq!(latest_thread_at, at("2026-05-02T10:00:00Z"));
                assert_eq!(fresh_thread_count, 2);
                assert!(prior.is_none());
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[test]
    fn resolved_threads_still_count_as_fresh() {
        // Resolution clears the reviewer's anchor; it is not the
        // sweep witness.
        let threads = vec![thread("2026-05-02T10:00:00Z", ThreadState::Resolved)];
        assert!(matches!(
            orient_review_class(&obs(None), &threads, None),
            ReviewClass::Fresh { .. }
        ));
    }

    #[test]
    fn threads_older_than_attestation_are_attested() {
        let threads = vec![thread("2026-05-01T10:00:00Z", ThreadState::Resolved)];
        let o = obs(Some(attestation("2026-05-02T10:00:00Z")));
        match orient_review_class(&o, &threads, None) {
            ReviewClass::Attested {
                attested_at,
                class_count,
            } => {
                assert_eq!(attested_at, at("2026-05-02T10:00:00Z"));
                assert_eq!(class_count, 1);
            }
            other => panic!("expected Attested, got {other:?}"),
        }
    }

    #[test]
    fn thread_at_attestation_instant_is_attested() {
        let threads = vec![thread("2026-05-02T10:00:00Z", ThreadState::Live)];
        let o = obs(Some(attestation("2026-05-02T10:00:00Z")));
        assert!(matches!(
            orient_review_class(&o, &threads, None),
            ReviewClass::Attested { .. }
        ));
    }

    #[test]
    fn newer_thread_re_arms_with_prior_and_counts_only_fresh() {
        let threads = vec![
            thread("2026-05-01T10:00:00Z", ThreadState::Resolved),
            thread("2026-05-03T10:00:00Z", ThreadState::Live),
        ];
        let o = obs(Some(attestation("2026-05-02T10:00:00Z")));
        match orient_review_class(&o, &threads, None) {
            ReviewClass::Fresh {
                latest_thread_at,
                fresh_thread_count,
                prior,
            } => {
                assert_eq!(latest_thread_at, at("2026-05-03T10:00:00Z"));
                assert_eq!(fresh_thread_count, 1);
                assert_eq!(prior.unwrap().classes[0].class, "unwrap in library code");
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    fn copilot_with_suppressed(reviewed_at: &str, fresh: bool, n: usize) -> CopilotReport {
        use crate::orient::bot_threads::BotThreadSummary;
        use crate::orient::copilot::{
            CopilotActivity, CopilotRepoConfig, CopilotReviewRound, CopilotTier, SuppressedComment,
        };
        let round = CopilotReviewRound {
            round: 1,
            requested_at: Timestamp::parse(reviewed_at).unwrap(),
            ack_at: None,
            reviewed_at: Some(Timestamp::parse(reviewed_at).unwrap()),
            commit: Some(head()),
            comments_visible: 0,
            comments_suppressed: u32::try_from(n).unwrap(),
            suppressed_comments: (0..n)
                .map(|i| SuppressedComment {
                    path: "src/a.rs".into(),
                    line: u32::try_from(i).unwrap() + 1,
                    body: "s".into(),
                })
                .collect(),
        };
        CopilotReport {
            config: CopilotRepoConfig {
                enabled: true,
                review_on_push: false,
                review_draft_pull_requests: false,
            },
            activity: CopilotActivity::Reviewed {
                latest: round.clone(),
            },
            rounds: vec![round],
            threads: BotThreadSummary::default(),
            tier: CopilotTier::Silver,
            fresh,
        }
    }

    #[test]
    fn fresh_suppressed_findings_count_as_threads() {
        let c = copilot_with_suppressed("2026-05-02T10:00:00Z", true, 2);
        match orient_review_class(&obs(None), &[], Some(&c)) {
            ReviewClass::Fresh {
                fresh_thread_count, ..
            } => assert_eq!(fresh_thread_count, 2),
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[test]
    fn stale_suppressed_findings_do_not_count() {
        // Same gate the reviews axis uses: a round not at HEAD is
        // keyed to an old commit and must not re-arm the axis.
        let c = copilot_with_suppressed("2026-05-02T10:00:00Z", false, 2);
        assert_eq!(
            orient_review_class(&obs(None), &[], Some(&c)),
            ReviewClass::NoThreads
        );
    }
}
