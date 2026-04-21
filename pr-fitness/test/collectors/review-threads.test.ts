import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  paginateThreads,
  type FetchPage,
  type ThreadsPage,
} from "../../src/collectors/review-threads.js";

/** Build a ThreadsPage with N threads, optional cursor for next page. */
function makePage(
  threads: { isResolved: boolean }[],
  nextCursor: string | null,
  reviewRequests: ThreadsPage["data"]["repository"]["pullRequest"]["reviewRequests"] = {
    nodes: [],
  },
): ThreadsPage {
  return {
    data: {
      repository: {
        pullRequest: {
          reviewThreads: {
            pageInfo: {
              hasNextPage: nextCursor !== null,
              endCursor: nextCursor,
            },
            nodes: threads.map((t) => ({
              isResolved: t.isResolved,
              comments: {
                nodes: [
                  { author: { login: "user" }, createdAt: "2026-04-21T00:00:00Z" },
                ],
              },
            })),
          },
          reviewRequests,
        },
      },
    },
  };
}

describe("paginateThreads", () => {
  it("returns single page when hasNextPage is false", async () => {
    const page = makePage(
      [{ isResolved: true }, { isResolved: false }],
      null,
    );
    const fetchPage: FetchPage = async (cursor) => {
      assert.equal(cursor, null);
      return page;
    };

    const result = await paginateThreads(fetchPage);
    const nodes = result.data.repository.pullRequest.reviewThreads.nodes;
    assert.equal(nodes.length, 2);
    assert.equal(nodes[0]!.isResolved, true);
    assert.equal(nodes[1]!.isResolved, false);
  });

  it("merges two pages of threads", async () => {
    const pages: ThreadsPage[] = [
      makePage(
        [{ isResolved: true }, { isResolved: true }],
        "cursor-1",
      ),
      makePage(
        [{ isResolved: false }],
        null,
      ),
    ];
    let callIndex = 0;
    const fetchPage: FetchPage = async (cursor) => {
      if (callIndex === 0) assert.equal(cursor, null);
      if (callIndex === 1) assert.equal(cursor, "cursor-1");
      return pages[callIndex++]!;
    };

    const result = await paginateThreads(fetchPage);
    const nodes = result.data.repository.pullRequest.reviewThreads.nodes;
    assert.equal(nodes.length, 3);
    assert.equal(nodes[0]!.isResolved, true);
    assert.equal(nodes[1]!.isResolved, true);
    assert.equal(nodes[2]!.isResolved, false);
  });

  it("merges three pages (>200 threads scenario)", async () => {
    const pages: ThreadsPage[] = [
      makePage(Array(100).fill({ isResolved: true }), "c1"),
      makePage(Array(100).fill({ isResolved: true }), "c2"),
      makePage(
        [{ isResolved: false }, { isResolved: false }, { isResolved: false }],
        null,
      ),
    ];
    let callIndex = 0;
    const fetchPage: FetchPage = async () => pages[callIndex++]!;

    const result = await paginateThreads(fetchPage);
    const nodes = result.data.repository.pullRequest.reviewThreads.nodes;
    assert.equal(nodes.length, 203);
    assert.equal(
      nodes.filter((n) => !n.isResolved).length,
      3,
    );
  });

  it("takes reviewRequests from first page only", async () => {
    const page1Requests = {
      nodes: [
        {
          requestedReviewer: {
            __typename: "Bot" as const,
            login: "copilot-pull-request-reviewer",
          },
        },
      ],
    };
    const page2Requests = {
      nodes: [
        {
          requestedReviewer: {
            __typename: "User" as const,
            login: "someone-else",
          },
        },
      ],
    };

    const pages: ThreadsPage[] = [
      makePage([{ isResolved: true }], "c1", page1Requests),
      makePage([{ isResolved: false }], null, page2Requests),
    ];
    let callIndex = 0;
    const fetchPage: FetchPage = async () => pages[callIndex++]!;

    const result = await paginateThreads(fetchPage);
    const requests = result.data.repository.pullRequest.reviewRequests.nodes;
    assert.equal(requests.length, 1);
    assert.equal(requests[0]!.requestedReviewer!.login, "copilot-pull-request-reviewer");
  });

  it("passes correct cursor to each fetch call", async () => {
    const cursors: (string | null)[] = [];
    const pages: ThreadsPage[] = [
      makePage([{ isResolved: true }], "alpha"),
      makePage([{ isResolved: true }], "beta"),
      makePage([{ isResolved: true }], null),
    ];
    let callIndex = 0;
    const fetchPage: FetchPage = async (cursor) => {
      cursors.push(cursor);
      return pages[callIndex++]!;
    };

    await paginateThreads(fetchPage);
    assert.deepEqual(cursors, [null, "alpha", "beta"]);
  });

  it("handles empty first page", async () => {
    const page = makePage([], null);
    const fetchPage: FetchPage = async () => page;

    const result = await paginateThreads(fetchPage);
    assert.equal(
      result.data.repository.pullRequest.reviewThreads.nodes.length,
      0,
    );
  });
});
