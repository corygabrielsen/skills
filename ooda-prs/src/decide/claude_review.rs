//! Claude-review candidate.
//!
//! Content-keyed attestation: fires only when the axis reports
//! review content past the last attestation. The quiet states
//! (no surface to grade, already addressed) emit nothing because
//! there is no agent action to take. Hygiene tier — advisory
//! rather than blocking.

use crate::act::address_claude_review::build_address_claude_review_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::claude_review::ClaudeReview;

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

/// Declared deps: own report + own attest-path location.
#[must_use]
pub(crate) fn candidates(
    claude_review: &ClaudeReview,
    attest_path: Option<&std::path::Path>,
    pr: PullRequestNumber,
) -> Vec<Action> {
    let ClaudeReview::Fresh {
        body_at,
        latest_claude_body,
        latest_claude_url,
        inline_thread_count,
        ..
    } = claude_review
    else {
        return Vec::new();
    };
    let Some(attest_path) = attest_path else {
        return Vec::new();
    };

    let prompt = build_address_claude_review_prompt(
        pr,
        *body_at,
        latest_claude_body,
        latest_claude_url,
        *inline_thread_count,
        Some(attest_path),
    );
    let kind = ActionKind::AddressClaudeReview {
        attest_path: attest_path.to_path_buf(),
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::Mid(MidTier::Hygiene),
        blocker: BlockerKey::from_static("claude_review_fresh"),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber};
    use chrono::{DateTime, Utc};
    use ooda_core::MidTier;

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn fresh() -> ClaudeReview {
        let at = DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        ClaudeReview::Fresh {
            latest_claude_at: at,
            body_at: at,
            latest_claude_body: "🔴 important".into(),
            latest_claude_url: "https://example/r/1".into(),
            inline_thread_count: 1,
            attested_at: None,
            head_sha: GitCommitSha::parse(HEAD_SHA).unwrap().as_str().to_string(),
        }
    }

    fn attest_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/state/753/claude_review_attest.json")
    }

    #[test]
    fn fresh_emits_address_claude_review() {
        let cs = candidates(&fresh(), Some(&attest_path()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::AddressClaudeReview { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Hygiene));
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "claude_review_fresh");
    }

    #[test]
    fn no_activity_emits_nothing() {
        let cs = candidates(&ClaudeReview::NoActivity, Some(&attest_path()), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn addressed_emits_nothing() {
        let cs = candidates(&ClaudeReview::Addressed, Some(&attest_path()), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn fresh_with_no_attest_path_emits_nothing() {
        assert!(candidates(&fresh(), None, pr()).is_empty());
    }

    #[test]
    fn address_claude_review_carries_attest_path_in_payload() {
        let cs = candidates(&fresh(), Some(&attest_path()), pr());
        let ActionKind::AddressClaudeReview { attest_path } = &cs[0].kind else {
            panic!("expected AddressClaudeReview");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/claude_review_attest.json")
        );
    }

    #[test]
    fn address_claude_review_action_name_is_address_claude_review() {
        let a = candidates(&fresh(), Some(&attest_path()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "AddressClaudeReview");
    }
}
