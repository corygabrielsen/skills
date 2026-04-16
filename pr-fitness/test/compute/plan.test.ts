import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { plan } from "../../src/compute/plan.js";
import type {
  CiSummary,
  PullRequestState,
  ReviewSummary,
} from "../../src/types/output.js";
import {
  APPROVED_REVIEWS,
  CLEAN_CI,
  CLEAN_STATE,
  UNCONFIGURED_COPILOT,
} from "../fixtures/helpers.js";

describe("plan", () => {
  it("returns empty for a fully mergeable PR", () => {
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.equal(actions.length, 0);
  });

  it("prescribes wait for pending CI", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      pending: 2,
      pending_names: ["Build", "Test"],
    };
    const actions = plan(
      ci,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    const waits = actions.filter((a) => a.type.kind === "wait_for_ci");
    assert.equal(waits.length, 1);
    assert.equal(waits[0]!.automation, "wait");
  });

  it("prescribes fix_ci for failures", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 2,
      failed: ["Lint", "Unit Tests"],
    };
    const actions = plan(
      ci,
      APPROVED_REVIEWS,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    const fixes = actions.filter((a) => a.type.kind === "fix_ci");
    assert.equal(fixes.length, 2);
    assert.equal(fixes[0]!.automation, "llm");
  });

  it("prescribes rebase for conflicts", () => {
    const state: PullRequestState = { ...CLEAN_STATE, conflict: "CONFLICTING" };
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(actions.some((a) => a.type.kind === "rebase"));
  });

  it("prescribes mark_ready for drafts", () => {
    const state: PullRequestState = { ...CLEAN_STATE, draft: true };
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    const a = actions.find((a) => a.type.kind === "mark_ready");
    assert.ok(a);
    assert.equal(a.automation, "full");
  });

  it("prescribes remove_wip_label", () => {
    const state: PullRequestState = { ...CLEAN_STATE, wip: true };
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(actions.some((a) => a.type.kind === "remove_wip_label"));
  });

  it("prescribes shorten_title", () => {
    const state: PullRequestState = {
      ...CLEAN_STATE,
      title_len: 55,
      title_ok: false,
    };
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    const a = actions.find((a) => a.type.kind === "shorten_title");
    assert.ok(a);
    assert.equal(a.automation, "llm");
  });

  it("prescribes address_threads", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      threads_unresolved: 3,
    };
    const actions = plan(
      CLEAN_CI,
      reviews,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    const a = actions.find((a) => a.type.kind === "address_threads");
    assert.ok(a);
    assert.equal(a.automation, "llm");
    assert.equal(a.type.kind === "address_threads" && a.type.count, 3);
  });

  it("prescribes request_approval when CI and reviews are clean", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "REVIEW_REQUIRED",
    };
    const actions = plan(
      CLEAN_CI,
      reviews,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(actions.some((a) => a.type.kind === "request_approval"));
  });

  it("omits request_approval when other blockers exist", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Lint"],
    };
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "REVIEW_REQUIRED",
    };
    const actions = plan(
      ci,
      reviews,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(!actions.some((a) => a.type.kind === "request_approval"));
  });

  it("omits approval actions when no review policy (NONE)", () => {
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      decision: "NONE",
    };
    const actions = plan(
      CLEAN_CI,
      reviews,
      CLEAN_STATE,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(!actions.some((a) => a.type.kind === "request_approval"));
    assert.ok(!actions.some((a) => a.type.kind === "add_reviewer"));
  });

  it("prescribes metadata actions for missing hygiene", () => {
    const state: PullRequestState = {
      ...CLEAN_STATE,
      content_label: false,
      assignees: 0,
      body: false,
    };
    const actions = plan(
      CLEAN_CI,
      APPROVED_REVIEWS,
      state,
      "pass",
      UNCONFIGURED_COPILOT,
    );
    assert.ok(actions.some((a) => a.type.kind === "add_content_label"));
    assert.ok(actions.some((a) => a.type.kind === "add_assignee"));
    assert.ok(actions.some((a) => a.type.kind === "add_description"));
  });

  it("orders CI before reviews before metadata", () => {
    const ci: CiSummary = {
      ...CLEAN_CI,
      fail: 1,
      failed: ["Lint"],
    };
    const reviews: ReviewSummary = {
      ...APPROVED_REVIEWS,
      threads_unresolved: 1,
    };
    const state: PullRequestState = { ...CLEAN_STATE, content_label: false };
    const actions = plan(ci, reviews, state, "pass", UNCONFIGURED_COPILOT);

    const kinds = actions.map((a) => a.type.kind);
    const ciIdx = kinds.indexOf("fix_ci");
    const threadIdx = kinds.indexOf("address_threads");
    const labelIdx = kinds.indexOf("add_content_label");

    assert.ok(ciIdx < threadIdx, "CI should come before reviews");
    assert.ok(threadIdx < labelIdx, "reviews should come before metadata");
  });
});
