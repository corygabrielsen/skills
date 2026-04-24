/**
 * Tests for computeScore — the function that maps (lifecycle,
 * agentBlockers, copilot) to the final fitness scalar.
 *
 * Key invariant: only AGENT-resolvable blockers cap the score.
 * Human-dependent blockers (pending review, not approved) don't
 * cap — they drive hil halts but don't regress the score.
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeScore } from "../../src/pr-fitness.js";
import { GitCommitSha, Timestamp } from "../../src/types/branded.js";
import type { CopilotReport } from "../../src/types/copilot.js";
import type { CursorReport } from "../../src/types/cursor.js";

const HEAD = GitCommitSha("abc12345abc12345abc12345abc12345abc12345");

const noCursor: CursorReport = { configured: false };

function platinumCopilot(): CopilotReport {
  return {
    configured: true,
    config: {
      enabled: true,
      reviewOnPush: false,
      reviewDraftPullRequests: false,
    },
    activity: {
      state: "reviewed",
      latest: {
        round: 1,
        requestedAt: Timestamp("2026-04-01T00:00:00Z"),
        ackAt: Timestamp("2026-04-01T00:01:00Z"),
        reviewedAt: Timestamp("2026-04-01T00:05:00Z"),
        commit: HEAD,
        commentsVisible: 0,
        commentsSuppressed: 0,
      },
    },
    rounds: [],
    threads: { total: 0, resolved: 0, unresolved: 0, stale: 0 },
    tier: "platinum",
    tier_display: "💠 (platinum)",
    fresh: true,
  };
}

describe("computeScore — agent blocker cap", () => {
  it("caps at 1 when agent-resolvable blockers exist, even if Copilot platinum", () => {
    const score = computeScore(
      "open",
      ["ci_fail: Mergeability Check"],
      platinumCopilot(),
      noCursor,
    );
    assert.equal(score as number, 1);
  });

  it("does NOT cap on human-only blockers — score reflects Copilot tier", () => {
    // This is the concept algebra fix: pending_human_review is a
    // hil-halt trigger, not a score regression.
    const score = computeScore("open", [], platinumCopilot(), noCursor);
    assert.equal(score as number, 4);
    // The key: "not_approved" and "pending_human_review" are NOT
    // passed as agentBlockers — they're in blockerSplit.human,
    // which computeScore never sees.
  });

  it("returns 4 for merged PRs regardless of blockers", () => {
    const score = computeScore(
      "merged",
      ["stale_blocker"],
      platinumCopilot(),
      noCursor,
    );
    assert.equal(score as number, 4);
  });

  it("returns 0 for closed PRs", () => {
    const score = computeScore("closed", [], platinumCopilot(), noCursor);
    assert.equal(score as number, 0);
  });

  it("returns 4 when Copilot is platinum and no agent blockers", () => {
    const score = computeScore("open", [], platinumCopilot(), noCursor);
    assert.equal(score as number, 4);
  });

  it("returns Copilot tier ordinal when no agent blockers (silver/gold)", () => {
    const silver: CopilotReport = {
      ...platinumCopilot(),
      tier: "silver",
      tier_display: "🥈 (silver)",
    } as CopilotReport;
    const gold: CopilotReport = {
      ...platinumCopilot(),
      tier: "gold",
      tier_display: "🥇 (gold)",
    } as CopilotReport;
    assert.equal(computeScore("open", [], silver, noCursor) as number, 2);
    assert.equal(computeScore("open", [], gold, noCursor) as number, 3);
  });

  it("returns 4 for non-Copilot PR with no agent blockers", () => {
    const unconfigured: CopilotReport = { configured: false };
    const score = computeScore("open", [], unconfigured, noCursor);
    assert.equal(score as number, 4);
  });

  it("returns 1 for non-Copilot PR with any agent blocker", () => {
    const unconfigured: CopilotReport = { configured: false };
    const score = computeScore(
      "open",
      ["ci_fail: lint"],
      unconfigured,
      noCursor,
    );
    assert.equal(score as number, 1);
  });
});
