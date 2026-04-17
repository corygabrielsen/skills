import type { GitHubRequestedReviewers } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

const JQ_FILTER = `{
  users: [.users[] | { login: .login, type: .type }],
  teams: [.teams[] | { slug: .slug }]
}`;

/**
 * Currently requested reviewers (users and teams).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectRequestedReviewers(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<GitHubRequestedReviewers> {
  const result = await gh<GitHubRequestedReviewers>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/requested_reviewers`,
    "--jq",
    JQ_FILTER,
  ]);
  if (result.ok) return result.data;
  return match(result.error, ghErrorThrow("requested-reviewers"));
}
