/**
 * Copilot compute — pure transformation from raw GitHub API data to
 * the distilled `CopilotReport` domain shape.
 *
 * Contract: no I/O, no clock, no mutation of inputs. All returned
 * structures are `readonly` and safe to share.
 */

import type { GitCommitSha, Timestamp } from "../types/branded.js";
import type {
  CopilotActivity,
  CopilotReport,
  CopilotRepoConfig,
  CopilotReviewRound,
  CopilotThreadSummary,
  CopilotTier,
  GitHubIssueEvent,
  GitHubPullRequestReview,
  GitHubPullRequestReviewThreadsResponse,
  GitHubRequestedReviewers,
} from "../types/index.js";
import { isCopilot } from "../types/copilot-identity.js";
import { formatCopilotTier } from "../types/copilot.js";
import { countBotThreads } from "./bot-threads.js";

const VISIBLE_RE = /generated (\d+) comments?\./;
const SUPPRESSED_RE = /Comments suppressed due to low confidence \((\d+)\)/;

export function parseCopilotReviewBody(body: string): {
  readonly visible: number;
  readonly suppressed: number;
} {
  // "generated no new comments" is Copilot's explicit zero-case —
  // checked before the numeric regex which would otherwise miss it.
  const visible = body.includes("generated no new comments")
    ? 0
    : Number(body.match(VISIBLE_RE)?.[1] ?? 0);
  const suppressed = Number(body.match(SUPPRESSED_RE)?.[1] ?? 0);
  return { visible, suppressed };
}

interface CopilotTimelinePoint {
  readonly kind: "requested" | "ack";
  readonly at: Timestamp;
}

function copilotTimeline(
  events: readonly GitHubIssueEvent[],
): readonly CopilotTimelinePoint[] {
  const points: CopilotTimelinePoint[] = [];
  for (const e of events) {
    if (e.created_at === null) continue;
    if (
      e.event === "review_requested" &&
      "requested" in e &&
      e.requested.kind === "user" &&
      isCopilot(e.requested.login)
    ) {
      points.push({ kind: "requested", at: e.created_at });
    } else if (e.event === "copilot_work_started") {
      points.push({ kind: "ack", at: e.created_at });
    }
  }
  points.sort((a, b) => (a.at < b.at ? -1 : a.at > b.at ? 1 : 0));
  return points;
}

export function correlateReviewRounds(
  timeline: readonly CopilotTimelinePoint[],
  reviews: readonly GitHubPullRequestReview[],
): readonly CopilotReviewRound[] {
  const sortedReviews = [...reviews].sort((a, b) =>
    a.submitted_at < b.submitted_at ? -1 : 1,
  );

  const rounds: CopilotReviewRound[] = [];
  let reviewCursor = 0;

  for (let i = 0; i < timeline.length; i++) {
    const req = timeline[i];
    if (req === undefined || req.kind !== "requested") continue;

    // Window end = next request, or unbounded if this is the last.
    let windowEnd: Timestamp | null = null;
    let ackAt: Timestamp | null = null;
    for (let j = i + 1; j < timeline.length; j++) {
      const p = timeline[j];
      if (p === undefined) continue;
      if (p.kind === "requested") {
        windowEnd = p.at;
        break;
      }
      if (ackAt === null) ackAt = p.at;
    }

    let reviewedAt: Timestamp | null = null;
    let commit: GitCommitSha | null = null;
    let commentsVisible = 0;
    let commentsSuppressed = 0;

    while (reviewCursor < sortedReviews.length) {
      const rev = sortedReviews[reviewCursor];
      if (rev === undefined) break;
      if (rev.submitted_at < req.at) {
        reviewCursor++;
        continue;
      }
      if (windowEnd !== null && rev.submitted_at >= windowEnd) break;
      reviewedAt = rev.submitted_at;
      commit = rev.commit_id;
      const counts = parseCopilotReviewBody(rev.body);
      commentsVisible = counts.visible;
      commentsSuppressed = counts.suppressed;
      reviewCursor++;
      break;
    }

    rounds.push({
      round: rounds.length + 1,
      requestedAt: req.at,
      ackAt,
      reviewedAt,
      commit,
      commentsVisible,
      commentsSuppressed,
    });
  }

  return rounds;
}

export function computeCopilotActivity(
  timeline: readonly CopilotTimelinePoint[],
  rounds: readonly CopilotReviewRound[],
  requestedReviewers: GitHubRequestedReviewers,
): CopilotActivity {
  const latest = rounds.at(-1);
  if (latest === undefined) {
    const pending = requestedReviewers.users.some((u) => isCopilot(u.login));
    const latestEvent = timeline.at(-1);
    if (pending && latestEvent !== undefined) {
      return { state: "requested", requestedAt: latestEvent.at };
    }
    return { state: "idle" };
  }

  if (latest.reviewedAt !== null) {
    return { state: "reviewed", latest };
  }
  if (latest.ackAt !== null) {
    return {
      state: "working",
      requestedAt: latest.requestedAt,
      ackAt: latest.ackAt,
    };
  }
  return { state: "requested", requestedAt: latest.requestedAt };
}

export function countCopilotThreads(
  threads: GitHubPullRequestReviewThreadsResponse,
  latestReviewedAt: Timestamp | null,
): CopilotThreadSummary {
  return countBotThreads(threads, latestReviewedAt, isCopilot);
}

export function scoreCopilotTier(
  rounds: readonly CopilotReviewRound[],
  threads: CopilotThreadSummary,
  head: GitCommitSha,
): CopilotTier {
  const latest = rounds.at(-1);
  if (latest === undefined) return "bronze";
  if (latest.reviewedAt === null) return "bronze";
  if (threads.unresolved > 0) return "bronze";
  if (latest.commentsSuppressed > 0) return "silver";
  if (threads.stale > 0) return "gold";
  if (latest.commit === head) return "platinum";
  return "gold";
}

export function isFresh(
  rounds: readonly CopilotReviewRound[],
  head: GitCommitSha,
): boolean {
  const latest = rounds.at(-1);
  return latest?.reviewedAt !== null && latest?.commit === head;
}

export function computeCopilot(input: {
  readonly events: readonly GitHubIssueEvent[];
  readonly reviews: readonly GitHubPullRequestReview[];
  readonly threads: GitHubPullRequestReviewThreadsResponse;
  readonly requestedReviewers: GitHubRequestedReviewers;
  readonly config: CopilotRepoConfig;
  readonly head: GitCommitSha;
}): CopilotReport {
  if (!input.config.enabled) {
    return { configured: false };
  }

  const copilotReviews = input.reviews.filter((r) => isCopilot(r.user));
  const timeline = copilotTimeline(input.events);
  const rounds = correlateReviewRounds(timeline, copilotReviews);
  const latestReviewedAt = rounds.at(-1)?.reviewedAt ?? null;
  const threads = countCopilotThreads(input.threads, latestReviewedAt);
  const activity = computeCopilotActivity(
    timeline,
    rounds,
    input.requestedReviewers,
  );
  const tier = scoreCopilotTier(rounds, threads, input.head);
  const fresh = isFresh(rounds, input.head);

  return {
    configured: true,
    config: input.config,
    activity,
    rounds,
    threads,
    tier,
    tier_display: formatCopilotTier(tier),
    fresh,
  };
}
