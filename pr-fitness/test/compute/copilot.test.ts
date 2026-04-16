/**
 * Copilot compute tests — focused on tier scoring and the stale-thread
 * signal that distinguishes truly-platinum state from "author replied
 * without pushing" state (bug: pr-fitness-dismissal).
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  countCopilotThreads,
  scoreCopilotTier,
} from "../../src/compute/copilot.js";
import { GitCommitSha, Timestamp } from "../../src/types/branded.js";
import type { Timestamp as TimestampT } from "../../src/types/branded.js";
import type {
  CopilotReviewRound,
  GitHubPullRequestReviewThreadsResponse,
} from "../../src/types/index.js";

const HEAD = GitCommitSha("abc12345abc12345abc12345abc12345abc12345");
const OLD = GitCommitSha("def67890def67890def67890def67890def67890");
const COPILOT = "copilot-pull-request-reviewer[bot]";

function ts(s: string): TimestampT {
  return Timestamp(s);
}

function thread(
  isResolved: boolean,
  comments: { login: string; createdAt: string }[],
): GitHubPullRequestReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][number] {
  return {
    isResolved,
    comments: {
      nodes: comments.map((c) => ({
        author: { login: c.login },
        createdAt: c.createdAt,
      })),
    },
  };
}

function threadsFromNodes(
  nodes: ReturnType<typeof thread>[],
): GitHubPullRequestReviewThreadsResponse {
  return {
    data: {
      repository: {
        pullRequest: {
          reviewThreads: { nodes },
          reviewRequests: { nodes: [] },
        },
      },
    },
  };
}

function round(
  overrides: Partial<CopilotReviewRound> = {},
): CopilotReviewRound {
  return {
    round: 1,
    requestedAt: ts("2026-04-10T10:00:00Z"),
    ackAt: ts("2026-04-10T10:01:00Z"),
    reviewedAt: ts("2026-04-10T10:05:00Z"),
    commit: HEAD,
    commentsVisible: 0,
    commentsSuppressed: 0,
    ...overrides,
  };
}

describe("countCopilotThreads — stale detection", () => {
  it("counts a thread as stale when author replies after latest review", () => {
    const t = threadsFromNodes([
      thread(true, [
        { login: COPILOT, createdAt: "2026-04-10T10:05:00Z" },
        { login: "cory", createdAt: "2026-04-10T10:30:00Z" },
      ]),
    ]);
    const s = countCopilotThreads(t, ts("2026-04-10T10:05:00Z"));
    assert.equal(s.total, 1);
    assert.equal(s.stale, 1);
  });

  it("does not count as stale when author reply predates the review", () => {
    const t = threadsFromNodes([
      thread(true, [
        { login: COPILOT, createdAt: "2026-04-10T10:05:00Z" },
        { login: "cory", createdAt: "2026-04-10T10:02:00Z" },
      ]),
    ]);
    const s = countCopilotThreads(t, ts("2026-04-10T10:05:00Z"));
    assert.equal(s.stale, 0);
  });

  it("counts at most once per thread regardless of how many replies are stale", () => {
    const t = threadsFromNodes([
      thread(false, [
        { login: COPILOT, createdAt: "2026-04-10T10:05:00Z" },
        { login: "cory", createdAt: "2026-04-10T10:30:00Z" },
        { login: "otheruser", createdAt: "2026-04-10T10:40:00Z" },
      ]),
    ]);
    const s = countCopilotThreads(t, ts("2026-04-10T10:05:00Z"));
    assert.equal(s.stale, 1);
  });

  it("ignores non-Copilot threads", () => {
    const t = threadsFromNodes([
      thread(false, [
        { login: "cory", createdAt: "2026-04-10T10:00:00Z" },
        { login: "cory", createdAt: "2026-04-10T11:00:00Z" },
      ]),
    ]);
    const s = countCopilotThreads(t, ts("2026-04-10T10:05:00Z"));
    assert.equal(s.total, 0);
    assert.equal(s.stale, 0);
  });

  it("stale is 0 when no review has completed (latestReviewedAt=null)", () => {
    const t = threadsFromNodes([
      thread(false, [
        { login: COPILOT, createdAt: "2026-04-10T10:00:00Z" },
        { login: "cory", createdAt: "2026-04-10T11:00:00Z" },
      ]),
    ]);
    const s = countCopilotThreads(t, null);
    assert.equal(s.stale, 0);
  });
});

describe("scoreCopilotTier — stale replies cap at gold", () => {
  const cleanThreads = { total: 0, resolved: 0, unresolved: 0, stale: 0 };
  const staleThreads = { total: 1, resolved: 1, unresolved: 0, stale: 1 };

  it("state C (reviewed at HEAD, no post-review activity) → platinum", () => {
    assert.equal(
      scoreCopilotTier([round({ commit: HEAD })], cleanThreads, HEAD),
      "platinum",
    );
  });

  it("state B (reviewed at HEAD, author replied without pushing) → gold", () => {
    assert.equal(
      scoreCopilotTier([round({ commit: HEAD })], staleThreads, HEAD),
      "gold",
    );
  });

  it("state A (reviewed at OLD, new commits pushed) → gold", () => {
    assert.equal(
      scoreCopilotTier([round({ commit: OLD })], cleanThreads, HEAD),
      "gold",
    );
  });

  it("unresolved threads dominate stale → bronze", () => {
    const t = { total: 2, resolved: 0, unresolved: 1, stale: 1 };
    assert.equal(
      scoreCopilotTier([round({ commit: HEAD })], t, HEAD),
      "bronze",
    );
  });

  it("suppressed comments dominate stale → silver", () => {
    assert.equal(
      scoreCopilotTier(
        [round({ commit: HEAD, commentsSuppressed: 2 })],
        staleThreads,
        HEAD,
      ),
      "silver",
    );
  });
});
