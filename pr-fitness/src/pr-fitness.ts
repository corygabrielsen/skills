import { collect } from "./collectors/index.js";
import { computeBlockers } from "./compute/blockers.js";
import { computeCi } from "./compute/ci.js";
import { plan } from "./compute/plan.js";
import { computeReviews } from "./compute/reviews.js";
import { computeState } from "./compute/state.js";
import { summarize } from "./compute/summary.js";
import { VERSION } from "./version.js";
import type { Lifecycle, PrFitnessReport } from "./types/index.js";

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

/** Assess PR merge readiness. All state is queried live. */
export async function prFitness(
  repo: string,
  pr: number,
): Promise<PrFitnessReport> {
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

  // Merged/closed PRs have no actionable blockers.
  const blockers =
    lifecycle === "open"
      ? computeBlockers(ci, reviews, state, data.graphite.status)
      : [];
  const actions =
    lifecycle === "open" ? plan(ci, reviews, state, data.graphite.status) : [];

  const durationMs = Math.round(performance.now() - start);

  return {
    version: VERSION,
    pr: data.pr.number,
    url: data.pr.url,
    title: data.pr.title,
    head: data.pr.headRefOid.slice(0, 8),
    base: data.pr.baseRefName,
    lifecycle,
    merged_at: data.pr.mergedAt ?? null,
    closed_at: data.pr.closedAt ?? null,
    mergeable: isMergeable(lifecycle, blockers),
    blockers,
    ci,
    reviews,
    state,
    graphite: data.graphite,
    actions,
    summary: summarize(lifecycle, blockers, data.pr.mergedAt),
    timestamp: new Date().toISOString(),
    duration_ms: durationMs,
  };
}
