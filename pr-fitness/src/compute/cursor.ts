/**
 * Cursor compute — pure transformation from raw GitHub API data to
 * the distilled `CursorReport` domain shape.
 *
 * Contract: no I/O, no clock, no mutation of inputs.
 */

import type { GitCommitSha, Timestamp } from "../types/branded.js";
import type {
  GitHubCheck,
  GitHubPullRequestReview,
  GitHubPullRequestReviewThreadsResponse,
} from "../types/index.js";
import type {
  CursorActivity,
  CursorReport,
  CursorReviewRound,
  CursorThreadSummary,
  CursorTier,
} from "../types/cursor.js";
import { CURSOR_CHECK_NAME, isCursor } from "../types/cursor-identity.js";
import { formatCopilotTier } from "../types/copilot.js";

const FINDINGS_RE = /found (\d+) potential issue/;

export function parseCursorReviewBody(body: string): {
  readonly findingsCount: number;
} {
  return { findingsCount: Number(body.match(FINDINGS_RE)?.[1] ?? 0) };
}

export function correlateCursorRounds(
  reviews: readonly GitHubPullRequestReview[],
): readonly CursorReviewRound[] {
  const sorted = [...reviews]
    .filter((r) => isCursor(r.user))
    .sort((a, b) => (a.submitted_at < b.submitted_at ? -1 : 1));

  return sorted.map((r, i) => ({
    round: i + 1,
    reviewedAt: r.submitted_at,
    commit: r.commit_id,
    findingsCount: parseCursorReviewBody(r.body).findingsCount,
  }));
}

/** Find the Cursor Bugbot check run in the checks list (at HEAD). */
export function findCursorCheck(
  checks: readonly GitHubCheck[],
): GitHubCheck | null {
  return checks.find((c) => c.name === CURSOR_CHECK_NAME) ?? null;
}

/**
 * Summarize Cursor review threads. Same semantics as Copilot: a
 * thread is Cursor-authored iff its first comment is by Cursor. A
 * thread is `stale` iff it has any non-Cursor comment authored
 * strictly after `latestReviewedAt`.
 */
export function countCursorThreads(
  threads: GitHubPullRequestReviewThreadsResponse,
  latestReviewedAt: Timestamp | null,
): CursorThreadSummary {
  const nodes = threads.data.repository.pullRequest.reviewThreads.nodes;
  let total = 0;
  let resolved = 0;
  let unresolved = 0;
  let stale = 0;

  for (const t of nodes) {
    const first = t.comments.nodes[0];
    if (first === undefined) continue;
    if (!isCursor(first.author.login)) continue;
    total++;
    if (t.isResolved) resolved++;
    else unresolved++;

    if (latestReviewedAt !== null) {
      for (const c of t.comments.nodes) {
        if (isCursor(c.author.login)) continue;
        if (c.createdAt > latestReviewedAt) {
          stale++;
          break;
        }
      }
    }
  }

  return { total, resolved, unresolved, stale };
}

export function computeCursorActivity(
  rounds: readonly CursorReviewRound[],
  check: GitHubCheck | null,
): CursorActivity {
  if (check !== null) {
    if (check.state === "QUEUED" || check.state === "IN_PROGRESS") {
      return { state: "reviewing" };
    }
    if (check.state === "SUCCESS") {
      return { state: "clean" };
    }
    // NEUTRAL or other completed states with findings
  }

  const latest = rounds.at(-1);
  if (latest !== undefined) {
    return { state: "reviewed", latest };
  }

  return { state: "idle" };
}

/**
 * Unified tier semantics (mirrors Copilot):
 *
 *   BRONZE:   unresolved>0 ∨ (never reviewed ∧ no check at HEAD)
 *   SILVER:   unresolved=0 ∧ check at HEAD in flight ∧ prior review exists
 *   GOLD:     unresolved=0 ∧ (findings at HEAD resolved ∨ reviewed at
 *               non-HEAD ∨ check pending but first review)
 *   PLATINUM: check at HEAD = SUCCESS (bot says clean)
 */
export function scoreCursorTier(
  rounds: readonly CursorReviewRound[],
  threads: CursorThreadSummary,
  check: GitHubCheck | null,
  head: GitCommitSha,
): CursorTier {
  if (threads.unresolved > 0) return "bronze";

  if (check !== null) {
    if (check.state === "SUCCESS") return "platinum";
    if (check.state === "QUEUED" || check.state === "IN_PROGRESS") {
      // Re-reviewing. Silver if prior activity has been cleaned up.
      return rounds.length > 0 ? "silver" : "bronze";
    }
    // NEUTRAL or other — findings were posted, but unresolved===0 here,
    // so they've been resolved. Gold.
    return "gold";
  }

  // No check at HEAD.
  const latest = rounds.at(-1);
  if (latest === undefined) return "bronze"; // never reviewed
  if (latest.commit === head) {
    // Reviewed at HEAD with findings (no check visible), resolved. Gold.
    return "gold";
  }
  return "gold"; // reviewed at non-HEAD
}

function isConfigured(
  rounds: readonly CursorReviewRound[],
  check: GitHubCheck | null,
): boolean {
  return rounds.length > 0 || check !== null;
}

function isFresh(
  rounds: readonly CursorReviewRound[],
  check: GitHubCheck | null,
  head: GitCommitSha,
): boolean {
  if (check !== null) return true; // check is always at HEAD
  const latest = rounds.at(-1);
  return latest?.commit === head;
}

export function computeCursor(input: {
  readonly reviews: readonly GitHubPullRequestReview[];
  readonly threads: GitHubPullRequestReviewThreadsResponse;
  readonly checks: readonly GitHubCheck[];
  readonly head: GitCommitSha;
}): CursorReport {
  const rounds = correlateCursorRounds(input.reviews);
  const check = findCursorCheck(input.checks);

  if (!isConfigured(rounds, check)) {
    return { configured: false };
  }

  const latestReviewedAt = rounds.at(-1)?.reviewedAt ?? null;
  const threads = countCursorThreads(input.threads, latestReviewedAt);
  const activity = computeCursorActivity(rounds, check);
  const tier = scoreCursorTier(rounds, threads, check, input.head);
  const fresh = isFresh(rounds, check, input.head);

  return {
    configured: true,
    activity,
    rounds,
    threads,
    tier,
    tier_display: formatCopilotTier(tier),
    fresh,
  };
}
