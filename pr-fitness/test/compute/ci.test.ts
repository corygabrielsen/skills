import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeCi } from "../../src/compute/ci.js";
import type { GitHubCheck } from "../../src/types/input.js";
import type { RequiredCheckConfig } from "../../src/types/input.js";
import { makeCheck } from "../fixtures/helpers.js";

/** Config entries for all observed checks — everything is required. */
function allRequired(checks: readonly GitHubCheck[]): RequiredCheckConfig[] {
  return checks.map((c) => ({ context: c.name, integration_id: 0 }));
}

/** Config entries for specific check names. */
function requiredConfigs(...names: string[]): RequiredCheckConfig[] {
  return names.map((n) => ({ context: n, integration_id: 0 }));
}

describe("computeCi — required counts", () => {
  it("counts empty checks as all zeros", () => {
    const result = computeCi([], []);
    assert.equal(result.pass, 0);
    assert.equal(result.fail, 0);
    assert.equal(result.pending, 0);
    assert.equal(result.missing, 0);
    assert.equal(result.total, 0);
  });

  it("counts pass states", () => {
    const checks = [
      makeCheck("A", "SUCCESS"),
      makeCheck("B", "SKIPPED"),
      makeCheck("C", "NEUTRAL"),
    ];
    const result = computeCi(checks, allRequired(checks));
    assert.equal(result.pass, 3);
    assert.equal(result.fail, 0);
    assert.equal(result.pending, 0);
    assert.equal(result.missing, 0);
    assert.equal(result.total, 3);
  });

  it("collects failed check names", () => {
    const checks = [
      makeCheck("Unit Tests", "FAILURE"),
      makeCheck("Lint", "SUCCESS"),
      makeCheck("E2E", "FAILURE"),
    ];
    const result = computeCi(checks, allRequired(checks));
    assert.equal(result.fail, 2);
    assert.deepEqual(result.failed, ["Unit Tests", "E2E"]);
  });

  it("includes failed_details with description and link", () => {
    const checks: GitHubCheck[] = [
      {
        name: "Lint",
        state: "FAILURE",
        description: "3 errors found",
        link: "https://github.com/example/widgets/actions/runs/123",
        completedAt: "2026-03-30T08:00:00Z",
      },
      makeCheck("Build", "SUCCESS"),
    ];
    const result = computeCi(checks, allRequired(checks));
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
    const result = computeCi(checks, allRequired(checks));
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
    const result = computeCi(checks, allRequired(checks));
    assert.equal(result.pass, 3);
    assert.equal(result.fail, 1);
    assert.equal(result.pending, 2);
    assert.equal(result.missing, 0);
    assert.equal(result.total, 6);
  });

  it("excludes Graphite mergeability check from counts", () => {
    const checks = [
      makeCheck("Lint", "SUCCESS"),
      makeCheck("Graphite / mergeability_check", "IN_PROGRESS"),
    ];
    const result = computeCi(checks, requiredConfigs("Lint"));
    assert.equal(result.total, 1);
    assert.equal(result.pending, 0);
    assert.equal(result.advisory.total, 0);
  });

  it("excludes Graphite from configured required set", () => {
    const checks = [
      makeCheck("Lint", "SUCCESS"),
      makeCheck("Graphite / mergeability_check", "IN_PROGRESS"),
    ];
    // Even if Graphite is in the configured required set, it's filtered out
    const result = computeCi(
      checks,
      requiredConfigs("Lint", "Graphite / mergeability_check"),
    );
    assert.equal(result.total, 1);
    assert.equal(result.missing, 0);
    assert.equal(result.pending, 0);
  });

  it("tracks most recent completed_at across all checks", () => {
    const checks: GitHubCheck[] = [
      { ...makeCheck("A", "SUCCESS"), completedAt: "2026-03-30T08:00:00Z" },
      { ...makeCheck("B", "SUCCESS"), completedAt: "2026-03-30T09:00:00Z" },
      { ...makeCheck("C", "FAILURE"), completedAt: "2026-03-30T08:30:00Z" },
    ];
    const result = computeCi(checks, allRequired(checks));
    assert.equal(result.completed_at, "2026-03-30T09:00:00Z");
  });

  it("returns null completed_at when no checks have completed", () => {
    const checks = [
      makeCheck("Build", "IN_PROGRESS"),
      makeCheck("Test", "QUEUED"),
    ];
    const result = computeCi(checks, allRequired(checks));
    assert.equal(result.completed_at, null);
  });

  it("returns null completed_at for empty checks", () => {
    const result = computeCi([], []);
    assert.equal(result.completed_at, null);
  });
});

