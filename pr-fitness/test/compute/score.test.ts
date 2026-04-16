/**
 * Tests for computeScore — the function that maps (lifecycle, blockers,
 * copilot) to the final fitness scalar. Regression pinning for the bug
 * where Copilot=platinum caused score=4 even when CI was failing.
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeScore } from "../../src/pr-fitness.js";
import { GitCommitSha, Timestamp } from "../../src/types/branded.js";
import type { CopilotReport } from "../../src/types/copilot.js";

const HEAD = GitCommitSha("abc12345abc12345abc12345abc12345abc12345");

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

describe("computeScore — blocker cap", () => {
  it("caps score at 1 when blockers exist, even if Copilot is platinum", () => {
    // Regression: PR 1724 had Copilot platinum + Mergeability Check failing
    // and returned score=4, which made /converge halt success on a broken PR.
    const score = computeScore(
      "open",
      ["ci_fail: Mergeability Check", "not_approved"],
      platinumCopilot(),
    );
    assert.equal(score as number, 1);
  });

  it("returns 4 for merged PRs regardless of blockers", () => {
    const score = computeScore("merged", ["stale_blocker"], platinumCopilot());
    assert.equal(score as number, 4);
  });

  it("returns 0 for closed PRs", () => {
    const score = computeScore("closed", [], platinumCopilot());
    assert.equal(score as number, 0);
  });

  it("returns 4 when Copilot is platinum and no blockers", () => {
    const score = computeScore("open", [], platinumCopilot());
    assert.equal(score as number, 4);
  });

  it("returns Copilot tier ordinal when no blockers (silver/gold)", () => {
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
    assert.equal(computeScore("open", [], silver) as number, 2);
    assert.equal(computeScore("open", [], gold) as number, 3);
  });

  it("returns 4 for non-Copilot PR with no blockers", () => {
    const unconfigured: CopilotReport = { configured: false };
    const score = computeScore("open", [], unconfigured);
    assert.equal(score as number, 4);
  });

  it("returns 1 for non-Copilot PR with any blocker", () => {
    const unconfigured: CopilotReport = { configured: false };
    const score = computeScore("open", ["ci_fail: lint"], unconfigured);
    assert.equal(score as number, 1);
  });
});
