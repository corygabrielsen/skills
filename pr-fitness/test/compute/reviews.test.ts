import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeReviews } from "../../src/compute/reviews.js";
import type { GhReview } from "../../src/types/input.js";
import { HEAD, makePr, makeThreads } from "../fixtures/helpers.js";

describe("computeReviews", () => {
  it("counts resolved and unresolved threads", () => {
    const result = computeReviews(
      makePr(),
      makeThreads([true, false, true, false, false]),
      [],
      [],
    );
    assert.equal(result.threads_total, 5);
    assert.equal(result.threads_unresolved, 3);
  });

  it("counts bot comments", () => {
    const comments = [
      { id: 1, login: "claude[bot]" },
      { id: 2, login: "copilot[bot]" },
      { id: 3, login: "corygabrielsen" },
    ];
    const result = computeReviews(makePr(), makeThreads([]), comments, []);
    assert.equal(result.bot_comments, 2);
  });

  it("detects approvals on current HEAD", () => {
    const reviews: GhReview[] = [
      { state: "APPROVED", commit_id: HEAD },
      { state: "APPROVED", commit_id: "old_sha" },
      { state: "COMMENTED", commit_id: HEAD },
    ];
    const result = computeReviews(makePr(), makeThreads([]), [], reviews);
    assert.equal(result.approvals_on_head, 1);
    assert.equal(result.approvals_stale, 1);
  });

  it("maps empty reviewDecision to NONE (no review policy)", () => {
    const result = computeReviews(
      makePr({ reviewDecision: "" }),
      makeThreads([]),
      [],
      [],
    );
    assert.equal(result.decision, "NONE");
  });

  it("maps null reviewDecision to NONE (no review policy)", () => {
    const result = computeReviews(
      makePr({ reviewDecision: null }),
      makeThreads([]),
      [],
      [],
    );
    assert.equal(result.decision, "NONE");
  });
});
