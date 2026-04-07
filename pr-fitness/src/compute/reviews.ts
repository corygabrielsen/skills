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

  // Use GraphQL reviewRequests (not gh pr view) — the CLI silently
  // drops bot reviewers like Copilot.
  const requestNodes = threads.data.repository.pullRequest.reviewRequests.nodes;
  const bots: string[] = [];
  const humans: string[] = [];
  for (const r of requestNodes) {
    const reviewer = r.requestedReviewer;
    if (!reviewer) continue;
    const name = reviewer.login ?? reviewer.name ?? "unknown";
    if (reviewer.__typename === "Bot" || name.endsWith("[bot]")) {
      bots.push(name);
    } else {
      humans.push(name);
    }
  }

  const botReviews = reviews
    .filter((r) => r.user.endsWith("[bot]"))
    .map((r) => ({
      user: r.user,
      state: r.state,
      submitted_at: r.submitted_at,
    }));

  return {
    decision,
    threads_unresolved: threadsUnresolved,
    threads_total: threadsTotal,
    bot_comments: botComments,
    approvals_on_head: approvalsOnHead,
    approvals_stale: approvalsTotal - approvalsOnHead,
    pending_reviews: { bots, humans },
    bot_reviews: botReviews,
  };
}
