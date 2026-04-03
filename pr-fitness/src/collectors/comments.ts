import type { GhIssueComment } from "../types/index.js";
import { gh } from "../util/gh.js";

export async function collectComments(
  repo: string,
  pr: number,
): Promise<readonly GhIssueComment[]> {
  return gh<GhIssueComment[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/comments`,
    "--jq",
    "[.[] | {login: .user.login, id: .id}]",
  ]);
}
