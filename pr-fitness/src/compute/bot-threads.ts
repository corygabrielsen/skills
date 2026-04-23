/**
 * Shared bot-thread summarization. Parameterized by an identity
 * predicate — the algorithm is identical across reviewer bots.
 */

import type { Timestamp } from "../types/branded.js";
import type { GitHubPullRequestReviewThreadsResponse } from "../types/index.js";

export interface BotThreadSummary {
  readonly total: number;
  readonly resolved: number;
  readonly unresolved: number;
  readonly stale: number;
}

/**
 * Summarize review threads authored by a specific bot.
 *
 * A thread is bot-authored iff its first comment's login passes
 * `isBot`. A bot thread is `stale` iff it has any non-bot comment
 * with `createdAt > latestReviewedAt` — a reply the reviewer hasn't
 * observed. When `latestReviewedAt` is null no review has completed
 * so no thread can be stale.
 */
export function countBotThreads(
  threads: GitHubPullRequestReviewThreadsResponse,
  latestReviewedAt: Timestamp | null,
  isBot: (login: string) => boolean,
): BotThreadSummary {
  const nodes = threads.data.repository.pullRequest.reviewThreads.nodes;
  let total = 0;
  let resolved = 0;
  let unresolved = 0;
  let stale = 0;

  for (const t of nodes) {
    const first = t.comments.nodes[0];
    if (first === undefined) continue;
    if (!isBot(first.author.login)) continue;
    total++;
    if (t.isResolved) resolved++;
    else unresolved++;

    if (latestReviewedAt !== null) {
      for (const c of t.comments.nodes) {
        if (isBot(c.author.login)) continue;
        if (c.createdAt > latestReviewedAt) {
          stale++;
          break;
        }
      }
    }
  }

  return { total, resolved, unresolved, stale };
}
