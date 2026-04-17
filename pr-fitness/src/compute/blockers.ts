import type {
  CiSummary,
  GraphiteStatus,
  PullRequestState,
  ReviewSummary,
} from "../types/index.js";

/**
 * Blockers split by who can resolve them.
 *
 * Agent-resolvable blockers cap the fitness score — the agent has work
 * to do. Human-dependent blockers drive `hil` halts but don't cap the
 * score — the PR's quality hasn't degraded, a human just hasn't acted
 * yet. `all` is the union, for display.
 */
export interface Blockers {
  readonly agent: readonly string[];
  readonly human: readonly string[];
  readonly all: readonly string[];
}

export function computeBlockers(
  ci: CiSummary,
  reviews: ReviewSummary,
  state: PullRequestState,
  graphite: GraphiteStatus,
): Blockers {
  const agent: string[] = [];
  const human: string[] = [];

  // Agent-resolvable — the agent can fix these or wait for them.
  if (ci.fail > 0) {
    agent.push(`ci_fail: ${ci.failed.join(", ")}`);
  }
  if (ci.pending > 0) {
    agent.push(`ci_pending: ${ci.pending_names.join(", ")}`);
  }
  if (graphite === "pending") {
    agent.push("stack_blocked");
  }
  if (reviews.threads_unresolved > 0) {
    agent.push(`${String(reviews.threads_unresolved)}_unresolved_threads`);
  }
  if (reviews.pending_reviews.bots.length > 0) {
    agent.push(
      `pending_bot_review: ${reviews.pending_reviews.bots.join(", ")}`,
    );
  }
  if (state.conflict === "CONFLICTING") {
    agent.push("merge_conflict");
  }
  if (state.draft) {
    agent.push("draft");
  }
  if (state.wip) {
    agent.push("wip_label");
  }
  if (!state.title_ok) {
    agent.push("title_too_long");
  }

  // Human-dependent — only a human can resolve these. The agent waits.
  if (reviews.decision !== "APPROVED" && reviews.decision !== "NONE") {
    human.push("not_approved");
  }
  if (reviews.pending_reviews.humans.length > 0) {
    human.push(
      `pending_human_review: ${reviews.pending_reviews.humans.join(", ")}`,
    );
  }

  return { agent, human, all: [...agent, ...human] };
}
