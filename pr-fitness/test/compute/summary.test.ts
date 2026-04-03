import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { summarize } from "../../src/compute/summary.js";

describe("summarize", () => {
  it("reports ready to merge for open PR with no blockers", () => {
    assert.equal(summarize("open", [], null), "Ready to merge");
  });

  it("lists blockers for open PR", () => {
    const result = summarize("open", ["ci_fail: Lint", "not_approved"], null);
    assert.equal(result, "Blocked: ci_fail: Lint, not_approved");
  });

  it("reports merged with timestamp", () => {
    assert.equal(
      summarize("merged", [], "2026-03-30T16:17:24Z"),
      "Merged 2026-03-30T16:17:24Z",
    );
  });

  it("reports merged without timestamp", () => {
    assert.equal(summarize("merged", [], null), "Merged");
  });

  it("reports closed (not merged)", () => {
    assert.equal(summarize("closed", [], null), "Closed (not merged)");
  });
});
