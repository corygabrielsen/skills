import type { GitHubIssueComment } from "../types/index.js";
import { gh } from "../util/gh.js";

export async function collectComments(
  repo: string,
  pr: number,
): Promise<readonly GitHubIssueComment[]> {
  return gh<GitHubIssueComment[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/comments`,
    "--jq",
    "[.[] | {login: .user.login, id: .id}]",
  ]);
}
