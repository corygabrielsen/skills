import type { GitHubCheck } from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/**
 * All check runs on the PR (required and optional).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectChecks(
  repo: string,
  pr: number,
): Promise<readonly GitHubCheck[]> {
  const result = await gh<GitHubCheck[]>([
    "pr",
    "checks",
    String(pr),
    "-R",
    repo,
    "--json",
    "name,state,description,link,completedAt",
  ]);
  if (result.ok) return result.data;
  return match(result.error, {
    ...ghErrorThrow("checks"),
  });
}
