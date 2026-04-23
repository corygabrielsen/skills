/**
 * Triage-wait tests — verify that CI waits are upgraded to agent
 * handoff when ambiguity signals are present, and left as normal
 * waits otherwise.
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { plan } from "../../src/compute/plan.js";
import { GitCommitSha, Timestamp } from "../../src/types/branded.js";
import type { CopilotReport } from "../../src/types/copilot.js";
import type { CursorReport } from "../../src/types/cursor.js";
import type {
  CiSummary,
  PullRequestState,
  ReviewSummary,
} from "../../src/types/output.js";

const HEAD = GitCommitSha("abc12345abc12345abc12345abc12345abc12345");

function quietCi(
  overrides: Partial<CiSummary> = {},
): CiSummary {
  return {
    pass: 1,
    fail: 0,
    pending: 0,
    total: 1,
    failed: [],
    pending_names: [],
    failed_details: [],
    missing: 0,
    missing_names: [],
    completed_at: "2026-04-23T10:00:00Z",
    advisory: {
      pass: 0,
      fail: 0,
      pending: 0,
      total: 0,
      failed: [],
      pending_names: [],
      failed_details: [],
    },
    ...overrides,
  };
}

function ciMissing(): CiSummary {
  return quietCi({
    missing: 1,
    missing_names: ["Mergeability Check"],
  });
}

function noCopilot(): CopilotReport {
  return { configured: false };
}

function noCursor(): CursorReport {
  return { configured: false };
}

function reviewingCursor(): CursorReport {
  return {
    configured: true,
    activity: { state: "reviewing" },
    rounds: [],
    threads: { total: 0, resolved: 0, unresolved: 0, stale: 0 },
    severity: { high: 0, medium: 0, low: 0 },
    tier: "bronze",
    tier_display: "🥉 (bronze)",
    fresh: false,
  };
}

function goldCopilotWithStale(): CopilotReport {
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
        requestedAt: Timestamp("2026-04-23T10:00:00Z"),
        ackAt: Timestamp("2026-04-23T10:01:00Z"),
        reviewedAt: Timestamp("2026-04-23T10:05:00Z"),
        commit: HEAD,
        commentsVisible: 2,
        commentsSuppressed: 0,
      },
    },
    rounds: [],
    threads: { total: 2, resolved: 2, unresolved: 0, stale: 1 },
    tier: "gold",
    tier_display: "🥇 (gold)",
    fresh: true,
  };
}

const emptyReviews: ReviewSummary = {
  decision: "NONE",
  threads_unresolved: 0,
  threads_total: 0,
  bot_comments: 0,
  approvals_on_head: 0,
  approvals_stale: 0,
  pending_reviews: { bots: [], humans: [] },
  bot_reviews: [],
};

const quietState: PullRequestState = {
  conflict: "MERGEABLE",
  draft: false,
  wip: false,
  title_len: 30,
  title_ok: true,
  body: true,
  summary: true,
  test_plan: true,
  content_label: true,
  assignees: 1,
  reviewers: 1,
  merge_when_ready: false,
  commits: 1,
  behind: false,
  updated_at: "2026-04-23T10:00:00Z",
  last_commit_at: "2026-04-23T10:00:00Z",
};

describe("triage_wait — fires on ambiguity signals", () => {
  it("cursor reviewing + missing CI → triage, not wait_for_ci", () => {
    const actions = plan(
      ciMissing(),
      emptyReviews,
      quietState,
      noCopilot(),
      reviewingCursor(),
      "w/r",
      1,
    );
    const kinds = actions.map((a) => a.kind);
    assert.ok(kinds.includes("triage_wait"), "triage_wait should fire");
    assert.ok(!kinds.includes("wait_for_ci"), "wait_for_ci should be suppressed");
  });

  it("advisory failure + missing CI → triage", () => {
    const ci = quietCi({
      missing: 1,
      missing_names: ["Mergeability Check"],
      advisory: {
        pass: 0,
        fail: 1,
        pending: 0,
        total: 1,
        failed: ["Lint"],
        pending_names: [],
        failed_details: [{ name: "Lint", description: "", link: "" }],
      },
    });
    const actions = plan(ci, emptyReviews, quietState, noCopilot(), noCursor(), "w/r", 1);
    const triage = actions.find((a) => a.kind === "triage_wait");
    assert.ok(triage, "triage_wait should fire");
    assert.match(triage.description, /Advisory "Lint" failed/);
  });

  it("copilot gold + missing CI → triage", () => {
    const actions = plan(
      ciMissing(),
      emptyReviews,
      quietState,
      goldCopilotWithStale(),
      noCursor(),
      "w/r",
      1,
    );
    const triage = actions.find((a) => a.kind === "triage_wait");
    assert.ok(triage);
    assert.match(triage.description, /Copilot gold/);
  });

  it("no signals + missing CI → normal wait_for_ci", () => {
    const actions = plan(
      ciMissing(),
      emptyReviews,
      quietState,
      noCopilot(),
      noCursor(),
      "w/r",
      1,
    );
    const kinds = actions.map((a) => a.kind);
    assert.ok(kinds.includes("wait_for_ci"));
    assert.ok(!kinds.includes("triage_wait"));
  });

  it("fail > 0 → no triage, failures take precedence", () => {
    const ci = quietCi({
      fail: 1,
      failed: ["Lint"],
      failed_details: [{ name: "Lint", description: "", link: "" }],
      missing: 1,
      missing_names: ["Mergeability Check"],
    });
    const actions = plan(
      ci,
      emptyReviews,
      quietState,
      noCopilot(),
      reviewingCursor(),
      "w/r",
      1,
    );
    const kinds = actions.map((a) => a.kind);
    assert.ok(!kinds.includes("triage_wait"), "triage should defer to fix_ci");
    assert.ok(kinds.includes("fix_ci"));
  });
});
