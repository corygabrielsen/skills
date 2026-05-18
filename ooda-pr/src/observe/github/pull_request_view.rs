//! Typed projection of the host's per-PR metadata view.
//!
//! The model carries only the fields downstream stages consume;
//! unmodeled fields are ignored by the decoder.

use serde::{Deserialize, Deserializer, Serialize};

use crate::ids::{BranchName, GitCommitSha, GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json};

/// Field-selection list submitted to the host view. Coupled to the
/// projection struct below; adding a field requires updating both
/// in lockstep.
const PR_FIELDS: &str = "title,number,url,body,state,isDraft,mergeable,\
     mergeStateStatus,headRefOid,headRefName,baseRefName,updatedAt,closedAt,\
     mergedAt,labels,assignees,reviewRequests,reviewDecision,commits,author";

/// Fetch PR metadata via `gh pr view`.
pub(crate) fn fetch_pull_request_view(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<PullRequestView, GhError> {
    let slug_s = slug.to_string();
    let pr_s = pr.to_string();
    gh_json(&["pr", "view", &pr_s, "-R", &slug_s, "--json", PR_FIELDS])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestView {
    pub title: String,
    pub number: PullRequestNumber,
    pub url: String,
    pub body: Option<String>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeable: Mergeable,
    pub merge_state_status: MergeStateStatus,
    pub head_ref_oid: GitCommitSha,
    pub head_ref_name: BranchName,
    pub base_ref_name: BranchName,
    pub updated_at: Timestamp,
    pub closed_at: Option<Timestamp>,
    pub merged_at: Option<Timestamp>,
    #[serde(default, deserialize_with = "deserialize_review_decision")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub assignees: Vec<Assignee>,
    #[serde(default)]
    pub review_requests: Vec<ReviewRequest>,
    #[serde(default)]
    pub commits: Vec<Commit>,
    /// PR author. Absent on deleted-identity (the host emits a
    /// ghost shape) or when the field never populated. Read by
    /// axes that classify by author class.
    #[serde(default)]
    pub author: Option<PullRequestAuthor>,
}

/// PR author identity. Carries only the login slug — bot-class
/// classification runs against the slug, so the wire model stays
/// narrow.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct PullRequestAuthor {
    /// Deleted-identity shapes (empty string, missing field) both
    /// decode to absence so downstream axes treat them uniformly.
    #[serde(default, deserialize_with = "deserialize_optional_login")]
    pub login: Option<GitHubLogin>,
}

fn deserialize_optional_login<'de, D>(d: D) -> Result<Option<GitHubLogin>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => GitHubLogin::parse(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

pub(crate) use ooda_core::PullRequestState;

// Variant name matches the host's wire vocabulary 1:1 even though
// it self-collides with the enum name; mirroring the source API
// keeps the boundary mapping unambiguous.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Full merge-readiness gate, distinct from the tree-conflict bit
/// above. The Unknown variant doubles as the catchall for unknown
/// future states — forward-compat decode never aborts the observe
/// pass on a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MergeStateStatus {
    Behind,
    Blocked,
    Clean,
    Dirty,
    Draft,
    HasHooks,
    Unstable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum ReviewDecision {
    Approved,
    ReviewRequired,
    ChangesRequested,
}

fn deserialize_review_decision<'de, D>(d: D) -> Result<Option<ReviewDecision>, D::Error>
where
    D: Deserializer<'de>,
{
    // Absence shapes (null, empty string) both decode to "no
    // decision needed" — the branch has no review policy. Unknown
    // string values are decode errors so unmodeled future states
    // surface explicitly.
    let raw: Option<String> = Option::deserialize(d)?;
    Ok(match raw.as_deref() {
        None | Some("") => None,
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "unknown reviewDecision: {other}"
            )));
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Label {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Assignee {
    pub login: GitHubLogin,
}

/// Either a user (has `login`) or a team (has `name`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewRequest {
    #[serde(default)]
    pub login: Option<GitHubLogin>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Commit {
    pub oid: GitCommitSha,
    /// Commit time, distinct from author time (which can predate it
    /// on rebased or cherry-picked commits). Anchors HEAD-at-time
    /// queries used by health axes.
    pub committed_date: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERGED_FIXTURE: &str = include_str!("../../../test/fixtures/github/pr_view_merged.json");

    #[test]
    fn deserializes_merged_pull_request_fixture() {
        let view: PullRequestView = serde_json::from_str(MERGED_FIXTURE).unwrap();
        assert_eq!(view.number.get(), 1563);
        assert_eq!(view.title, "Fix usize hashing in runner selection");
        assert_eq!(
            view.state,
            PullRequestState::Terminal(ooda_core::TerminalState::Merged)
        );
        assert!(!view.is_draft);
        assert_eq!(view.mergeable, Mergeable::Unknown);
        assert_eq!(view.merge_state_status, MergeStateStatus::Unknown);
        assert_eq!(view.base_ref_name.as_str(), "master");
        assert_eq!(view.review_decision, Some(ReviewDecision::Approved));
        assert!(view.merged_at.is_some());
        assert!(view.closed_at.is_some());
        assert_eq!(view.labels.len(), 2);
        assert_eq!(view.labels[0].name, "bug");
        assert_eq!(view.assignees.len(), 1);
        assert_eq!(view.assignees[0].login.as_str(), "corygabrielsen");
        assert_eq!(view.review_requests.len(), 0);
        assert_eq!(view.commits.len(), 1);
        assert_eq!(
            view.commits[0].oid.as_str(),
            "01845f2415c87c611417a3500ddfb9facd01fb42"
        );
    }

    #[test]
    fn review_decision_null_maps_to_none() {
        let json = modify_fixture("\"APPROVED\"", "null");
        let view: PullRequestView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.review_decision, None);
    }

    #[test]
    fn review_decision_empty_string_maps_to_none() {
        let json = modify_fixture("\"APPROVED\"", "\"\"");
        let view: PullRequestView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.review_decision, None);
    }

    #[test]
    fn review_decision_unknown_string_errors() {
        let json = modify_fixture("\"APPROVED\"", "\"WHO_KNOWS\"");
        let err = serde_json::from_str::<PullRequestView>(&json).unwrap_err();
        assert!(err.to_string().contains("unknown reviewDecision"));
    }

    #[test]
    fn changes_requested_variant_parses() {
        let json = modify_fixture("\"APPROVED\"", "\"CHANGES_REQUESTED\"");
        let view: PullRequestView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.review_decision, Some(ReviewDecision::ChangesRequested));
    }

    #[test]
    fn extra_fields_ignored() {
        // Commit has authoredDate / authors / messageBody / messageHeadline
        // in the real output; we only model `oid`. Deserialization must
        // tolerate the extras without failing.
        let view: PullRequestView = serde_json::from_str(MERGED_FIXTURE).unwrap();
        assert_eq!(view.commits.len(), 1);
    }

    #[test]
    fn missing_optional_timestamps_deserialize_as_none() {
        // Strip `closedAt` and `mergedAt` to simulate an open PR.
        let json = MERGED_FIXTURE
            .replace(
                "\"closedAt\": \"2026-03-30T16:17:24Z\",\n    ",
                "\"closedAt\": null,\n    ",
            )
            .replace(
                "\"mergedAt\": \"2026-03-30T16:17:24Z\",\n    ",
                "\"mergedAt\": null,\n    ",
            );
        let view: PullRequestView = serde_json::from_str(&json).unwrap();
        assert_eq!(view.closed_at, None);
        assert_eq!(view.merged_at, None);
    }

    /// Replace the reviewDecision value in the merged fixture.
    fn modify_fixture(from: &str, to: &str) -> String {
        let key = "\"reviewDecision\": ";
        let mut out = String::from(MERGED_FIXTURE);
        let start = out.find(key).expect("fixture has reviewDecision") + key.len();
        let end = start + from.len();
        assert_eq!(&out[start..end], from, "fixture value changed");
        out.replace_range(start..end, to);
        out
    }
}
