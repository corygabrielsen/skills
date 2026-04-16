import type {
  CiSummary,
  GraphiteStatus,
  PullRequestState,
  ReviewSummary,
} from "../types/output.js";
import type { Action, TargetEffect } from "../types/action.js";
import type { CopilotReport } from "../types/copilot.js";
import { PositiveSeconds } from "../types/branded.js";

/**
 * Attention-phase urgency for "waiting on async axis" actions.
 *
 * Axes that produce signals over time (CI, bot reviews, human reviews,
 * individual reviewer bots like Copilot/Cursor/Claude) share a shape:
 * pre-first-signal waits demand attention; post-positive waits are
 * background; post-negative waits are active blockers again.
 *
 *   hasReported=false                       → blocks  (need first data)
 *   hasReported=true && currentlyFailing    → blocks  (active blocker)
 *   hasReported=true && !currentlyFailing   → neutral (background)
 *
 * This is the generic rule. When additional reviewer lanes (Cursor,
 * Claude, Codex-local, …) land, each becomes a caller of this helper
 * with its own `hasReported`/`currentlyFailing` derivation — no change
 * to /converge or the fitness contract required. The lane pattern is
 * inlined here until a second fitness skill or a dashboard consumer
 * forces it into a report-level type.
 */
function pendingAxisEffect(
  hasReported: boolean,
  currentlyFailing: boolean,
): TargetEffect {
  if (!hasReported) return "blocks";
  if (currentlyFailing) return "blocks";
  return "neutral";
}

/**
 * Derive an action plan from fitness dimensions.
 *
 * Pure function: takes observation, returns prescription.
 * Actions are ordered by priority — fix CI first (a review fix
 * that breaks CI wastes a cycle), then reviews, then metadata.
 *
 * `repo` and `pr` are used to materialize `execute` argv for full
 * actions (gh / gt invocations).
 */
