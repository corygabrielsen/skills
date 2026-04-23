/**
 * Cursor compute tests — tier scoring, check-run driven states,
 * and parallel semantics to the Copilot tier system.
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  countCursorThreads,
  scoreCursorTier,
  findCursorCheck,
  parseCursorReviewBody,
} from "../../src/compute/cursor.js";
import { GitCommitSha, Timestamp } from "../../src/types/branded.js";
import type { Timestamp as TimestampT } from "../../src/types/branded.js";
import type {
  GitHubCheck,
  GitHubPullRequestReviewThreadsResponse,
} from "../../src/types/index.js";
import type { CursorReviewRound } from "../../src/types/cursor.js";

const HEAD = GitCommitSha("abc12345abc12345abc12345abc12345abc12345");
const OLD = GitCommitSha("def67890def67890def67890def67890def67890");
const CURSOR = "cursor[bot]";

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

function round(overrides: Partial<CursorReviewRound> = {}): CursorReviewRound {
  return {
    round: 1,
    reviewedAt: ts("2026-04-10T10:05:00Z"),
    commit: HEAD,
    findingsCount: 1,
    ...overrides,
  };
}

function check(state: GitHubCheck["state"]): GitHubCheck {
  return {
    name: "Cursor Bugbot",
    state,
    description: "",
    link: "",
    completedAt: "",
  };
}

const cleanThreads = { total: 0, resolved: 0, unresolved: 0, stale: 0 };
const oneUnresolved = { total: 1, resolved: 0, unresolved: 1, stale: 0 };
const oneResolved = { total: 1, resolved: 1, unresolved: 0, stale: 0 };

describe("parseCursorReviewBody", () => {
  it("extracts findings count from 'found N potential issue(s)'", () => {
    assert.equal(
      parseCursorReviewBody("Cursor Bugbot has reviewed your changes and found 3 potential issues.").findingsCount,
      3,
    );
    assert.equal(
      parseCursorReviewBody("found 1 potential issue.").findingsCount,
      1,
    );
  });

  it("returns 0 when pattern not matched", () => {
    assert.equal(parseCursorReviewBody("some other text").findingsCount, 0);
  });
});

describe("findCursorCheck", () => {
  it("finds Cursor Bugbot by exact name", () => {
    const checks = [
      { name: "Lint", state: "SUCCESS" as const, description: "", link: "", completedAt: "" },
      check("IN_PROGRESS"),
    ];
    const found = findCursorCheck(checks);
    assert.equal(found?.name, "Cursor Bugbot");
    assert.equal(found?.state, "IN_PROGRESS");
  });

  it("returns null when no Cursor check present", () => {
    assert.equal(findCursorCheck([]), null);
  });
});

describe("countCursorThreads — cursor-authored detection", () => {
  it("counts only threads whose first comment is Cursor", () => {
    const t = threadsFromNodes([
      thread(false, [{ login: CURSOR, createdAt: "2026-04-10T10:05:00Z" }]),
      thread(false, [{ login: "cory", createdAt: "2026-04-10T10:05:00Z" }]),
    ]);
    assert.deepEqual(countCursorThreads(t, null), {
      total: 1,
      resolved: 0,
      unresolved: 1,
      stale: 0,
    });
  });

  it("flags stale when human replies after latest review", () => {
    const t = threadsFromNodes([
      thread(true, [
        { login: CURSOR, createdAt: "2026-04-10T10:05:00Z" },
        { login: "cory", createdAt: "2026-04-10T10:30:00Z" },
      ]),
    ]);
    const summary = countCursorThreads(t, ts("2026-04-10T10:05:00Z"));
    assert.equal(summary.stale, 1);
  });
});

describe("scoreCursorTier — tier semantics", () => {
  it("platinum: check at HEAD = SUCCESS (bot says clean)", () => {
    assert.equal(
      scoreCursorTier([], cleanThreads, check("SUCCESS"), HEAD),
      "platinum",
    );
  });

  it("bronze: unresolved threads dominate any check state", () => {
    assert.equal(
      scoreCursorTier([round()], oneUnresolved, check("SUCCESS"), HEAD),
      "bronze",
    );
  });

  it("silver: threads resolved, check in progress, prior review exists", () => {
    assert.equal(
      scoreCursorTier(
        [round({ commit: OLD })],
        oneResolved,
        check("IN_PROGRESS"),
        HEAD,
      ),
      "silver",
    );
    assert.equal(
      scoreCursorTier(
        [round({ commit: OLD })],
        oneResolved,
        check("QUEUED"),
        HEAD,
      ),
      "silver",
    );
  });

  it("bronze: check in progress but no prior review (first-time)", () => {
    assert.equal(
      scoreCursorTier([], cleanThreads, check("IN_PROGRESS"), HEAD),
      "bronze",
    );
  });

  it("gold: findings at HEAD resolved (check NEUTRAL)", () => {
    assert.equal(
      scoreCursorTier([round()], oneResolved, check("NEUTRAL"), HEAD),
      "gold",
    );
  });

  it("gold: reviewed at non-HEAD, no check at HEAD", () => {
    assert.equal(
      scoreCursorTier(
        [round({ commit: OLD })],
        oneResolved,
        null,
        HEAD,
      ),
      "gold",
    );
  });

  it("bronze: never reviewed, no check", () => {
    assert.equal(scoreCursorTier([], cleanThreads, null, HEAD), "bronze");
  });
});
