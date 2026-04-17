import type { GitHubIssueComment } from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/**
 * Issue-level comments on the PR (not inline review comments).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectComments(
  repo: string,
  pr: number,
): Promise<readonly GitHubIssueComment[]> {
  const result = await gh<GitHubIssueComment[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/comments`,
    "--paginate",
    "--jq",
    "[.[] | {login: .user.login, id: .id}]",
  ]);
  if (result.ok) return result.data;
  return match(result.error, {
    ...ghErrorThrow("comments"),
  });
}
