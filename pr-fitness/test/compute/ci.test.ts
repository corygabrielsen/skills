import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeCi } from "../../src/compute/ci.js";
import type { GhCheck } from "../../src/types/input.js";
import { makeCheck } from "../fixtures/helpers.js";

describe("computeCi", () => {
  it("counts empty checks as all zeros", () => {
    const result = computeCi([]);
    assert.equal(result.pass, 0);
    assert.equal(result.fail, 0);
    assert.equal(result.pending, 0);
    assert.equal(result.total, 0);
  });

  it("counts pass states", () => {
    const checks = [
      makeCheck("A", "SUCCESS"),
      makeCheck("B", "SKIPPED"),
      makeCheck("C", "NEUTRAL"),
    ];
    const result = computeCi(checks);
    assert.equal(result.pass, 3);
    assert.equal(result.fail, 0);
    assert.equal(result.pending, 0);
    assert.equal(result.total, 3);
  });

  it("collects failed check names", () => {
    const checks = [
      makeCheck("Unit Tests", "FAILURE"),
      makeCheck("Lint", "SUCCESS"),
      makeCheck("E2E", "FAILURE"),
    ];
    const result = computeCi(checks);
    assert.equal(result.fail, 2);
    assert.deepEqual(result.failed, ["Unit Tests", "E2E"]);
  });

  it("includes failed_details with description and link", () => {
    const checks: GhCheck[] = [
      {
        name: "Lint",
        state: "FAILURE",
        description: "3 errors found",
        link: "https://github.com/example/widgets/actions/runs/123",
        completedAt: "2026-03-30T08:00:00Z",
      },
      makeCheck("Build", "SUCCESS"),
    ];
    const result = computeCi(checks);
    assert.equal(result.failed_details.length, 1);
    assert.equal(result.failed_details[0]!.name, "Lint");
    assert.equal(result.failed_details[0]!.description, "3 errors found");
    assert.ok(result.failed_details[0]!.link.includes("actions/runs"));
  });

  it("collects pending check names", () => {
    const checks = [
      makeCheck("Build", "IN_PROGRESS"),
      makeCheck("Deploy", "QUEUED"),
      makeCheck("Lint", "SUCCESS"),
    ];
    const result = computeCi(checks);
    assert.equal(result.pending, 2);
    assert.deepEqual(result.pending_names, ["Build", "Deploy"]);
  });

  it("handles mixed states", () => {
    const checks = [
      makeCheck("A", "SUCCESS"),
      makeCheck("B", "FAILURE"),
      makeCheck("C", "IN_PROGRESS"),
      makeCheck("D", "SKIPPED"),
      makeCheck("E", "QUEUED"),
      makeCheck("F", "NEUTRAL"),
    ];
    const result = computeCi(checks);
    assert.equal(result.pass, 3);
    assert.equal(result.fail, 1);
    assert.equal(result.pending, 2);
    assert.equal(result.total, 6);
  });

  it("excludes Graphite mergeability check from counts", () => {
    const checks = [
      makeCheck("Lint", "SUCCESS"),
      makeCheck("Graphite / mergeability_check", "IN_PROGRESS"),
    ];
    const result = computeCi(checks);
    assert.equal(result.total, 1);
    assert.equal(result.pending, 0);
  });

  it("tracks most recent completed_at across all checks", () => {
    const checks: GhCheck[] = [
      { ...makeCheck("A", "SUCCESS"), completedAt: "2026-03-30T08:00:00Z" },
      { ...makeCheck("B", "SUCCESS"), completedAt: "2026-03-30T09:00:00Z" },
      { ...makeCheck("C", "FAILURE"), completedAt: "2026-03-30T08:30:00Z" },
    ];
    const result = computeCi(checks);
    assert.equal(result.completed_at, "2026-03-30T09:00:00Z");
  });

  it("returns null completed_at when no checks have completed", () => {
    const checks = [
      makeCheck("Build", "IN_PROGRESS"),
      makeCheck("Test", "QUEUED"),
    ];
    const result = computeCi(checks);
    assert.equal(result.completed_at, null);
  });

  it("returns null completed_at for empty checks", () => {
    const result = computeCi([]);
    assert.equal(result.completed_at, null);
  });
});
