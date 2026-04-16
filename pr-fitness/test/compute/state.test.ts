import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeState } from "../../src/compute/state.js";
import { makePr } from "../fixtures/helpers.js";

describe("computeState", () => {
  it("computes a clean state", () => {
    const state = computeState(makePr());
    assert.equal(state.draft, false);
    assert.equal(state.wip, false);
    assert.equal(state.conflict, "MERGEABLE");
    assert.equal(state.body, true);
    assert.equal(state.summary, true);
    assert.equal(state.test_plan, true);
    assert.equal(state.content_label, true);
    assert.equal(state.assignees, 1);
    assert.equal(state.reviewers, 1);
    assert.equal(state.commits, 1);
    assert.equal(state.behind, false);
    assert.equal(state.updated_at, "2026-03-30T08:00:00Z");
    assert.equal(state.last_commit_at, null);
  });

  it("detects BEHIND mergeStateStatus", () => {
    const state = computeState(makePr({ mergeStateStatus: "BEHIND" }));
    assert.equal(state.behind, true);
  });

  it("reports behind=false for other mergeStateStatus values", () => {
    const others = [
      "CLEAN",
      "UNSTABLE",
      "BLOCKED",
      "DIRTY",
      "DRAFT",
      "HAS_HOOKS",
      "UNKNOWN",
    ] as const;
    for (const status of others) {
      const state = computeState(makePr({ mergeStateStatus: status }));
      assert.equal(state.behind, false, `status=${status}`);
    }
  });

  it("includes last_commit_at when provided", () => {
    const state = computeState(makePr(), "2026-03-30T07:00:00Z");
    assert.equal(state.last_commit_at, "2026-03-30T07:00:00Z");
  });

  it("detects title too long", () => {
    // "A".repeat(48) + " (#100)" = 55 chars
    const state = computeState(makePr({ title: "A".repeat(48) }));
    assert.equal(state.title_ok, false);
    assert.ok(state.title_len > 50);
  });

  it("detects title within limit", () => {
    // "Short" + " (#100)" = 12 chars
    const state = computeState(makePr({ title: "Short" }));
    assert.equal(state.title_ok, true);
  });

  it("detects WIP label", () => {
    const state = computeState(
      makePr({ labels: [{ name: "work in progress" }] }),
    );
    assert.equal(state.wip, true);
  });

  it("detects merge-when-ready label", () => {
    const state = computeState(
      makePr({ labels: [{ name: "merge-when-ready" }, { name: "bug" }] }),
    );
    assert.equal(state.merge_when_ready, true);
    assert.equal(state.content_label, true);
  });

  it("detects missing body", () => {
    const state = computeState(makePr({ body: null }));
    assert.equal(state.body, false);
    assert.equal(state.summary, false);
    assert.equal(state.test_plan, false);
  });

  it("detects missing summary section", () => {
    const state = computeState(makePr({ body: "Just some text" }));
    assert.equal(state.body, true);
    assert.equal(state.summary, false);
  });

  it("detects draft", () => {
    const state = computeState(makePr({ isDraft: true }));
    assert.equal(state.draft, true);
  });

  it("detects conflict", () => {
    const state = computeState(makePr({ mergeable: "CONFLICTING" }));
    assert.equal(state.conflict, "CONFLICTING");
  });

  it("counts multiple commits", () => {
    const state = computeState(
      makePr({ commits: [{ oid: "a" }, { oid: "b" }, { oid: "c" }] }),
    );
    assert.equal(state.commits, 3);
  });

  it("detects enhancement label", () => {
    const state = computeState(makePr({ labels: [{ name: "enhancement" }] }));
    assert.equal(state.content_label, true);
  });

  it("detects no content label", () => {
    const state = computeState(makePr({ labels: [{ name: "rust" }] }));
    assert.equal(state.content_label, false);
  });
});
