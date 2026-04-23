import type { GitHubPullRequestReviewThreadsResponse } from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/** Minimal valid response that downstream compute functions can handle. */
export const EMPTY_THREADS: GitHubPullRequestReviewThreadsResponse = {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: { nodes: [] },
        reviewRequests: { nodes: [] },
      },
    },
  },
};

/** Single page of the paginated GraphQL response (includes pageInfo). */
export interface ThreadsPage {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: {
          pageInfo: { hasNextPage: boolean; endCursor: string | null };
          nodes: GitHubPullRequestReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"];
        };
        reviewRequests: GitHubPullRequestReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewRequests"];
      };
    };
  };
}

/** Callback that fetches one page of threads given an optional cursor. */
export type FetchPage = (cursor: string | null) => Promise<ThreadsPage>;

/**
 * Paginate reviewThreads pages into a single merged response.
 *
 * Exported for testing — production callers use collectReviewThreads.
 */
export async function paginateThreads(
  fetchPage: FetchPage,
): Promise<GitHubPullRequestReviewThreadsResponse> {
  type ThreadNode = GitHubPullRequestReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][number];
  const allThreads: ThreadNode[] = [];
  let cursor: string | null = null;
  let reviewRequests: GitHubPullRequestReviewThreadsResponse["data"]["repository"]["pullRequest"]["reviewRequests"] | null = null;

  do {
    const page: ThreadsPage = await fetchPage(cursor);
    const pull = page.data.repository.pullRequest;
    allThreads.push(...pull.reviewThreads.nodes);

    if (!reviewRequests) {
      reviewRequests = pull.reviewRequests;
    }

    const pageInfo = pull.reviewThreads.pageInfo;
    cursor = pageInfo.hasNextPage ? pageInfo.endCursor : null;
  } while (cursor);

  return {
    data: {
      repository: {
        pullRequest: {
          reviewThreads: { nodes: allThreads },
          reviewRequests: reviewRequests!,
        },
      },
    },
  };
}

/**
 * Review threads and pending review requests (GraphQL).
 *
 * Paginates reviewThreads via cursor to handle PRs with >100 threads.
 * reviewRequests is fetched once on the first page.
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectReviewThreads(
  owner: string,
  name: string,
  pr: number,
): Promise<GitHubPullRequestReviewThreadsResponse> {
  return paginateThreads(async (cursor) => {
    const afterClause: string = cursor ? `,after:"${cursor}"` : "";
    const result: Awaited<ReturnType<typeof gh<ThreadsPage>>> = await gh<ThreadsPage>([
      "api",
      "graphql",
      "-f",
      `query={
        repository(owner:"${owner}",name:"${name}") {
          pullRequest(number:${String(pr)}) {
            reviewThreads(first:100${afterClause}) {
              pageInfo { hasNextPage endCursor }
              nodes {
                isResolved
                comments(first:100) {
                  nodes { author { login } createdAt body }
                }
              }
            }
            reviewRequests(first:100) {
              nodes {
                requestedReviewer {
                  ... on User { __typename login }
                  ... on Bot { __typename login }
                  ... on Team { __typename name: name }
                  ... on Mannequin { __typename login }
                }
              }
            }
          }
        }
      }`,
    ]);

    if (!result.ok) {
      match(result.error, ghErrorThrow("review-threads"));
      // ghErrorThrow always throws; this is unreachable but satisfies the
      // return type so paginateThreads stays gh-agnostic.
      throw new Error("unreachable");
    }

    return result.data;
  });
}
