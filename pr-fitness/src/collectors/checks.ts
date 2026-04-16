import type { GitHubCheck } from "../types/index.js";
import { gh } from "../util/gh.js";

export async function collectChecks(
  repo: string,
  pr: number,
): Promise<readonly GitHubCheck[]> {
  return gh<GitHubCheck[]>([
    "pr",
    "checks",
    String(pr),
    "-R",
    repo,
    "--json",
    "name,state,description,link,completedAt",
  ]);
}
