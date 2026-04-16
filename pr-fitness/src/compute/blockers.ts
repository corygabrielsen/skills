import type {
  CiSummary,
  GraphiteStatus,
  PullRequestState,
  ReviewSummary,
} from "../types/index.js";

/** Derive the list of hard blockers preventing merge. */
export function computeBlockers(
  ci: CiSummary,
  reviews: ReviewSummary,
  state: PullRequestState,
  graphite: GraphiteStatus,
): readonly string[] {
  const blockers: string[] = [];

  if (ci.fail > 0) {
    blockers.push(`ci_fail: ${ci.failed.join(", ")}`);
  }
  if (ci.pending > 0) {
    blockers.push(`ci_pending: ${ci.pending_names.join(", ")}`);
  }
  if (graphite === "pending") {
    blockers.push("stack_blocked");
  }
  if (reviews.decision !== "APPROVED" && reviews.decision !== "NONE") {
    blockers.push("not_approved");
  }
  if (reviews.threads_unresolved > 0) {
    blockers.push(`${String(reviews.threads_unresolved)}_unresolved_threads`);
  }
  if (reviews.pending_reviews.bots.length > 0) {
    blockers.push(
      `pending_bot_review: ${reviews.pending_reviews.bots.join(", ")}`,
    );
  }
  if (reviews.pending_reviews.humans.length > 0) {
    blockers.push(
      `pending_human_review: ${reviews.pending_reviews.humans.join(", ")}`,
    );
  }
  if (state.conflict === "CONFLICTING") {
    blockers.push("merge_conflict");
  }
  if (state.draft) {
    blockers.push("draft");
  }
  if (state.wip) {
    blockers.push("wip_label");
  }
  if (!state.title_ok) {
    blockers.push("title_too_long");
  }

  return blockers;
}
