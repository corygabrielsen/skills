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
import {
  COPILOT_TIER_EMOJI,
  formatScoreOrdinal,
  tierForScore,
} from "./types/copilot.js";
import type {
  AxisLine,
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
 * Collect informational lines that shouldn't drive fitness but that a
 * human reader should see — e.g. advisory check failures that didn't
 * block merge but did fail. Empty when nothing needs flagging.
 */
function computeNotes(ci: CiSummary): readonly string[] {
  const notes: string[] = [];
  if (ci.advisory.fail > 0) {
    notes.push(
      `${String(ci.advisory.fail)} advisory check${
        ci.advisory.fail === 1 ? "" : "s"
      } failing (non-blocking): ${ci.advisory.failed.join(", ")}`,
    );
  }
  return notes;
}

/**
 * Branded rendering of a score. Uses the tier emoji/label for any 1..4
 * score — the ordinal scale is identical whether Copilot is configured
 * or not, so 🥇 (gold) is just as motivating on a CI/review-only PR as
 * on a Copilot one. Merged PRs get the top-tier emoji as a terminal
 * marker; closed PRs get their own neutral marker.
 */
function computeScoreDisplay(lifecycle: Lifecycle, score: ScoreT): string {
  if (lifecycle === "merged") return `${COPILOT_TIER_EMOJI.platinum} (merged)`;
  if (lifecycle === "closed") return "⚫ (closed)";
  return formatScoreOrdinal(score);
}

function scoreEmoji(lifecycle: Lifecycle, score: ScoreT): string {
  if (lifecycle === "merged") return COPILOT_TIER_EMOJI.platinum;
  if (lifecycle === "closed") return "⚫";
  const tier = tierForScore(score);
  return tier !== null ? COPILOT_TIER_EMOJI[tier] : "";
}

function scoreLabel(lifecycle: Lifecycle, score: ScoreT): string {
  if (lifecycle === "merged") return "merged";
  if (lifecycle === "closed") return "closed";
  const tier = tierForScore(score);
  return tier ?? `score ${String(score as number)}`;
}

function computeAxes(
  ci: CiSummary,
  copilot: CopilotReport,
  reviews: ReviewSummary,
): readonly AxisLine[] {
  const axes: AxisLine[] = [];

  // CI axis
  if (ci.fail > 0) {
    axes.push({
      name: "CI",
      emoji: "❌",
      summary: ci.failed.join(", ") + " failing",
    });
  } else if (ci.pending > 0) {
    axes.push({
      name: "CI",
      emoji: "⏳",
      summary: `${String(ci.pending)} check${ci.pending === 1 ? "" : "s"} running`,
    });
  } else {
    axes.push({ name: "CI", emoji: "✅", summary: "" });
  }

  // Copilot axis
  if (copilot.configured) {
    switch (copilot.activity.state) {
      case "idle":
        axes.push({ name: "Copilot", emoji: "⏳", summary: "awaiting review" });
        break;
      case "requested":
        axes.push({
          name: "Copilot",
          emoji: "⏳",
          summary: "review requested",
        });
        break;
      case "working":
        axes.push({
          name: "Copilot",
          emoji: "⏳",
          summary: `reviewing (round ${String(copilot.rounds.length)})`,
        });
        break;
      case "reviewed": {
        const tierEmoji = COPILOT_TIER_EMOJI[copilot.tier];
        let detail = "";
        if (copilot.threads.unresolved > 0) {
          detail = `${String(copilot.threads.unresolved)} unresolved thread${copilot.threads.unresolved === 1 ? "" : "s"}`;
        } else if (copilot.threads.stale > 0) {
          detail = `hasn't seen ${String(copilot.threads.stale)} post-review repl${copilot.threads.stale === 1 ? "y" : "ies"}`;
        } else if (!copilot.fresh) {
          detail = "reviewed, not at HEAD";
        } else {
          detail = "reviewed at HEAD";
        }
        axes.push({
          name: "Copilot",
          emoji: tierEmoji,
          summary: `${copilot.tier}${detail.length > 0 ? " · " + detail : ""}`,
        });
        break;
      }
      case "unconfigured":
        break;
    }
  }

  // Approval axis
  if (reviews.pending_reviews.humans.length > 0) {
    axes.push({
      name: "Approval",
      emoji: "⏳",
      summary: `pending from ${reviews.pending_reviews.humans.join(", ")}`,
    });
  } else if (reviews.decision === "CHANGES_REQUESTED") {
    axes.push({
      name: "Approval",
      emoji: "❌",
      summary: "changes requested",
    });
  } else if (reviews.decision === "APPROVED" || reviews.decision === "NONE") {
    axes.push({ name: "Approval", emoji: "✅", summary: "" });
  }

  return axes;
}

interface SnapshotInput {
  readonly pr: { number: number; headRefOid: string; baseRefName: string };
  readonly lifecycle: Lifecycle;
  readonly score: ScoreT;
  readonly target: ScoreT;
  readonly blockers: readonly string[];
  readonly ci: CiSummary;
  readonly copilot: CopilotReport;
  readonly reviews: ReviewSummary;
  readonly state: import("./types/output.js").PullRequestState;
  readonly graphiteStatus: string;
}

function buildSnapshot(input: SnapshotInput): Record<string, unknown> {
  const {
    pr,
    lifecycle,
    score,
    target,
    blockers,
    ci,
    copilot,
    reviews,
    state,
  } = input;
  return {
    version: VERSION,
    pr: pr.number,
    head: pr.headRefOid.slice(0, 8),
    base: pr.baseRefName,
    lifecycle,
    score: score as number,
    target: target as number,
    blockers,
    ci: {
      pass: ci.pass,
      fail: ci.fail,
      pending: ci.pending,
      failed: ci.failed_details.map((d) => ({ name: d.name, link: d.link })),
      advisory_failed: ci.advisory.failed,
    },
    copilot: copilot.configured
      ? {
          tier: copilot.tier,
          activity: copilot.activity.state,
          fresh: copilot.fresh,
          stale: copilot.threads.stale,
          unresolved: copilot.threads.unresolved,
        }
      : { configured: false },
    reviews: {
      decision: reviews.decision,
      threads_unresolved: reviews.threads_unresolved,
      approvals_on_head: reviews.approvals_on_head,
      pending_humans: reviews.pending_reviews.humans,
      pending_bots: reviews.pending_reviews.bots,
    },
    state: {
      conflict: state.conflict,
      draft: state.draft,
      behind: state.behind,
    },
    graphite: input.graphiteStatus,
  };
}

export function computeScore(
  lifecycle: Lifecycle,
  blockers: readonly string[],
  copilot: CopilotReport,
): ScoreT {
  if (lifecycle === "merged") return Score(4);
  if (lifecycle === "closed") return Score(0);

  // Any hard blocker caps score at 1 regardless of Copilot tier.
  // Copilot being platinum means Copilot is happy — it doesn't
  // mean the PR is merge-ready. CI failures, missing approvals,
  // merge conflicts, and other gates are orthogonal to Copilot's
  // assessment; a PR with any of those outstanding cannot be at
  // target.
  if (blockers.length > 0) return Score(1);

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

  // No blockers, no Copilot — every gate (CI, reviews, conflicts,
  // metadata) passed because any fail would have emitted a blocker
  // above. Score is at target.
  return Score(4);
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
  const ci = computeCi(data.checks, data.requiredCheckNames);
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

  const score = computeScore(lifecycle, blockers, copilot);
  const scoreDisplay = computeScoreDisplay(lifecycle, score);
  const targetDisplay = formatScoreOrdinal(target);
  const statusLine = summarize(lifecycle, blockers, data.pr.mergedAt);
  const notes = computeNotes(ci);
  const activityState: Record<string, string> = copilot.configured
    ? { copilot: copilot.activity.state }
    : {};
  const axes = lifecycle === "open" ? computeAxes(ci, copilot, reviews) : [];
  const targetTier = tierForScore(target);
  const durationMs = Math.round(performance.now() - start);
  const timestamp = new Date().toISOString();
  const snapshot = buildSnapshot({
    pr: data.pr,
    lifecycle,
    score,
    target,
    blockers,
    ci,
    copilot,
    reviews,
    state,
    graphiteStatus: data.graphite.status,
  });

  const base: PullRequestFitnessReport = {
    version: VERSION,
    pr: data.pr.number,
    url: data.pr.url,
    title: data.pr.title,
    head: data.pr.headRefOid.slice(0, 8),
    base: data.pr.baseRefName,
    lifecycle,
    score,
    score_display: scoreDisplay,
    target,
    target_display: targetDisplay,
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
    status: statusLine,
    notes,
    activity_state: activityState,
    score_emoji: scoreEmoji(lifecycle, score),
    score_label: scoreLabel(lifecycle, score),
    target_label: targetTier ?? `score ${String(target as number)}`,
    axes,
    snapshot: { ...snapshot, timestamp, duration_ms: durationMs },
    timestamp,
    duration_ms: durationMs,
  };

  if (lifecycle === "merged" || lifecycle === "closed") {
    return { ...base, terminal: { kind: lifecycle } };
  }
  return base;
}
