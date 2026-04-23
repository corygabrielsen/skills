import { collect } from "./collectors/index.js";
import { computeBlockers, EMPTY_BLOCKERS } from "./compute/blockers.js";
import { computeCi } from "./compute/ci.js";
import { computeCopilot } from "./compute/copilot.js";
import { computeCursor } from "./compute/cursor.js";
import { plan } from "./compute/plan.js";
import { computeReviews } from "./compute/reviews.js";
import { computeState } from "./compute/state.js";
import { summarize } from "./compute/summary.js";
import type { CollectorError } from "./util/collector-error.js";
import { pluralize } from "./util/text.js";
import { VERSION } from "./version.js";
import { GitCommitSha, Score } from "./types/branded.js";
import type {
  PullRequestNumber,
  RepoSlug,
  Score as ScoreT,
} from "./types/branded.js";
import type { CopilotReport, CopilotTier } from "./types/copilot.js";
import {
  COPILOT_TIER_EMOJI,
  compareCopilotTier,
  formatScoreOrdinal,
  tierForScore,
} from "./types/copilot.js";
import type { CursorReport } from "./types/cursor.js";
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
 * block merge but did fail, or degraded collectors whose errors were
 * absorbed. Empty when nothing needs flagging.
 */
function computeNotes(
  ci: CiSummary,
  degraded: readonly CollectorError[],
): readonly string[] {
  const notes: string[] = [];
  if (ci.advisory.fail > 0) {
    notes.push(
      `${String(ci.advisory.fail)} advisory check${
        ci.advisory.fail === 1 ? "" : "s"
      } failing (non-blocking): ${ci.advisory.failed.join(", ")}`,
    );
  }
  for (const d of degraded) {
    notes.push(`⚠ ${d.collector} degraded: ${d.ghError.kind}`);
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

/**
 * Render the `reviewed` detail suffix for a bot axis. Shared between
 * Copilot and Cursor — both check unresolved → stale → non-HEAD in
 * that order.
 */
function reviewedDetail(
  threads: { readonly unresolved: number; readonly stale: number },
  fresh: boolean,
): string {
  if (threads.unresolved > 0) {
    return pluralize(threads.unresolved, "unresolved thread");
  }
  if (threads.stale > 0) {
    return `hasn't seen ${pluralize(threads.stale, "post-review reply", "post-review replies")}`;
  }
  if (!fresh) return "reviewed, not at HEAD";
  return "reviewed at HEAD";
}

function computeAxes(
  ci: CiSummary,
  copilot: CopilotReport,
  cursor: CursorReport,
  reviews: ReviewSummary,
  requiredChecksDegraded: boolean,
): readonly AxisLine[] {
  const axes: AxisLine[] = [];

  // Required Checks axis — says what it is, not the generic "CI"
  if (requiredChecksDegraded) {
    axes.push({
      name: "Required Checks",
      emoji: "❓",
      summary: "config unavailable (query failed)",
    });
  } else if (ci.fail > 0) {
    axes.push({
      name: "Required Checks",
      emoji: "❌",
      summary: ci.failed.join(", ") + " failing",
    });
  } else if (ci.missing > 0) {
    axes.push({
      name: "Required Checks",
      emoji: "❓",
      summary: `${ci.missing_names.join(", ")} not started`,
    });
  } else if (ci.pending > 0) {
    axes.push({
      name: "Required Checks",
      emoji: "⏳",
      summary: `${String(ci.pending)} check${ci.pending === 1 ? "" : "s"} running`,
    });
  } else {
    axes.push({ name: "Required Checks", emoji: "✅", summary: "" });
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
      case "reviewed":
        axes.push({
          name: "Copilot",
          emoji: COPILOT_TIER_EMOJI[copilot.tier],
          summary: `${copilot.tier} · ${reviewedDetail(copilot.threads, copilot.fresh)}`,
        });
        break;
      case "unconfigured":
        break;
    }
  }

  // Cursor axis
  if (cursor.configured) {
    let detail: string;
    switch (cursor.activity.state) {
      case "idle":
        detail = "awaiting review";
        break;
      case "reviewing":
        detail = "reviewing";
        break;
      case "clean":
        detail = "clean at HEAD";
        break;
      case "reviewed":
        detail = reviewedDetail(cursor.threads, cursor.fresh);
        break;
    }
    axes.push({
      name: "Cursor",
      emoji: COPILOT_TIER_EMOJI[cursor.tier],
      summary: `${cursor.tier} · ${detail}`,
    });
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
  readonly cursor: CursorReport;
  readonly reviews: ReviewSummary;
  readonly state: import("./types/output.js").PullRequestState;
  readonly graphiteStatus: string;
  readonly degraded: readonly CollectorError[];
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
    cursor,
    reviews,
    state,
    degraded,
  } = input;
  const snap: Record<string, unknown> = {
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
      missing: ci.missing,
      failed: ci.failed_details.map((d) => ({ name: d.name, link: d.link })),
      missing_names: ci.missing_names,
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
    cursor: cursor.configured
      ? {
          tier: cursor.tier,
          activity: cursor.activity.state,
          fresh: cursor.fresh,
          stale: cursor.threads.stale,
          unresolved: cursor.threads.unresolved,
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
  if (degraded.length > 0) {
    snap.degraded = degraded.map((d) => ({
      collector: d.collector,
      error: d.ghError.kind,
    }));
  }
  return snap;
}

/**
 * Combine configured reviewer-bot tiers by taking the minimum
 * (worst). If neither is configured, returns null.
 */
function combinedBotTier(
  copilot: CopilotReport,
  cursor: CursorReport,
): CopilotTier | null {
  const tiers: CopilotTier[] = [];
  if (copilot.configured) tiers.push(copilot.tier);
  if (cursor.configured) tiers.push(cursor.tier);
  if (tiers.length === 0) return null;
  return tiers.reduce((min, t) => (compareCopilotTier(t, min) < 0 ? t : min));
}

export function computeScore(
  lifecycle: Lifecycle,
  agentBlockers: readonly string[],
  copilot: CopilotReport,
  cursor: CursorReport,
): ScoreT {
  if (lifecycle === "merged") return Score(4);
  if (lifecycle === "closed") return Score(0);

  // Only agent-resolvable blockers cap the score. Human-dependent
  // blockers (pending review, not approved) drive hil halts but
  // don't regress the score — the PR's quality hasn't decreased,
  // a human just hasn't acted yet. This lets the emoji progression
  // converge (🥉→🥈→🥇→💠) while the agent works, with the hil
  // halt at the end saying "your turn."
  if (agentBlockers.length > 0) return Score(1);

  const botTier = combinedBotTier(copilot, cursor);
  if (botTier !== null) {
    switch (botTier) {
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

  // No blockers, no reviewer bots configured — every gate (CI,
  // reviews, conflicts, metadata) passed because any fail would
  // have emitted a blocker above. Score is at target.
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
  const ci = computeCi(data.checks, data.requiredCheckConfig);
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

  const cursor = computeCursor({
    reviews: data.reviews,
    threads: data.threads,
    checks: data.checks,
    head: GitCommitSha(data.pr.headRefOid),
  });

  // Merged/closed PRs have no actionable blockers.
  const blockerSplit =
    lifecycle === "open"
      ? computeBlockers(ci, reviews, state, data.graphite.status)
      : EMPTY_BLOCKERS;
  const actions =
    lifecycle === "open"
      ? plan(ci, reviews, state, copilot, cursor, repo, pr)
      : [];

  // Score reflects agent-achievable fitness. Human-dependent blockers
  // (pending review, not approved) drive hil halts but don't cap score.
  const score = computeScore(lifecycle, blockerSplit.agent, copilot, cursor);
  const scoreDisplay = computeScoreDisplay(lifecycle, score);
  const targetDisplay = formatScoreOrdinal(target);
  const statusLine = summarize(lifecycle, blockerSplit.all, data.pr.mergedAt);
  const notes = computeNotes(ci, data.degraded);
  const activityState: Record<string, string> = {
    ...(copilot.configured ? { copilot: copilot.activity.state } : {}),
    ...(cursor.configured ? { cursor: cursor.activity.state } : {}),
  };
  const requiredChecksDegraded = data.degraded.some(
    (d) => d.collector === "required-checks",
  );
  const axes =
    lifecycle === "open"
      ? computeAxes(ci, copilot, cursor, reviews, requiredChecksDegraded)
      : [];
  const targetTier = tierForScore(target);
  const durationMs = Math.round(performance.now() - start);
  const timestamp = new Date().toISOString();
  const snapshot = buildSnapshot({
    pr: data.pr,
    lifecycle,
    score,
    target,
    blockers: blockerSplit.all,
    ci,
    copilot,
    cursor,
    reviews,
    state,
    graphiteStatus: data.graphite.status,
    degraded: data.degraded,
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
    mergeable: isMergeable(lifecycle, blockerSplit.all),
    blockers: blockerSplit.all,
    blocker_split: {
      agent: blockerSplit.agent,
      human: blockerSplit.human,
      structural: blockerSplit.structural,
    },
    ci,
    reviews,
    copilot,
    cursor,
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
