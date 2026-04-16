import type { GitHubPullRequestReview } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh } from "../util/gh.js";

export async function collectReviews(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<readonly GitHubPullRequestReview[]> {
  return gh<GitHubPullRequestReview[]>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/reviews`,
    "--paginate",
    "--jq",
    "[.[] | {user: .user.login, state, commit_id, submitted_at, body}]",
  ]);
}
