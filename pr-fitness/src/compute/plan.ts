import type {
  CiSummary,
  PullRequestState,
  ReviewSummary,
} from "../types/output.js";
import type { Action, TargetEffect } from "../types/action.js";
import type { CopilotReport } from "../types/copilot.js";
import type { CursorReport } from "../types/cursor.js";
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
  copilot: CopilotReport,
  cursor: CursorReport,
  repo: string,
  pr: number,
): readonly Action[] {
  const actions: Action[] = [];
  const prArg = String(pr);

  // ── Priority 1: CI ─────────────────────────────────────────────
  // Fix CI before anything else. A review fix that breaks CI wastes
  // a loop iteration.

  // Compute triage signals: if CI would cause a wait but other
  // signals suggest potentially-actionable work in parallel, hand
  // off to the agent instead of sleeping.
  const ambiguityTriggered =
    ci.fail === 0 &&
    (ci.pending > 0 || ci.missing > 0) &&
    (triageSignal_inProgressBotReview(cursor) ||
      triageSignal_advisoryFailure(ci) ||
      triageSignal_copilotAdvance(copilot));

  if (ambiguityTriggered) {
    const blockedChecks = [...ci.pending_names, ...ci.missing_names];
    pushAction(actions, {
      blocker: `ci_triage: ${blockedChecks.join(", ")}`,
      description: triageDescription(blockedChecks, ci, cursor, copilot),
      automation: "agent",
      target_effect: "blocks",
      type: { kind: "triage_wait", blocked_checks: blockedChecks },
    });
  } else {
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

    if (ci.missing > 0) {
      pushAction(actions, {
        blocker: `ci_missing: ${ci.missing_names.join(", ")}`,
        description: `${String(ci.missing)} required check(s) not started: ${ci.missing_names.join(", ")}`,
        automation: "wait",
        target_effect: "blocks",
        type: { kind: "wait_for_ci", pending: ci.missing_names },
        next_poll_seconds: PositiveSeconds(60),
      });
    }
  }

  for (const name of ci.failed) {
    pushAction(actions, {
      blocker: `ci_fail: ${name}`,
      description: `Fix failing check: ${name}`,
      automation: "agent",
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
      automation: "agent",
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
      automation: "agent",
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
      description: addressThreadsDescription(
        reviews.threads_unresolved,
        copilot,
        cursor,
      ),
      automation: "agent",
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
        if (copilot.tier !== "platinum" && copilot.threads.unresolved === 0) {
          const latestRound = copilot.activity.latest;
          const staleCount = copilot.threads.stale;
          const notAtHead = !copilot.fresh;

          // Rerequest when Copilot hasn't seen recent activity — a fresh
          // review may resolve stale replies AND reset the suppressed
          // count in one pass. Low-confidence findings aren't directly
          // "addressable"; the only real action on them is to push code
          // (which triggers a new review anyway). So rerequest dominates
          // address_suppressed whenever stale>0 or !fresh, regardless of
          // tier. Address_suppressed is only the primary action when
          // suppressed findings are the SOLE barrier.
          if (staleCount > 0 || notAtHead) {
            const description =
              staleCount > 0
                ? `Re-request Copilot so it reads ${String(staleCount)} post-review reply/replies — may also clear low-confidence findings`
                : `Re-request Copilot review on HEAD to reach platinum`;
            pushAction(actions, {
              blocker: `copilot_tier_${copilot.tier}`,
              description,
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
          } else if (copilot.tier === "silver") {
            pushAction(actions, {
              blocker: `copilot_tier_${copilot.tier}`,
              description: `Copilot flagged ${String(latestRound.commentsSuppressed)} low-confidence finding(s). Investigate and push fixes for any that are real — the next review may clear them.`,
              automation: "agent",
              target_effect: "advances",
              type: {
                kind: "address_copilot_suppressed",
                count: latestRound.commentsSuppressed,
              },
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
      automation: "agent",
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
      automation: "agent",
      target_effect: "neutral",
      type: { kind: "add_reviewer" },
    });
  }

  if (!state.body) {
    pushAction(actions, {
      blocker: "no_description",
      description: "Add PR description",
      automation: "agent",
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

// ──────────────────────────────────────────────────────────────────
// Triage signals — conditions under which a CI wait should be
// upgraded to an agent handoff because other signals suggest
// potentially-actionable work runs in parallel.
// ──────────────────────────────────────────────────────────────────

function triageSignal_inProgressBotReview(cursor: CursorReport): boolean {
  return cursor.configured && cursor.activity.state === "reviewing";
}

function triageSignal_advisoryFailure(ci: CiSummary): boolean {
  return ci.advisory.failed.length > 0;
}

function triageSignal_copilotAdvance(copilot: CopilotReport): boolean {
  return (
    copilot.configured &&
    copilot.activity.state === "reviewed" &&
    copilot.threads.unresolved === 0 &&
    copilot.tier !== "platinum"
  );
}

function triageDescription(
  blockedChecks: readonly string[],
  ci: CiSummary,
  cursor: CursorReport,
  copilot: CopilotReport,
): string {
  const quoted = blockedChecks.map((n) => `"${n}"`).join(", ");
  const lines: string[] = [`CI waiting on ${quoted}. Concurrent state:`];

  if (triageSignal_inProgressBotReview(cursor)) {
    lines.push("- Cursor check in progress");
  }
  for (const name of ci.advisory.failed) {
    lines.push(`- Advisory "${name}" failed`);
  }
  if (
    copilot.configured &&
    copilot.activity.state === "reviewed" &&
    copilot.threads.unresolved === 0 &&
    copilot.tier !== "platinum"
  ) {
    const detail: string[] = [];
    if (copilot.threads.stale > 0) {
      detail.push(
        `${String(copilot.threads.stale)} stale repl${copilot.threads.stale === 1 ? "y" : "ies"}`,
      );
    } else if (!copilot.fresh) {
      detail.push("not at HEAD");
    }
    const suppressed = copilot.activity.latest.commentsSuppressed;
    if (suppressed > 0) {
      detail.push(
        `${String(suppressed)} low-confidence finding${suppressed === 1 ? "" : "s"}`,
      );
    }
    const tail = detail.length > 0 ? ` · ${detail.join(", ")}` : "";
    lines.push(`- Copilot ${copilot.tier}${tail}`);
  }

  return lines.join("\n");
}

/**
 * Build the address_threads description with per-bot context when
 * available. Symmetric shape: "<Bot>: <facts>." then the generalization
 * directive.
 */
function addressThreadsDescription(
  total: number,
  copilot: CopilotReport,
  cursor: CursorReport,
): string {
  const parts: string[] = [`Address ${String(total)} unresolved review thread(s).`];

  if (cursor.configured && cursor.threads.unresolved > 0) {
    const { high, medium, low } = cursor.severity;
    const sev: string[] = [];
    if (high > 0) sev.push(`${String(high)} high`);
    if (medium > 0) sev.push(`${String(medium)} medium`);
    if (low > 0) sev.push(`${String(low)} low`);
    if (sev.length > 0) parts.push(`Cursor: ${sev.join(", ")}.`);
  }

  if (copilot.configured && copilot.activity.state === "reviewed") {
    const issues = copilot.threads.unresolved;
    const suppressed = copilot.activity.latest.commentsSuppressed;
    const bits: string[] = [];
    if (issues > 0) bits.push(`${String(issues)} issue${issues === 1 ? "" : "s"}`);
    if (suppressed > 0) {
      bits.push(
        `${String(suppressed)} low-confidence finding${suppressed === 1 ? "" : "s"}`,
      );
    }
    if (bits.length > 0) parts.push(`Copilot: ${bits.join(", ")}.`);
  }

  parts.push(
    "For each issue, think deeply about the entire class of issue, in general, and solve the general form of the issue across all relevant code. This ensures the entire category of each issue is solved in general.",
  );

  return parts.join(" ");
}
