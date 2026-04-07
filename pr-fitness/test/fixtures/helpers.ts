import type {
  GhCheck,
  GhIssueComment,
  GhPrView,
  GhReview,
  GhReviewThreadsResponse,
} from "../../src/types/input.js";
import type {
  CiSummary,
  PrState,
  ReviewSummary,
} from "../../src/types/output.js";

const HEAD = "abc12345abc12345abc12345abc12345abc12345";

// ── Raw API fixtures ────────────────────────────────────────────

export function makePr(overrides: Partial<GhPrView> = {}): GhPrView {
  return {
    title: "Fix a bug",
    number: 100,
    url: "https://github.com/example/widgets/pull/100",
    body: "## Summary\n\nFixed it.\n\n## Test plan\n\n- [x] tests pass",
    state: "OPEN",
    isDraft: false,
    mergeable: "MERGEABLE",
    headRefOid: HEAD,
    baseRefName: "master",
    updatedAt: "2026-03-30T08:00:00Z",
    closedAt: null,
    mergedAt: null,
    reviewDecision: "APPROVED",
    labels: [{ name: "bug" }],
    assignees: [{ login: "cory" }],
    reviewRequests: [{ name: "review-team" }],
    commits: [{ oid: "abc" }],
    ...overrides,
  };
}

export function makeCheck(name: string, state: GhCheck["state"]): GhCheck {
  return { name, state, description: "", link: "", completedAt: "" };
}

export function makeThreads(
  resolved: boolean[],
  reviewRequests: GhReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewRequests"]["nodes"] = [],
): GhReviewThreadsResponse {
  return {
    data: {
      repository: {
        pullRequest: {
          reviewThreads: {
            nodes: resolved.map((isResolved) => ({
              isResolved,
              comments: { nodes: [{ author: { login: "user" } }] },
            })),
          },
          reviewRequests: { nodes: reviewRequests },
        },
      },
    },
  };
}

export function makeComment(id: number, login: string): GhIssueComment {
  return { id, login };
}

export function makeReview(
  state: string,
  commit_id: string = HEAD,
  user: string = "someone",
): GhReview {
  return { user, state, commit_id, submitted_at: "2026-03-30T08:00:00Z" };
}

export { HEAD };

// ── Computed fixtures ───────────────────────────────────────────

export const CLEAN_CI: CiSummary = {
  pass: 10,
  fail: 0,
  pending: 0,
  total: 10,
  failed: [],
  pending_names: [],
  failed_details: [],
  completed_at: null,
};

export const APPROVED_REVIEWS: ReviewSummary = {
  decision: "APPROVED",
  threads_unresolved: 0,
  threads_total: 3,
  bot_comments: 1,
  approvals_on_head: 1,
  approvals_stale: 0,
  pending_reviews: { bots: [], humans: [] },
  bot_reviews: [],
};

export const CLEAN_STATE: PrState = {
  conflict: "MERGEABLE",
  draft: false,
  wip: false,
  title_len: 40,
  title_ok: true,
  body: true,
  summary: true,
  test_plan: true,
  content_label: true,
  assignees: 1,
  reviewers: 1,
  merge_when_ready: true,
  commits: 1,
  updated_at: "2026-03-30T08:00:00Z",
  last_commit_at: null,
};
