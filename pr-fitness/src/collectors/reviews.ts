import type { GitHubPullRequestReview } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/**
 * All reviews submitted on the PR.
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectReviews(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<readonly GitHubPullRequestReview[]> {
  const result = await gh<GitHubPullRequestReview[]>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/reviews`,
    "--paginate",
    "--jq",
    "[.[] | {user: .user.login, state, commit_id, submitted_at, body}]",
  ]);
  if (result.ok) return result.data;
  return match(result.error, ghErrorThrow("reviews"));
}
