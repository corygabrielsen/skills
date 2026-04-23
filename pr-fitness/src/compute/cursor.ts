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
  CursorSeverityBreakdown,
  CursorThreadSummary,
  CursorTier,
} from "../types/cursor.js";
import { CURSOR_CHECK_NAME, isCursor } from "../types/cursor-identity.js";
import { formatCopilotTier } from "../types/copilot.js";
import { countBotThreads } from "./bot-threads.js";

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

export function countCursorThreads(
  threads: GitHubPullRequestReviewThreadsResponse,
  latestReviewedAt: Timestamp | null,
): CursorThreadSummary {
  return countBotThreads(threads, latestReviewedAt, isCursor);
}

const SEVERITY_RE = /\*\*(High|Medium|Low) Severity\*\*/;

/**
 * Count Cursor severity tags across unresolved cursor-authored threads.
 * Resolved threads are excluded — the breakdown is about work left.
 */
export function countCursorSeverity(
  threads: GitHubPullRequestReviewThreadsResponse,
): CursorSeverityBreakdown {
  const nodes = threads.data.repository.pullRequest.reviewThreads.nodes;
  let high = 0;
  let medium = 0;
  let low = 0;

  for (const t of nodes) {
    if (t.isResolved) continue;
    const first = t.comments.nodes[0];
    if (first === undefined) continue;
    if (!isCursor(first.author.login)) continue;
    const match = first.body.match(SEVERITY_RE);
    if (match === null) continue;
    switch (match[1]) {
      case "High":
        high++;
        break;
      case "Medium":
        medium++;
        break;
      case "Low":
        low++;
        break;
    }
  }

  return { high, medium, low };
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
 *   BRONZE:   unresolved>0 ∨ (never reviewed ∧ no check)
 *   SILVER:   unresolved=0 ∧ check in flight ∧ prior review exists
 *   GOLD:     unresolved=0 ∧ (findings at HEAD resolved ∨ reviewed at non-HEAD)
 *   PLATINUM: check at HEAD = SUCCESS (bot says clean)
 */
export function scoreCursorTier(
  rounds: readonly CursorReviewRound[],
  threads: CursorThreadSummary,
  check: GitHubCheck | null,
): CursorTier {
  if (threads.unresolved > 0) return "bronze";
  if (check?.state === "SUCCESS") return "platinum";
  if (check?.state === "QUEUED" || check?.state === "IN_PROGRESS") {
    return rounds.length > 0 ? "silver" : "bronze";
  }
  if (rounds.length === 0) return "bronze";
  return "gold";
}

function isConfigured(
  rounds: readonly CursorReviewRound[],
  check: GitHubCheck | null,
): boolean {
  return rounds.length > 0 || check !== null;
}

/** Latest review is at HEAD. Matches Copilot's `fresh` semantics. */
function isFresh(
  rounds: readonly CursorReviewRound[],
  head: GitCommitSha,
): boolean {
  return rounds.at(-1)?.commit === head;
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
  const severity = countCursorSeverity(input.threads);
  const activity = computeCursorActivity(rounds, check);
  const tier = scoreCursorTier(rounds, threads, check);
  const fresh = isFresh(rounds, input.head);

  return {
    configured: true,
    activity,
    rounds,
    threads,
    severity,
    tier,
    tier_display: formatCopilotTier(tier),
    fresh,
  };
}
