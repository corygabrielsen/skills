import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { computeReviews } from "../../src/compute/reviews.js";
import type { GitHubPullRequestReview } from "../../src/types/input.js";
import { HEAD, makePr, makeReview, makeThreads } from "../fixtures/helpers.js";

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
    const reviews: GitHubPullRequestReview[] = [
      makeReview("APPROVED", HEAD, "alice"),
      makeReview("APPROVED", "old_sha", "bob"),
      makeReview("COMMENTED", HEAD, "charlie"),
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

  it("detects pending bot reviewers from GraphQL", () => {
    const threads = makeThreads(
      [],
      [
        {
          requestedReviewer: {
            __typename: "Bot",
            login: "copilot-pull-request-reviewer",
          },
        },
      ],
    );
    const result = computeReviews(makePr(), threads, [], []);
    assert.deepEqual(result.pending_reviews.bots, [
      "copilot-pull-request-reviewer",
    ]);
    assert.deepEqual(result.pending_reviews.humans, []);
  });

  it("detects pending human reviewers from GraphQL", () => {
    const threads = makeThreads(
      [],
      [
        {
          requestedReviewer: {
            __typename: "User",
            login: "audieleon",
          },
        },
      ],
    );
    const result = computeReviews(makePr(), threads, [], []);
    assert.deepEqual(result.pending_reviews.bots, []);
    assert.deepEqual(result.pending_reviews.humans, ["audieleon"]);
  });

  it("classifies [bot] suffix as bot even with User typename", () => {
    const threads = makeThreads(
      [],
      [
        {
          requestedReviewer: {
            __typename: "User",
            login: "some-tool[bot]",
          },
        },
      ],
    );
    const result = computeReviews(makePr(), threads, [], []);
    assert.deepEqual(result.pending_reviews.bots, ["some-tool[bot]"]);
  });

  it("skips null requestedReviewer", () => {
    const threads = makeThreads([], [{ requestedReviewer: null }]);
    const result = computeReviews(makePr(), threads, [], []);
    assert.deepEqual(result.pending_reviews.bots, []);
    assert.deepEqual(result.pending_reviews.humans, []);
  });

  it("surfaces submitted bot reviews", () => {
    const reviews: GitHubPullRequestReview[] = [
      makeReview("COMMENTED", HEAD, "copilot-pull-request-reviewer[bot]"),
      makeReview("APPROVED", HEAD, "audieleon"),
      makeReview("COMMENTED", HEAD, "cursor[bot]"),
    ];
    const result = computeReviews(makePr(), makeThreads([]), [], reviews);
    assert.equal(result.bot_reviews.length, 2);
    assert.equal(
      result.bot_reviews[0]!.user,
      "copilot-pull-request-reviewer[bot]",
    );
    assert.equal(result.bot_reviews[1]!.user, "cursor[bot]");
  });

  it("returns empty bot_reviews when no bots reviewed", () => {
    const reviews: GitHubPullRequestReview[] = [
      makeReview("APPROVED", HEAD, "audieleon"),
    ];
    const result = computeReviews(makePr(), makeThreads([]), [], reviews);
    assert.equal(result.bot_reviews.length, 0);
  });
});
