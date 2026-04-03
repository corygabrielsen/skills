import type {
  GhIssueComment,
  GhPrView,
  GhReview,
  GhReviewThreadsResponse,
  ReviewSummary,
} from "../types/index.js";

export function computeReviews(
  pr: GhPrView,
  threads: GhReviewThreadsResponse,
  comments: readonly GhIssueComment[],
  reviews: readonly GhReview[],
): ReviewSummary {
  const nodes = threads.data.repository.pullRequest.reviewThreads.nodes;
  const threadsTotal = nodes.length;
  const threadsUnresolved = nodes.filter((n) => !n.isResolved).length;

  const botComments = comments.filter((c) => c.login.endsWith("[bot]")).length;

  const headSha = pr.headRefOid;
  const approvalsOnHead = reviews.filter(
    (r) => r.state === "APPROVED" && r.commit_id === headSha,
  ).length;
  const approvalsTotal = reviews.filter((r) => r.state === "APPROVED").length;

  // GitHub returns null/empty when no review policy is configured.
  // That's distinct from REVIEW_REQUIRED (policy exists, not yet met).
  const decision = pr.reviewDecision || "NONE";

  const bots: string[] = [];
  const humans: string[] = [];
  for (const r of pr.reviewRequests) {
    const name = r.login ?? r.name ?? "unknown";
    if (name.endsWith("[bot]") || name === "Copilot") {
      bots.push(name);
    } else {
      humans.push(name);
    }
  }

  return {
    decision,
    threads_unresolved: threadsUnresolved,
    threads_total: threadsTotal,
    bot_comments: botComments,
    approvals_on_head: approvalsOnHead,
    approvals_stale: approvalsTotal - approvalsOnHead,
    pending_reviews: { bots, humans },
  };
}
