import type { GitHubPullRequestView } from "../types/index.js";
import { gh } from "../util/gh.js";

const PR_FIELDS = [
  "title",
  "number",
  "url",
  "body",
  "state",
  "isDraft",
  "mergeable",
  "headRefOid",
  "baseRefName",
  "updatedAt",
  "closedAt",
  "mergedAt",
  "labels",
  "assignees",
  "reviewRequests",
  "reviewDecision",
  "commits",
].join(",");

export async function collectPrMetadata(
  repo: string,
  pr: number,
): Promise<GitHubPullRequestView> {
  return gh<GitHubPullRequestView>([
    "pr",
    "view",
    String(pr),
    "-R",
    repo,
    "--json",
    PR_FIELDS,
  ]);
}
