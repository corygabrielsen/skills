import { gh } from "../util/gh.js";

interface RequiredCheckRow {
  readonly name: string;
}

/**
 * Names of checks that gate merge for this PR.
 *
 * `gh pr checks --required` abstracts over both classic branch-protection
 * rules and rulesets — whatever's actually required comes back. Empty
 * array means "nothing is required," which is a legitimate config and
 * shouldn't error: the fallback falls through to review/approval gates.
 */
export async function collectRequiredCheckNames(
  repo: string,
  pr: number,
): Promise<readonly string[]> {
  const rows = await gh<RequiredCheckRow[]>([
    "pr",
    "checks",
    String(pr),
    "-R",
    repo,
    "--required",
    "--json",
    "name",
  ]);
  return rows.map((r) => r.name);
}