describe("computeCi — required vs advisory split", () => {
  it("routes non-required failing checks to advisory, keeps required counts clean", () => {
    const checks = [
      makeCheck("Unit Tests", "SUCCESS"),
      makeCheck("Mergeability Check", "SUCCESS"),
      makeCheck("Run Claude Code Review", "FAILURE"),
    ];
    const result = computeCi(
      checks,
      requiredConfigs("Unit Tests", "Mergeability Check"),
    );
    assert.equal(result.fail, 0);
    assert.deepEqual(result.failed, []);
    assert.equal(result.advisory.fail, 1);
    assert.deepEqual(result.advisory.failed, ["Run Claude Code Review"]);
    assert.equal(result.advisory.failed_details.length, 1);
  });

  it("treats every check as advisory when required list is empty", () => {
    const checks = [
      makeCheck("Unit Tests", "SUCCESS"),
      makeCheck("Lint", "FAILURE"),
    ];
    const result = computeCi(checks, []);
    assert.equal(result.total, 0);
    assert.equal(result.fail, 0);
    assert.equal(result.missing, 0);
    assert.equal(result.advisory.total, 2);
    assert.equal(result.advisory.fail, 1);
    assert.deepEqual(result.advisory.failed, ["Lint"]);
  });

  it("keeps pending split too: required pending vs advisory pending", () => {
    const checks = [
      makeCheck("Unit Tests", "IN_PROGRESS"),
      makeCheck("Optional E2E", "QUEUED"),
    ];
    const result = computeCi(checks, requiredConfigs("Unit Tests"));
    assert.deepEqual(result.pending_names, ["Unit Tests"]);
    assert.deepEqual(result.advisory.pending_names, ["Optional E2E"]);
  });
});

describe("computeCi — missing checks (join model)", () => {
  it("detects configured required check with no observed check-run", () => {
    const checks = [makeCheck("Lint", "SUCCESS")];
    const result = computeCi(
      checks,
      requiredConfigs("Lint", "Mergeability Check"),
    );
    assert.equal(result.pass, 1);
    assert.equal(result.missing, 1);
    assert.deepEqual(result.missing_names, ["Mergeability Check"]);
    assert.equal(result.total, 1); // total counts observed only
  });

  it("all required missing when no check-runs exist", () => {
    const result = computeCi(
      [],
      requiredConfigs("Mergeability Check", "Build"),
    );
    assert.equal(result.pass, 0);
    assert.equal(result.fail, 0);
    assert.equal(result.pending, 0);
    assert.equal(result.missing, 2);
    assert.deepEqual(result.missing_names, ["Mergeability Check", "Build"]);
    assert.equal(result.total, 0);
  });

  it("missing check does not appear in advisory", () => {
    const checks = [makeCheck("Advisory Check", "SUCCESS")];
    const result = computeCi(checks, requiredConfigs("Required Check"));
    assert.equal(result.missing, 1);
    assert.equal(result.advisory.total, 1);
    assert.equal(result.advisory.pass, 1);
  });

  it("mixed: some present, some missing, some advisory", () => {
    const checks = [
      makeCheck("Lint", "SUCCESS"),
      makeCheck("Build", "FAILURE"),
      makeCheck("Deploy Preview", "IN_PROGRESS"),
    ];
    const result = computeCi(
      checks,
      requiredConfigs("Lint", "Build", "Mergeability Check"),
    );
    assert.equal(result.pass, 1);
    assert.equal(result.fail, 1);
    assert.equal(result.missing, 1);
    assert.deepEqual(result.missing_names, ["Mergeability Check"]);
    assert.equal(result.advisory.total, 1); // Deploy Preview
    assert.equal(result.advisory.pending, 1);
  });
});
