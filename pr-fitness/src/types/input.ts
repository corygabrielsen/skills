/**
 * Raw GitHub API response types.
 *
 * These model the exact shapes returned by `gh` CLI JSON output.
 * Only fields we actually use are typed — the rest is ignored.
 */

import type { GitCommitSha, GitHubLogin, Timestamp } from "./branded.js";

/** gh pr view --json ... */
/**
 * GitHub's composite merge-readiness state. See GitHub's docs for the
 * full state machine; relevant values here:
 *   CLEAN      — mergeable, all gates pass
 *   UNSTABLE   — mergeable but non-required checks failing
 *   BLOCKED    — blocked by required reviews / checks / resolution
 *   BEHIND     — base has advanced past the merge base (needs rebase)
 *   DIRTY      — tree-level merge conflict
 *   DRAFT      — PR is a draft
 *   HAS_HOOKS  — pre-merge hooks configured
 *   UNKNOWN    — GitHub hasn't finished computing yet
 */
export type MergeStateStatus =
  | "CLEAN"
  | "UNSTABLE"
  | "BLOCKED"
  | "BEHIND"
  | "DIRTY"
  | "DRAFT"
  | "HAS_HOOKS"
  | "UNKNOWN";

export interface GitHubPullRequestView {
  title: string;
  number: number;
  url: string;
  body: string | null;
  state: "OPEN" | "MERGED" | "CLOSED";
  isDraft: boolean;
  mergeable: "MERGEABLE" | "CONFLICTING" | "UNKNOWN";
  /**
   * Full merge-readiness gate from GitHub (required checks, reviews,
   * base-branch drift, pre-merge hooks). Distinct from `mergeable`,
   * which only covers tree-level conflicts.
   */
  mergeStateStatus: MergeStateStatus;
  headRefOid: string;
  baseRefName: string;
  updatedAt: string;
  closedAt: string | null;
  mergedAt: string | null;
  reviewDecision:
    | "APPROVED"
    | "REVIEW_REQUIRED"
    | "CHANGES_REQUESTED"
    | ""
    | null;
  labels: readonly { name: string }[];
  assignees: readonly { login: string }[];
  reviewRequests: readonly { login?: string; name?: string }[];
  commits: readonly { oid: string }[];
}

/** gh pr checks --json name,state,description,link,completedAt */
export interface GitHubCheck {
  name: string;
  state:
    | "SUCCESS"
    | "FAILURE"
    | "SKIPPED"
    | "NEUTRAL"
    | "IN_PROGRESS"
    | "QUEUED";
  /** Check run output title (one-liner summary). */
  description: string;
  /** URL to the check run details page. */
  link: string;
  /** ISO 8601 completion time (empty string if not completed). */
  completedAt: string;
}

/** GraphQL reviewThreads + reviewRequests response */
export interface GitHubPullRequestReviewThreadsResponse {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: {
          nodes: readonly {
            isResolved: boolean;
            comments: {
              nodes: readonly {
                author: { login: string };
                createdAt: string;
              }[];
            };
          }[];
        };
        reviewRequests: {
          nodes: readonly {
            requestedReviewer: {
              __typename: string;
              login?: string;
              name?: string;
            } | null;
          }[];
        };
      };
    };
  };
}

/** gh api repos/.../issues/.../comments */
export interface GitHubIssueComment {
  id: number;
  login: string;
}

/** gh api repos/.../pulls/.../reviews */
export interface GitHubPullRequestReview {
  readonly user: GitHubLogin;
  readonly state: GitHubPullRequestReviewState;
  readonly commit_id: GitCommitSha;
  readonly submitted_at: Timestamp;
  readonly body: string;
}

export type GitHubPullRequestReviewState =
  | "APPROVED"
  | "CHANGES_REQUESTED"
  | "COMMENTED"
  | "DISMISSED"
  | "PENDING";
