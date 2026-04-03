/**
 * Raw GitHub API response types.
 *
 * These model the exact shapes returned by `gh` CLI JSON output.
 * Only fields we actually use are typed — the rest is ignored.
 */

/** gh pr view --json ... */
export interface GhPrView {
  title: string;
  number: number;
  url: string;
  body: string | null;
  state: "OPEN" | "MERGED" | "CLOSED";
  isDraft: boolean;
  mergeable: "MERGEABLE" | "CONFLICTING" | "UNKNOWN";
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
export interface GhCheck {
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

/** GraphQL reviewThreads response */
export interface GhReviewThreadsResponse {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: {
          nodes: readonly {
            isResolved: boolean;
            comments: {
              nodes: readonly { author: { login: string } }[];
            };
          }[];
        };
      };
    };
  };
}

/** gh api repos/.../issues/.../comments */
export interface GhIssueComment {
  id: number;
  login: string;
}

/** gh api repos/.../pulls/.../reviews */
export interface GhReview {
  state: string;
  commit_id: string;
}
