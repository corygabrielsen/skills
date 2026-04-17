import type { GitHubPullRequestView } from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

const PR_FIELDS = [
  "title",
  "number",
  "url",
  "body",
  "state",
  "isDraft",
  "mergeable",
  "mergeStateStatus",
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

/**
 * Core PR metadata — the single required collector.
 *
 * I₂: ALL errors fatal. Without PR metadata, no report can be built.
 */
export async function collectPrMetadata(
  repo: string,
  pr: number,
): Promise<GitHubPullRequestView> {
  const result = await gh<GitHubPullRequestView>([
    "pr",
    "view",
    String(pr),
    "-R",
    repo,
    "--json",
    PR_FIELDS,
  ]);
  if (result.ok) return result.data;
  return match(result.error, ghErrorThrow("pr-metadata"));
}
