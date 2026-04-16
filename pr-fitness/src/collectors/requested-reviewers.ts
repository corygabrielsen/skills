import type { GitHubRequestedReviewers } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh } from "../util/gh.js";

const JQ_FILTER = `{
  users: [.users[] | { login: .login, type: .type }],
  teams: [.teams[] | { slug: .slug }]
}`;

export async function collectRequestedReviewers(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<GitHubRequestedReviewers> {
  return gh<GitHubRequestedReviewers>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/requested_reviewers`,
    "--jq",
    JQ_FILTER,
  ]);
}
