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
    const blockers = computeBlockers(
      CLEAN_CI,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
    );
    assert.deepEqual(blockers, []);
  });

  it("reports CI failures", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Run Unit Tests"],
    };
    const blockers = computeBlockers(ci, APPROVED_REVIEWS, CLEAN_STATE, "pass");
    assert.ok(blockers.some((b) => b.includes("ci_fail")));
    assert.ok(blockers.some((b) => b.includes("Run Unit Tests")));
  });

  it("reports CI pending", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      pending: 2,
      pending_names: ["E2E Tests", "Lint"],
    };
    const blockers = computeBlockers(ci, APPROVED_REVIEWS, CLEAN_STATE, "pass");
    assert.ok(blockers.some((b) => b.includes("ci_pending")));
  });

  it("reports not approved", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "REVIEW_REQUIRED",
    };
    const blockers = computeBlockers(CLEAN_CI, reviews, CLEAN_STATE, "pass");
    assert.ok(blockers.includes("not_approved"));
  });

  it("does not block when no review policy (NONE)", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "NONE",
    };
    const blockers = computeBlockers(CLEAN_CI, reviews, CLEAN_STATE, "pass");
    assert.ok(!blockers.includes("not_approved"));
  });

  it("reports unresolved threads", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      threads_unresolved: 3,
    };
    const blockers = computeBlockers(CLEAN_CI, reviews, CLEAN_STATE, "pass");
    assert.ok(blockers.some((b) => b.includes("3_unresolved_threads")));
  });

  it("reports merge conflict", () => {
    const state: PullRequestState = { ...CLEAN_STATE, conflict: "CONFLICTING" };
    const blockers = computeBlockers(CLEAN_CI, APPROVED_REVIEWS, state, "pass");
    assert.ok(blockers.includes("merge_conflict"));
  });

  it("reports draft", () => {
    const state: PullRequestState = { ...CLEAN_STATE, draft: true };
    const blockers = computeBlockers(CLEAN_CI, APPROVED_REVIEWS, state, "pass");
    assert.ok(blockers.includes("draft"));
  });

  it("reports WIP label", () => {
    const state: PullRequestState = { ...CLEAN_STATE, wip: true };
    const blockers = computeBlockers(CLEAN_CI, APPROVED_REVIEWS, state, "pass");
    assert.ok(blockers.includes("wip_label"));
  });

  it("reports title too long", () => {
    const state: PullRequestState = {
      ...CLEAN_STATE,
      title_len: 55,
      title_ok: false,
    };
    const blockers = computeBlockers(CLEAN_CI, APPROVED_REVIEWS, state, "pass");
    assert.ok(blockers.includes("title_too_long"));
  });

  it("reports multiple blockers simultaneously", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Lint"],
    };
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "CHANGES_REQUESTED",
      threads_unresolved: 2,
    };
    const state: PullRequestState = { ...CLEAN_STATE, draft: true };
    const blockers = computeBlockers(ci, reviews, state, "pass");
    assert.ok(blockers.length >= 4);
    assert.ok(blockers.some((b) => b.includes("ci_fail")));
    assert.ok(blockers.some((b) => b.includes("not_approved")));
    assert.ok(blockers.some((b) => b.includes("unresolved_threads")));
    assert.ok(blockers.includes("draft"));
  });
});
