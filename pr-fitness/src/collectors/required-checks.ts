import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

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
 *
 * I₂: `empty → []` — the bug fix. No required checks right now is valid,
 * possibly transient. All other GhError variants are fatal.
 */
export async function collectRequiredCheckNames(
  repo: string,
  pr: number,
): Promise<readonly string[]> {
  const result = await gh<RequiredCheckRow[]>([
    "pr",
    "checks",
    String(pr),
    "-R",
    repo,
    "--required",
    "--json",
    "name",
  ]);
  if (result.ok) return result.data.map((r) => r.name);
  return match(result.error, {
    ...ghErrorThrow("required-checks"),
    empty: () => [],
  });
}
