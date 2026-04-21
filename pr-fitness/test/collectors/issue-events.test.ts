import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  shapeEvent,
  type RawIssueEvent,
} from "../../src/collectors/issue-events.js";

describe("shapeEvent", () => {
  it("shapes a basic event with actor", () => {
    const raw: RawIssueEvent = {
      event: "labeled",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
    };
    const result = shapeEvent(raw);
    assert.equal(result.event, "labeled");
    assert.deepEqual(result.actor, { login: "cory" });
    assert.equal(result.created_at, "2026-04-21T00:00:00Z");
  });

  it("handles null actor", () => {
    const raw: RawIssueEvent = {
      event: "closed",
      actor: null,
      created_at: "2026-04-21T00:00:00Z",
    };
    const result = shapeEvent(raw);
    assert.equal(result.actor, null);
  });

  it("shapes review_requested with user reviewer", () => {
    const raw: RawIssueEvent = {
      event: "review_requested",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
      requested_reviewer: { login: "alice" },
    };
    const result = shapeEvent(raw);
    assert.equal(result.event, "review_requested");
    assert.ok("requested" in result);
    if ("requested" in result) {
      assert.equal(result.requested.kind, "user");
      assert.ok("login" in result.requested);
      if ("login" in result.requested) {
        assert.equal(result.requested.login, "alice");
      }
    }
  });

  it("shapes review_requested with team reviewer", () => {
    const raw: RawIssueEvent = {
      event: "review_requested",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
      requested_team: { slug: "review-team" },
    };
    const result = shapeEvent(raw);
    assert.equal(result.event, "review_requested");
    assert.ok("requested" in result);
    if ("requested" in result) {
      assert.equal(result.requested.kind, "team");
      assert.ok("slug" in result.requested);
      if ("slug" in result.requested) {
        assert.equal(result.requested.slug, "review-team");
      }
    }
  });

  it("shapes review_request_removed with user reviewer", () => {
    const raw: RawIssueEvent = {
      event: "review_request_removed",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
      requested_reviewer: { login: "bob" },
    };
    const result = shapeEvent(raw);
    assert.equal(result.event, "review_request_removed");
    assert.ok("requested" in result);
    if ("requested" in result) {
      assert.equal(result.requested.kind, "user");
    }
  });

  it("falls back to catchall when review_requested has neither reviewer nor team", () => {
    const raw: RawIssueEvent = {
      event: "review_requested",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
    };
    const result = shapeEvent(raw);
    assert.equal(result.event, "review_requested");
    assert.ok(!("requested" in result));
  });

  it("prefers requested_reviewer over requested_team", () => {
    const raw: RawIssueEvent = {
      event: "review_requested",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
      requested_reviewer: { login: "alice" },
      requested_team: { slug: "review-team" },
    };
    const result = shapeEvent(raw);
    assert.ok("requested" in result);
    if ("requested" in result) {
      assert.equal(result.requested.kind, "user");
    }
  });

  it("does not add requested field to non-review events", () => {
    const raw: RawIssueEvent = {
      event: "assigned",
      actor: { login: "cory" },
      created_at: "2026-04-21T00:00:00Z",
      requested_reviewer: { login: "alice" },
    };
    const result = shapeEvent(raw);
    assert.ok(!("requested" in result));
  });

  it("handles null created_at", () => {
    const raw: RawIssueEvent = {
      event: "labeled",
      actor: { login: "cory" },
      created_at: null,
    };
    const result = shapeEvent(raw);
    assert.equal(result.created_at, null);
  });

  it("strips extra fields from raw actor", () => {
    const raw: RawIssueEvent = {
      event: "labeled",
      actor: { login: "cory" } as { login: string; id?: number },
      created_at: "2026-04-21T00:00:00Z",
    };
    // Assign extra property to verify it's not passed through
    (raw.actor as Record<string, unknown>)["id"] = 12345;
    const result = shapeEvent(raw);
    assert.deepEqual(result.actor, { login: "cory" });
    assert.ok(!("id" in (result.actor ?? {})));
  });
});
