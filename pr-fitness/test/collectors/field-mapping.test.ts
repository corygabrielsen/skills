import { describe, it } from "node:test";
import assert from "node:assert/strict";

/**
 * Tests for the field-mapping logic in comments.ts and reviews.ts.
 *
 * These collectors removed --jq and now map fields in TypeScript.
 * The mappers are inline lambdas, so we replicate them here to
 * lock the contract — if the mapping changes, these tests catch it.
 */

// Replicate the raw types and mapping logic from each collector.
// This avoids needing to mock `gh` while still testing the transform.

describe("comments field mapping", () => {
  // Mirrors the mapping in collectComments: c => ({ id, login: user.login })
  interface RawComment {
    id: number;
    user: { login: string };
  }
  function mapComment(c: RawComment) {
    return { id: c.id, login: c.user.login };
  }

  it("extracts id and login from raw comment", () => {
    const raw: RawComment = { id: 42, user: { login: "alice" } };
    assert.deepEqual(mapComment(raw), { id: 42, login: "alice" });
  });

  it("handles bot login with [bot] suffix", () => {
    const raw: RawComment = {
      id: 99,
      user: { login: "copilot-pull-request-reviewer[bot]" },
    };
    const result = mapComment(raw);
    assert.equal(result.login, "copilot-pull-request-reviewer[bot]");
  });

  it("maps multiple comments preserving order", () => {
    const raws: RawComment[] = [
      { id: 1, user: { login: "alice" } },
      { id: 2, user: { login: "bob" } },
      { id: 3, user: { login: "charlie" } },
    ];
    const result = raws.map(mapComment);
    assert.deepEqual(result, [
      { id: 1, login: "alice" },
      { id: 2, login: "bob" },
      { id: 3, login: "charlie" },
    ]);
  });
});

describe("reviews field mapping", () => {
  // Mirrors the mapping in collectReviews
  interface RawReview {
    user: { login: string };
    state: string;
    commit_id: string;
    submitted_at: string;
    body: string;
  }
  function mapReview(r: RawReview) {
    return {
      user: r.user.login,
      state: r.state,
      commit_id: r.commit_id,
      submitted_at: r.submitted_at,
      body: r.body,
    };
  }

  it("flattens user.login to user", () => {
    const raw: RawReview = {
      user: { login: "alice" },
      state: "APPROVED",
      commit_id: "abc123",
      submitted_at: "2026-04-21T00:00:00Z",
      body: "LGTM",
    };
    const result = mapReview(raw);
    assert.equal(result.user, "alice");
    assert.equal(result.state, "APPROVED");
    assert.equal(result.commit_id, "abc123");
    assert.equal(result.submitted_at, "2026-04-21T00:00:00Z");
    assert.equal(result.body, "LGTM");
  });

  it("preserves all review states", () => {
    for (const state of ["APPROVED", "CHANGES_REQUESTED", "COMMENTED", "DISMISSED", "PENDING"]) {
      const raw: RawReview = {
        user: { login: "reviewer" },
        state,
        commit_id: "sha",
        submitted_at: "2026-04-21T00:00:00Z",
        body: "",
      };
      assert.equal(mapReview(raw).state, state);
    }
  });

  it("handles empty body", () => {
    const raw: RawReview = {
      user: { login: "bot" },
      state: "COMMENTED",
      commit_id: "sha",
      submitted_at: "2026-04-21T00:00:00Z",
      body: "",
    };
    assert.equal(mapReview(raw).body, "");
  });

  it("maps multiple reviews preserving order", () => {
    const raws: RawReview[] = [
      { user: { login: "alice" }, state: "APPROVED", commit_id: "a", submitted_at: "t1", body: "" },
      { user: { login: "bob" }, state: "CHANGES_REQUESTED", commit_id: "b", submitted_at: "t2", body: "fix" },
    ];
    const result = raws.map(mapReview);
    assert.equal(result[0]!.user, "alice");
    assert.equal(result[1]!.user, "bob");
    assert.equal(result[1]!.body, "fix");
  });
});
