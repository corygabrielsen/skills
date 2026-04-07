import type { GhReviewThreadsResponse } from "../types/index.js";
import { gh } from "../util/gh.js";

export async function collectReviewThreads(
  owner: string,
  name: string,
  pr: number,
): Promise<GhReviewThreadsResponse> {
  return gh<GhReviewThreadsResponse>([
    "api",
    "graphql",
    "-f",
    `query={
      repository(owner:"${owner}",name:"${name}") {
        pullRequest(number:${String(pr)}) {
          reviewThreads(first:100) {
            nodes {
              isResolved
              comments(first:1) {
                nodes { author { login } }
              }
            }
          }
          reviewRequests(first:20) {
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
}
