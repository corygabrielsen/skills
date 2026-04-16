import type {
  CiSummary,
  GraphiteStatus,
  PullRequestState,
  ReviewSummary,
} from "../types/output.js";
import type { Action } from "../types/action.js";
import type { CopilotReport } from "../types/copilot.js";

/**
 * Derive an action plan from fitness dimensions.
 *
 * Pure function: takes observation, returns prescription.
 * Actions are ordered by priority — fix CI first (a review fix
 * that breaks CI wastes a cycle), then reviews, then metadata.
 */
export function plan(
  ci: CiSummary,
  reviews: ReviewSummary,
  state: PullRequestState,
  graphite: GraphiteStatus,
  copilot: CopilotReport,
): readonly Action[] {
  const actions: Action[] = [];

  // ── Priority 0: Stack ordering ─────────────────────────────────
  // Graphite's mergeability check is pending until downstack merges.
  // Nothing to do — it resolves automatically.

  if (graphite === "pending") {
    actions.push({
      blocker: "stack_blocked",
      description: "Waiting for downstack PR to merge",
      automation: "wait",
      type: { kind: "wait_for_ci", pending: ["downstack merge"] },
    });
  }

  // ── Priority 1: CI ─────────────────────────────────────────────
  // Fix CI before anything else. A review fix that breaks CI wastes
  // a loop iteration.

  if (ci.pending > 0) {
    actions.push({
      blocker: `ci_pending: ${ci.pending_names.join(", ")}`,
      description: `Wait for ${String(ci.pending)} pending check(s)`,
      automation: "wait",
      type: { kind: "wait_for_ci", pending: ci.pending_names },
    });
  }

  for (const name of ci.failed) {
    actions.push({
      blocker: `ci_fail: ${name}`,
      description: `Fix failing check: ${name}`,
      automation: "llm",
      type: { kind: "fix_ci", check_name: name },
    });
  }

  // ── Priority 2: Merge blockers (mechanical) ────────────────────

  if (state.conflict === "CONFLICTING") {
    actions.push({
      blocker: "merge_conflict",
      description: "Rebase to resolve merge conflicts",
      automation: "full",
      type: { kind: "rebase" },
    });
  }

  if (state.draft) {
    actions.push({
      blocker: "draft",
      description: "Mark PR as ready for review",
      automation: "full",
      type: { kind: "mark_ready" },
    });
  }

  if (state.wip) {
    actions.push({
      blocker: "wip_label",
      description: 'Remove "work in progress" label',
      automation: "full",
      type: { kind: "remove_wip_label" },
    });
  }

  if (!state.title_ok) {
    actions.push({
      blocker: "title_too_long",
      description: `Shorten title (${String(state.title_len)} chars, max 50)`,
      automation: "llm",
      type: { kind: "shorten_title", current_len: state.title_len },
    });
  }

  // ── Priority 3: Reviews ────────────────────────────────────────
  //
  // Ordering within this section:
  //   1. Address unresolved threads (hard blocker; llm)
  //   2. Copilot mechanical moves (free wins toward platinum)
  //   3. Wait on pending reviewers (bots, humans)
  //   4. Approval request (human click)

  if (reviews.threads_unresolved > 0) {
    actions.push({
      blocker: `${String(reviews.threads_unresolved)}_unresolved_threads`,
      description: `Address ${String(reviews.threads_unresolved)} unresolved review thread(s)`,
      automation: "llm",
      type: {
        kind: "address_threads",
        count: reviews.threads_unresolved,
      },
    });
  }

  if (copilot.configured) {
    switch (copilot.activity.state) {
      case "requested":
        actions.push({
          blocker: "copilot_not_acked",
          description: "Waiting for Copilot to start reviewing",
          automation: "wait",
          type: { kind: "wait_for_copilot_ack" },
        });
        break;
      case "working":
        actions.push({
          blocker: "copilot_reviewing",
          description: "Waiting for Copilot to finish reviewing",
          automation: "wait",
          type: { kind: "wait_for_copilot_review" },
        });
        break;
      case "reviewed":
        // If tier isn't platinum and PR is otherwise clean, suggest
        // either addressing suppressed findings or re-requesting.
        if (copilot.tier !== "platinum" && copilot.threads.unresolved === 0) {
          const latestRound = copilot.activity.latest;
          if (copilot.tier === "silver") {
            actions.push({
              blocker: `copilot_tier_${copilot.tier}`,
              description: `Address ${String(latestRound.commentsSuppressed)} Copilot low-confidence finding(s) to reach platinum`,
              automation: "llm",
              type: {
                kind: "address_copilot_suppressed",
                count: latestRound.commentsSuppressed,
              },
            });
          } else {
            actions.push({
              blocker: `copilot_tier_${copilot.tier}`,
              description:
                "Re-request Copilot review on HEAD to reach platinum",
              automation: "full",
              type: { kind: "rerequest_copilot" },
            });
          }
        }
        break;
      case "idle":
      case "unconfigured":
        break;
    }
  }

  if (reviews.pending_reviews.bots.length > 0) {
    actions.push({
      blocker: `pending_bot_review: ${reviews.pending_reviews.bots.join(", ")}`,
      description: `Wait for bot review from ${reviews.pending_reviews.bots.join(", ")}`,
      automation: "wait",
      type: {
        kind: "wait_for_review",
        reviewers: reviews.pending_reviews.bots,
      },
    });
  }

  if (reviews.pending_reviews.humans.length > 0) {
    actions.push({
      blocker: `pending_human_review: ${reviews.pending_reviews.humans.join(", ")}`,
      description: `Waiting on human review from ${reviews.pending_reviews.humans.join(", ")}`,
      automation: "human",
      type: {
        kind: "wait_for_review",
        reviewers: reviews.pending_reviews.humans,
      },
    });
  }

  if (reviews.decision !== "APPROVED" && reviews.decision !== "NONE") {
    // If there are no unresolved threads and CI is green, the main
    // blocker is just getting someone to click approve.
    const ciClean = ci.fail === 0 && ci.pending === 0;
    const threadsClean = reviews.threads_unresolved === 0;

    if (ciClean && threadsClean) {
      actions.push({
        blocker: "not_approved",
        description: "Request or self-approve",
        automation: "human",
        type: { kind: "request_approval" },
      });
    }
  }

  // ── Priority 4: Metadata (non-blocking but good hygiene) ──────

  if (!state.content_label) {
    actions.push({
      blocker: "missing_content_label",
      description: "Add bug or enhancement label",
      automation: "full",
      type: { kind: "add_content_label" },
    });
  }

  if (state.assignees === 0) {
    actions.push({
      blocker: "no_assignee",
      description: "Add assignee",
      automation: "full",
      type: { kind: "add_assignee" },
    });
  }

  if (
    state.reviewers === 0 &&
    reviews.decision !== "APPROVED" &&
    reviews.decision !== "NONE"
  ) {
    actions.push({
      blocker: "no_reviewer",
      description: "Request reviewers",
      automation: "full",
      type: { kind: "add_reviewer" },
    });
  }

  if (!state.body) {
    actions.push({
      blocker: "no_description",
      description: "Add PR description",
      automation: "llm",
      type: { kind: "add_description" },
    });
  }

  return actions;
}
