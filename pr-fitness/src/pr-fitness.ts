import { collect } from "./collectors/index.js";
import { computeBlockers } from "./compute/blockers.js";
import { computeCi } from "./compute/ci.js";
import { computeCopilot } from "./compute/copilot.js";
import { plan } from "./compute/plan.js";
import { computeReviews } from "./compute/reviews.js";
import { computeState } from "./compute/state.js";
import { summarize } from "./compute/summary.js";
import { VERSION } from "./version.js";
import { GitCommitSha, Score } from "./types/branded.js";
import type {
  PullRequestNumber,
  RepoSlug,
  Score as ScoreT,
} from "./types/branded.js";
import type { CopilotReport } from "./types/copilot.js";
import type {
  CiSummary,
  Lifecycle,
  PullRequestFitnessReport,
  ReviewSummary,
} from "./types/index.js";

/** Default target if the caller doesn't specify — 💠 platinum (4). */
export const DEFAULT_TARGET: ScoreT = Score(4);

function toLifecycle(state: "OPEN" | "MERGED" | "CLOSED"): Lifecycle {
  return state.toLowerCase() as Lifecycle;
}

function isMergeable(
  lifecycle: Lifecycle,
  blockers: readonly string[],
): boolean {
  if (lifecycle === "merged") return true;
  if (lifecycle === "closed") return false;
  return blockers.length === 0;
}

/**
 * Compute a scalar fitness score.
 *
 * When Copilot is configured, use its tier ordinal as the score —
 * it's already a total order on review quality.
 *
 * Otherwise fall back to a CI/review-derived scalar:
 *   4: green CI, approved (or no-review-policy)
 *   3: green CI, approval pending (but no blockers)
 *   2: CI green but unresolved threads
 *   1: any hard blocker (CI fail, conflict, draft, wip, …)
 */
function computeScore(
  lifecycle: Lifecycle,
  blockers: readonly string[],
  ci: CiSummary,
  reviews: ReviewSummary,
  copilot: CopilotReport,
): ScoreT {
  if (lifecycle === "merged") return Score(4);
  if (lifecycle === "closed") return Score(0);

  if (copilot.configured) {
    switch (copilot.tier) {
      case "bronze":
        return Score(1);
      case "silver":
        return Score(2);
      case "gold":
        return Score(3);
      case "platinum":
        return Score(4);
    }
  }

  if (blockers.length > 0) return Score(1);

  const ciGreen = ci.fail === 0 && ci.pending === 0;
  if (!ciGreen) return Score(1);

  if (reviews.threads_unresolved > 0) return Score(2);

  const approved =
    reviews.decision === "APPROVED" || reviews.decision === "NONE";
  return approved ? Score(4) : Score(3);
}

/** Assess PR merge readiness. All state is queried live. */
export async function prFitness(
  repo: RepoSlug,
  pr: PullRequestNumber,
  target: ScoreT = DEFAULT_TARGET,
): Promise<PullRequestFitnessReport> {
  const start = performance.now();

  const data = await collect(repo, pr);

  const lifecycle = toLifecycle(data.pr.state);
  const ci = computeCi(data.checks);
  const reviews = computeReviews(
    data.pr,
    data.threads,
    data.comments,
    data.reviews,
  );
  const state = computeState(data.pr, data.lastCommitDate);

  const copilot = computeCopilot({
    events: data.events,
    reviews: data.reviews,
    threads: data.threads,
    requestedReviewers: data.requestedReviewers,
    config: data.copilotConfig,
    head: GitCommitSha(data.pr.headRefOid),
  });

  // Merged/closed PRs have no actionable blockers.
  const blockers =
    lifecycle === "open"
      ? computeBlockers(ci, reviews, state, data.graphite.status)
      : [];
  const actions =
    lifecycle === "open"
      ? plan(ci, reviews, state, data.graphite.status, copilot, repo, pr)
      : [];

  const score = computeScore(lifecycle, blockers, ci, reviews, copilot);

  const durationMs = Math.round(performance.now() - start);

  const base: PullRequestFitnessReport = {
    version: VERSION,
    pr: data.pr.number,
    url: data.pr.url,
    title: data.pr.title,
    head: data.pr.headRefOid.slice(0, 8),
    base: data.pr.baseRefName,
    lifecycle,
    score,
    target,
    merged_at: data.pr.mergedAt ?? null,
    closed_at: data.pr.closedAt ?? null,
    mergeable: isMergeable(lifecycle, blockers),
    blockers,
    ci,
    reviews,
    copilot,
    state,
    graphite: data.graphite,
    actions,
    summary: summarize(lifecycle, blockers, data.pr.mergedAt),
    timestamp: new Date().toISOString(),
    duration_ms: durationMs,
  };

  if (lifecycle === "merged" || lifecycle === "closed") {
    return { ...base, terminal: { kind: lifecycle } };
  }
  return base;
}
