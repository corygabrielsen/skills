import type { GitHubPullRequestReview } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/** Raw shape from GET /repos/{o}/{r}/pulls/{n}/reviews. */
interface RawReview {
  user: { login: string };
  state: string;
  commit_id: string;
  submitted_at: string;
  body: string;
}

/**
 * All reviews submitted on the PR.
 *
 * Field selection done in TypeScript so --paginate can merge
 * pages into a single array (--jq breaks multi-page responses).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectReviews(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<readonly GitHubPullRequestReview[]> {
  const result = await gh<RawReview[]>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/reviews`,
    "--paginate",
  ]);
  if (result.ok) {
    return result.data.map((r) => ({
      user: r.user.login,
      state: r.state,
      commit_id: r.commit_id,
      submitted_at: r.submitted_at,
      body: r.body,
    })) as GitHubPullRequestReview[];
  }
  return match(result.error, ghErrorThrow("reviews"));
}
