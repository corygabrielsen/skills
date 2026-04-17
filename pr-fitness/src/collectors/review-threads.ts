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

/**
 * Review threads and pending review requests (GraphQL).
 *
 * I₂: `empty →` minimal valid response with empty nodes arrays.
 */
export async function collectReviewThreads(
  owner: string,
  name: string,
  pr: number,
): Promise<GitHubPullRequestReviewThreadsResponse> {
  const result = await gh<GitHubPullRequestReviewThreadsResponse>([
    "api",
    "graphql",
    "-f",
    `query={
      repository(owner:"${owner}",name:"${name}") {
        pullRequest(number:${String(pr)}) {
          reviewThreads(first:100) {
            nodes {
              isResolved
              comments(first:100) {
                nodes { author { login } createdAt }
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
  if (result.ok) return result.data;
  return match(result.error, {
    ...ghErrorThrow("review-threads"),
    empty: () => EMPTY_THREADS,
  });
}
