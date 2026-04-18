import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeBlockers } from "../../src/compute/blockers.js";
import type {
  CiSummary,
  PullRequestState,
  ReviewSummary,
} from "../../src/types/output.js";
import {
  APPROVED_REVIEWS,
  CLEAN_CI,
  CLEAN_STATE,
} from "../fixtures/helpers.js";

describe("computeBlockers", () => {
  it("returns empty for a fully mergeable PR", () => {
    const { all } = computeBlockers(
      CLEAN_CI,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
    );
    assert.deepEqual(all, []);
  });

  it("classifies CI failures as agent-resolvable", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Run Unit Tests"],
    };
    const { agent, human } = computeBlockers(
      ci,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
    );
    assert.ok(agent.some((b) => b.includes("ci_fail")));
    assert.equal(human.length, 0);
  });

  it("classifies pending_human_review as human-dependent", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      pending_reviews: { bots: [], humans: ["review-team"] },
    };
    const { agent, human } = computeBlockers(
      CLEAN_CI,
      reviews,
      CLEAN_STATE,
      "pass",
    );
    assert.equal(agent.length, 0);
    assert.ok(human.some((b) => b.includes("pending_human_review")));
  });

  it("classifies not_approved as human-dependent", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "REVIEW_REQUIRED",
    };
    const { agent, human } = computeBlockers(
      CLEAN_CI,
      reviews,
      CLEAN_STATE,
      "pass",
    );
    assert.equal(agent.length, 0);
    assert.ok(human.includes("not_approved"));
  });

  it("does not block when no review policy (NONE)", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "NONE",
    };
    const { all } = computeBlockers(CLEAN_CI, reviews, CLEAN_STATE, "pass");
    assert.ok(!all.includes("not_approved"));
  });

  it("classifies missing required checks as agent-resolvable", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      missing: 1,
      missing_names: ["Mergeability Check"],
    };
    const { agent, human } = computeBlockers(
      ci,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
    );
    assert.ok(agent.some((b) => b.includes("ci_missing")));
    assert.equal(human.length, 0);
  });

  it("classifies unresolved threads as agent-resolvable", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      threads_unresolved: 3,
    };
    const { agent } = computeBlockers(CLEAN_CI, reviews, CLEAN_STATE, "pass");
    assert.ok(agent.some((b) => b.includes("3_unresolved_threads")));
  });

  it("classifies merge conflict as agent-resolvable", () => {
    const state: PullRequestState = {
      ...CLEAN_STATE,
      conflict: "CONFLICTING",
    };
    const { agent } = computeBlockers(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
    );
    assert.ok(agent.includes("merge_conflict"));
  });

  it("classifies draft as agent-resolvable", () => {
    const state: PullRequestState = { ...CLEAN_STATE, draft: true };
    const { agent } = computeBlockers(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
    );
    assert.ok(agent.includes("draft"));
  });

  it("classifies stack_blocked as structural", () => {
    const { agent, human, structural } = computeBlockers(
      CLEAN_CI,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pending",
    );
    assert.equal(agent.length, 0);
    assert.equal(human.length, 0);
    assert.ok(structural.includes("stack_blocked"));
  });

  it("all = agent ∪ human ∪ structural", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Lint"],
    };
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "CHANGES_REQUESTED",
      threads_unresolved: 2,
      pending_reviews: { bots: [], humans: ["alice"] },
    };
    const state: PullRequestState = { ...CLEAN_STATE, draft: true };
    const { agent, human, structural, all } = computeBlockers(
      ci,
      reviews,
      state,
      "pending",
    );
    assert.ok(agent.length >= 3); // ci_fail, unresolved, draft
    assert.ok(human.length >= 2); // not_approved, pending_human
    assert.ok(structural.includes("stack_blocked"));
    assert.equal(all.length, agent.length + human.length + structural.length);
  });
});
