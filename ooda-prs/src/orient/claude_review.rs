//! Claude-review attestation axis.
//!
//! Diverges from the SHA-based `pull_request_metadata` / `doc_review`
//! axes: Claude does not re-fire on `synchronize`, so HEAD drift past
//! the attested SHA is not actionable on its own. The signal is
//! *content drift* — has Claude posted new review material past the
//! last attestation? — projected to `NoActivity` / `Addressed` /
//! `Fresh`. Aggregates across three GitHub surfaces (review
//! submissions, issue comments, inline review threads).

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::observe::github::claude_review_attest::ClaudeReviewObservation;

// ── Identity ─────────────────────────────────────────────────────────

const CLAUDE_LOGINS: &[&str] = &["claude[bot]", "claude"];

pub fn is_claude(login: &str) -> bool {
    CLAUDE_LOGINS.contains(&login)
}

// ── Axis projection ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClaudeReview {
    NoActivity,
    Addressed,
    Fresh {
        latest_claude_at: DateTime<Utc>,
        latest_claude_body: String,
        latest_claude_url: String,
        inline_thread_count: usize,
        attested_at: Option<DateTime<Utc>>,
        head_sha: String,
    },
}

/// Project a [`ClaudeReviewObservation`] into the typed axis.
#[must_use]
pub fn orient_claude_review(obs: &ClaudeReviewObservation) -> ClaudeReview {
    let Some(latest_at) = obs.latest_claude_at else {
        return ClaudeReview::NoActivity;
    };
    let attested_at = obs.attestation.as_ref().map(|a| a.attested_at);
    if let Some(attested_at) = attested_at
        && latest_at <= attested_at
    {
        return ClaudeReview::Addressed;
    }
    ClaudeReview::Fresh {
        latest_claude_at: latest_at,
        latest_claude_body: obs.latest_claude_body.clone().unwrap_or_default(),
        latest_claude_url: obs.latest_claude_url.clone().unwrap_or_default(),
        inline_thread_count: obs.inline_thread_count,
        attested_at,
        head_sha: obs.head_sha.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitCommitSha;
    use ooda_core::attest::{CLAUDE_REVIEW_SCHEMA_VERSION, ClaudeReviewAttestation};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    fn attestation(at: DateTime<Utc>) -> ClaudeReviewAttestation {
        ClaudeReviewAttestation {
            attested_sha: HEAD_SHA.to_string(),
            attested_at: at,
            version: CLAUDE_REVIEW_SCHEMA_VERSION,
        }
    }

    #[test]
    fn is_claude_matches_both_login_forms() {
        assert!(is_claude("claude[bot]"));
        assert!(is_claude("claude"));
        assert!(!is_claude("Claude"));
        assert!(!is_claude("claude-bot"));
        assert!(!is_claude("copilot[bot]"));
    }

    #[test]
    fn no_claude_activity_yields_no_activity() {
        let obs = ClaudeReviewObservation {
            attestation: None,
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: None,
            latest_claude_body: None,
            latest_claude_url: None,
            inline_thread_count: 0,
        };
        assert_eq!(orient_claude_review(&obs), ClaudeReview::NoActivity);
    }

    #[test]
    fn claude_content_older_than_attestation_yields_addressed() {
        let posted = DateTime::parse_from_rfc3339("2026-05-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let attested = DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let obs = ClaudeReviewObservation {
            attestation: Some(attestation(attested)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: Some(posted),
            latest_claude_body: Some("nit".into()),
            latest_claude_url: Some("https://example".into()),
            inline_thread_count: 0,
        };
        assert_eq!(orient_claude_review(&obs), ClaudeReview::Addressed);
    }

    #[test]
    fn claude_content_at_attestation_timestamp_still_addressed() {
        let when = DateTime::parse_from_rfc3339("2026-05-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let obs = ClaudeReviewObservation {
            attestation: Some(attestation(when)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: Some(when),
            latest_claude_body: None,
            latest_claude_url: None,
            inline_thread_count: 0,
        };
        assert_eq!(orient_claude_review(&obs), ClaudeReview::Addressed);
    }

    #[test]
    fn claude_content_past_attestation_yields_fresh() {
        let attested = DateTime::parse_from_rfc3339("2026-05-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let posted = DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let obs = ClaudeReviewObservation {
            attestation: Some(attestation(attested)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: Some(posted),
            latest_claude_body: Some("important finding".into()),
            latest_claude_url: Some("https://example/r/1".into()),
            inline_thread_count: 2,
        };
        match orient_claude_review(&obs) {
            ClaudeReview::Fresh {
                latest_claude_at,
                latest_claude_body,
                latest_claude_url,
                inline_thread_count,
                attested_at,
                head_sha,
            } => {
                assert_eq!(latest_claude_at, posted);
                assert_eq!(latest_claude_body, "important finding");
                assert_eq!(latest_claude_url, "https://example/r/1");
                assert_eq!(inline_thread_count, 2);
                assert_eq!(attested_at, Some(attested));
                assert_eq!(head_sha, HEAD_SHA);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[test]
    fn claude_content_without_attestation_yields_fresh() {
        let posted = DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let obs = ClaudeReviewObservation {
            attestation: None,
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: Some(posted),
            latest_claude_body: Some("first review".into()),
            latest_claude_url: Some("https://example/r/2".into()),
            inline_thread_count: 0,
        };
        match orient_claude_review(&obs) {
            ClaudeReview::Fresh {
                attested_at,
                latest_claude_body,
                ..
            } => {
                assert_eq!(attested_at, None);
                assert_eq!(latest_claude_body, "first review");
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }
}
