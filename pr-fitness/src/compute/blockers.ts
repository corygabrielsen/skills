import type {
  CiSummary,
  GraphiteStatus,
  PullRequestState,
  ReviewSummary,
} from "../types/index.js";

/**
 * Blockers split by resolution authority.
 *
 * Three categories form a lattice by "who resolves":
 *
 *   Agent      — automated system can fix (CI, threads, draft).
 *                Caps fitness score.
 *   Human      — human action needed (approve, review).
 *                Drives hil halt, doesn't cap score.
 *   Structural — resolves when external conditions change (downstack
 *                merges). Drives halt with re-entry hint, doesn't
 *                cap score.
 *
 * `all` is the union for display and merge-readiness checks.
 */
export interface Blockers {
  readonly agent: readonly string[];
  readonly human: readonly string[];
  readonly structural: readonly string[];
  readonly all: readonly string[];
}

export const EMPTY_BLOCKERS: Blockers = {
  agent: [],
  human: [],
  structural: [],
  all: [],
};

export function computeBlockers(
  ci: CiSummary,
  reviews: ReviewSummary,
  state: PullRequestState,
  graphite: GraphiteStatus,
): Blockers {
  const agent: string[] = [];
  const human: string[] = [];
  const structural: string[] = [];

  // Agent-resolvable — the agent can fix these or wait for them.
  if (ci.fail > 0) {
    agent.push(`ci_fail: ${ci.failed.join(", ")}`);
  }
  if (ci.missing > 0) {
    agent.push(`ci_missing: ${ci.missing_names.join(", ")}`);
  }
  if (ci.pending > 0) {
    agent.push(`ci_pending: ${ci.pending_names.join(", ")}`);
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

  // Structural — resolves when external conditions change. Not actionable.
  if (graphite === "pending") {
    structural.push("stack_blocked");
  }

  return { agent, human, structural, all: [...agent, ...human, ...structural] };
}
