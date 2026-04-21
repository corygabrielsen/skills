import type { GitHubIssueComment } from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/** Raw shape from GET /repos/{o}/{r}/issues/{n}/comments. */
interface RawIssueComment {
  id: number;
  user: { login: string };
}

/**
 * Issue-level comments on the PR (not inline review comments).
 *
 * Field selection done in TypeScript so --paginate can merge
 * pages into a single array (--jq breaks multi-page responses).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectComments(
  repo: string,
  pr: number,
): Promise<readonly GitHubIssueComment[]> {
  const result = await gh<RawIssueComment[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/comments`,
    "--paginate",
  ]);
  if (result.ok) {
    return result.data.map((c) => ({ id: c.id, login: c.user.login }));
  }
  return match(result.error, ghErrorThrow("comments"));
}