export function plan(
  ci: CiSummary,
  reviews: ReviewSummary,
  state: PullRequestState,
  graphite: GraphiteStatus,
  copilot: CopilotReport,
  repo: string,
  pr: number,
): readonly Action[] {
  const actions: Action[] = [];
  const prArg = String(pr);

  // ── Priority 0: Stack ordering ─────────────────────────────────
  // Graphite's mergeability check is pending until downstack merges.
  // Nothing to do — it resolves automatically.

  if (graphite === "pending") {
    pushAction(actions, {
      blocker: "stack_blocked",
      description: "Waiting for downstack PR to merge",
      automation: "wait",
      target_effect: "blocks",
      type: { kind: "wait_for_ci", pending: ["downstack merge"] },
      next_poll_seconds: PositiveSeconds(60),
    });
  }

  // ── Priority 1: CI ─────────────────────────────────────────────
  // Fix CI before anything else. A review fix that breaks CI wastes
  // a loop iteration.

  if (ci.pending > 0) {
    // Pre-first-signal CI is attention-grabbing (need to know if it will
    // pass). Post-green pending CI is background — an incremental re-run
    // after a push; trust the prior green until it flips to red.
    pushAction(actions, {
      blocker: `ci_pending: ${ci.pending_names.join(", ")}`,
      description: `Wait for ${String(ci.pending)} pending check(s)`,
      automation: "wait",
      target_effect: pendingAxisEffect(ci.completed_at !== null, ci.fail > 0),
      type: { kind: "wait_for_ci", pending: ci.pending_names },
      next_poll_seconds: PositiveSeconds(60),
    });
  }

  for (const name of ci.failed) {
    pushAction(actions, {
      blocker: `ci_fail: ${name}`,
      description: `Fix failing check: ${name}`,
      automation: "llm",
      target_effect: "blocks",
      type: { kind: "fix_ci", check_name: name },
    });
  }

  // ── Priority 2: Merge blockers (mechanical) ────────────────────

  if (state.conflict === "CONFLICTING") {
    // No known fixed argv for rebase — could be `gt rebase` or
    // `git rebase origin/master`. Fall back to LLM so the caller
    // picks the right tool for the repo.
    pushAction(actions, {
      blocker: "merge_conflict",
      description: "Rebase to resolve merge conflicts",
      automation: "llm",
      target_effect: "blocks",
      type: { kind: "rebase" },
    });
  }

  if (state.draft) {
    pushAction(actions, {
      blocker: "draft",
      description: "Mark PR as ready for review",
      automation: "full",
      target_effect: "blocks",
      type: { kind: "mark_ready" },
      execute: ["gh", "pr", "ready", prArg, "-R", repo],
    });
  }

  if (state.wip) {
    pushAction(actions, {
      blocker: "wip_label",
      description: 'Remove "work in progress" label',
      automation: "full",
      target_effect: "blocks",
      type: { kind: "remove_wip_label" },
      execute: ["gh", "pr", "edit", prArg, "-R", repo, "--remove-label", "wip"],
    });
  }

  if (!state.title_ok) {
    pushAction(actions, {
      blocker: "title_too_long",
      description: `Shorten title (${String(state.title_len)} chars, max 50)`,
      automation: "llm",
      target_effect: "blocks",
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
    pushAction(actions, {
      blocker: `${String(reviews.threads_unresolved)}_unresolved_threads`,
      description: `Address ${String(reviews.threads_unresolved)} unresolved review thread(s)`,
      automation: "llm",
      target_effect: "blocks",
      type: {
        kind: "address_threads",
        count: reviews.threads_unresolved,
      },
    });
  }

  if (copilot.configured) {
    switch (copilot.activity.state) {
      case "requested":
        pushAction(actions, {
          blocker: "copilot_not_acked",
          description: "Waiting for Copilot to start reviewing",
          automation: "wait",
          target_effect: "blocks",
          type: { kind: "wait_for_copilot_ack" },
          next_poll_seconds: PositiveSeconds(15),
        });
        break;
      case "working":
        pushAction(actions, {
          blocker: "copilot_reviewing",
          description: "Waiting for Copilot to finish reviewing",
          automation: "wait",
          target_effect: "blocks",
          type: { kind: "wait_for_copilot_review" },
          next_poll_seconds: PositiveSeconds(60),
        });
        break;
      case "reviewed":
        // If tier isn't platinum and PR is otherwise clean, suggest
        // either addressing suppressed findings or re-requesting.
        if (copilot.tier !== "platinum" && copilot.threads.unresolved === 0) {
          const latestRound = copilot.activity.latest;
          if (copilot.tier === "silver") {
            pushAction(actions, {
              blocker: `copilot_tier_${copilot.tier}`,
              description: `Address ${String(latestRound.commentsSuppressed)} Copilot low-confidence finding(s) to reach platinum`,
              automation: "llm",
              target_effect: "advances",
              type: {
                kind: "address_copilot_suppressed",
                count: latestRound.commentsSuppressed,
              },
            });
          } else {
            pushAction(actions, {
              blocker: `copilot_tier_${copilot.tier}`,
              description:
                "Re-request Copilot review on HEAD to reach platinum",
              automation: "full",
              target_effect: "advances",
              type: { kind: "rerequest_copilot" },
              execute: [
                "gh",
                "api",
                `repos/${repo}/pulls/${prArg}/requested_reviewers`,
                "--method",
                "POST",
                "-f",
                "reviewers[]=copilot-pull-request-reviewer[bot]",
              ],
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
    pushAction(actions, {
      blocker: `pending_bot_review: ${reviews.pending_reviews.bots.join(", ")}`,
      description: `Wait for bot review from ${reviews.pending_reviews.bots.join(", ")}`,
      automation: "wait",
      target_effect: "blocks",
      type: {
        kind: "wait_for_review",
        reviewers: reviews.pending_reviews.bots,
      },
      next_poll_seconds: PositiveSeconds(60),
    });
  }

  if (reviews.pending_reviews.humans.length > 0) {
    pushAction(actions, {
      blocker: `pending_human_review: ${reviews.pending_reviews.humans.join(", ")}`,
      description: `Waiting on human review from ${reviews.pending_reviews.humans.join(", ")}`,
      automation: "human",
      target_effect: "blocks",
      type: {
        kind: "wait_for_review",
        reviewers: reviews.pending_reviews.humans,
      },
      next_poll_seconds: PositiveSeconds(300),
    });
  }

  if (reviews.decision !== "APPROVED" && reviews.decision !== "NONE") {
    // If there are no unresolved threads and CI is green, the main
    // blocker is just getting someone to click approve.
    const ciClean = ci.fail === 0 && ci.pending === 0;
    const threadsClean = reviews.threads_unresolved === 0;

    if (ciClean && threadsClean) {
      pushAction(actions, {
        blocker: "not_approved",
        description: "Request or self-approve",
        automation: "human",
        target_effect: "blocks",
        type: { kind: "request_approval" },
      });
    }
  }

  // ── Priority 4: Metadata (non-blocking but good hygiene) ──────

  if (!state.content_label) {
    // No known fixed argv — which label (bug vs enhancement) needs
    // judgment — fall back to LLM.
    pushAction(actions, {
      blocker: "missing_content_label",
      description: "Add bug or enhancement label",
      automation: "llm",
      target_effect: "neutral",
      type: { kind: "add_content_label" },
    });
  }

  if (state.assignees === 0) {
    pushAction(actions, {
      blocker: "no_assignee",
      description: "Add assignee",
      automation: "full",
      target_effect: "neutral",
      type: { kind: "add_assignee" },
      execute: ["gh", "pr", "edit", prArg, "-R", repo, "--add-assignee", "@me"],
    });
  }

  if (
    state.reviewers === 0 &&
    reviews.decision !== "APPROVED" &&
    reviews.decision !== "NONE"
  ) {
    // Reviewer identity is caller-specific — fall back to LLM.
    pushAction(actions, {
      blocker: "no_reviewer",
      description: "Request reviewers",
      automation: "llm",
      target_effect: "neutral",
      type: { kind: "add_reviewer" },
    });
  }

  if (!state.body) {
    pushAction(actions, {
      blocker: "no_description",
      description: "Add PR description",
      automation: "llm",
      target_effect: "neutral",
      type: { kind: "add_description" },
    });
  }

  return actions;
}

/**
 * Append an action, synthesizing the top-level `kind` from `type.kind`.
 * /converge's generic Action contract requires `kind` at the top level
 * alongside the rich discriminated `type` payload.
 */
function pushAction(actions: Action[], partial: Omit<Action, "kind">): void {
  actions.push({ ...partial, kind: partial.type.kind });
}
