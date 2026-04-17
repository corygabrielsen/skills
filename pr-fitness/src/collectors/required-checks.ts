import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";
import { setTimeout as sleep } from "node:timers/promises";

interface RequiredCheckRow {
  readonly name: string;
}

/**
 * GitHub does not atomically create check-run objects on push. After a
 * force-push (or first push), there is a 5-30s window where
 * `gh pr checks --required` exits 1 with "no required checks reported"
 * even though the branch protection rules ARE configured — the check
 * runs simply don't exist yet.
 *
 * A single retry after a short wait accommodates this. The wait is only
 * paid when the first call fails (happy path has zero delay). If the
 * retry also fails, the error flows through the normal match → throw →
 * I₃ degrade path.
 */
const POST_PUSH_SETTLE_MS = 5_000;

/**
 * Names of checks that gate merge for this PR.
 *
 * `gh pr checks --required` abstracts over both classic branch-protection
 * rules and rulesets — whatever's actually required comes back.
 */
export async function collectRequiredCheckNames(
  repo: string,
  pr: number,
): Promise<readonly string[]> {
  const args = [
    "pr",
    "checks",
    String(pr),
    "-R",
    repo,
    "--required",
    "--json",
    "name",
  ] as const;

  let result = await gh<RequiredCheckRow[]>([...args]);
  if (!result.ok) {
    await sleep(POST_PUSH_SETTLE_MS);
    result = await gh<RequiredCheckRow[]>([...args]);
  }

  if (result.ok) return result.data.map((r) => r.name);
  return match(result.error, ghErrorThrow("required-checks"));
}
