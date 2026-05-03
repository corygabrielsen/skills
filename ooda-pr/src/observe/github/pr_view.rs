//! Typed view of `gh pr view --json ...` output.
//!
//! Mirrors the GraphQL shape that the `gh` CLI emits for a pull
//! request. Extra fields in the JSON are ignored; we model only what
//! later stages (orient/decide) consume.

use serde::{Deserialize, Deserializer};

use crate::ids::{BranchName, GitCommitSha, GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{gh_json, GhError};

/// Fields passed via `--json` to `gh pr view`. Must stay aligned with
/// the `PullRequestView` struct below.
const PR_FIELDS: &str = "title,number,url,body,state,isDraft,mergeable,\
     mergeStateStatus,headRefOid,baseRefName,updatedAt,closedAt,mergedAt,\
     labels,assignees,reviewRequests,reviewDecision,commits";

/// Fetch PR metadata via `gh pr view`.
pub fn fetch_pr_view(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<PullRequestView, GhError> {
    let slug_s = slug.to_string();
    let pr_s = pr.to_string();
    gh_json(&[
        "pr", "view", &pr_s, "-R", &slug_s, "--json", PR_FIELDS,
    ])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestView {
    pub title: String,
    pub number: PullRequestNumber,
    pub url: String,
    pub body: Option<String>,
    pub state: PrState,
    pub is_draft: bool,
    pub mergeable: Mergeable,
    pub merge_state_status: MergeStateStatus,
    pub head_ref_oid: GitCommitSha,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

// GraphQL's `Mergeable` enum has a variant literally named
// `MERGEABLE`; mirroring that preserves 1:1 alignment with the
// source API at the cost of `Mergeable::Mergeable` tripping clippy.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

/// GitHub's full merge-readiness gate. Distinct from [`Mergeable`],
/// which only reports tree-level conflicts.
///
/// `Unknown` is both GitHub's documented "still computing" state
/// AND our `#[serde(other)]` catchall — any future status variant
/// GitHub adds routes here too rather than aborting deserialization.
/// Same pattern as `CheckState::Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeStateStatus {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ReviewRequired,
    ChangesRequested,
}

fn deserialize_review_decision<'de, D>(d: D) -> Result<Option<ReviewDecision>, D::Error>
where
    D: Deserializer<'de>,
{
    // GitHub returns one of: "APPROVED" | "REVIEW_REQUIRED" |
    // "CHANGES_REQUESTED" | "" | null. Empty string and null both
    // mean "no decision needed" (e.g., branch has no review policy).
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Assignee {
    pub login: GitHubLogin,
}

/// Either a user (has `login`) or a team (has `name`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewRequest {
    #[serde(default)]
    pub login: Option<GitHubLogin>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Commit {
    pub oid: GitCommitSha,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERGED_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/pr_view_merged.json");

    #[test]
    fn deserializes_merged_pr_fixture() {
        let view: PullRequestView = serde_json::from_str(MERGED_FIXTURE).unwrap();
        assert_eq!(view.number.get(), 1563);
        assert_eq!(view.title, "Fix usize hashing in runner selection");
        assert_eq!(view.state, PrState::Merged);
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
