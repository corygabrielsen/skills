import type { RequiredCheckConfig } from "../types/input.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

// ---------------------------------------------------------------------------
// Response shapes from the two authoritative configuration sources
// ---------------------------------------------------------------------------

/** Single rule entry from GET /repos/{o}/{r}/rules/branches/{branch}. */
interface BranchRule {
  readonly type: string;
  readonly parameters?: unknown;
  readonly ruleset_id: number;
  readonly ruleset_source: string;
  readonly ruleset_source_type: string;
}

/** Parameters for a rule of type "required_status_checks". */
interface RequiredStatusChecksParams {
  readonly required_status_checks: ReadonlyArray<{
    readonly context: string;
    readonly integration_id: number;
  }>;
}

/** Response from GET /repos/{o}/{r}/branches/{b}/protection/required_status_checks. */
interface BranchProtectionChecks {
  readonly checks: ReadonlyArray<{
    readonly context: string;
    readonly app_id: number | null;
  }>;
}

function isRequiredStatusChecksParams(
  params: unknown,
): params is RequiredStatusChecksParams {
  if (typeof params !== "object" || params === null) return false;
  const p = params as Record<string, unknown>;
  return Array.isArray(p.required_status_checks);
}

// ---------------------------------------------------------------------------
// Stack root resolution
// ---------------------------------------------------------------------------

/**
 * Walk down a PR stack to find the branch it ultimately merges into.
 *
 * Starting from `baseBranch`, find the open PR whose head branch equals
 * it, take that PR's base, and repeat. Terminates when no open PR has
 * the current branch as its head — that's the stack root (e.g. master).
 *
 * For non-stacked PRs (base = master), the first query returns nothing
 * and the function returns `baseBranch` unchanged.
 *
 * Graceful degradation: if any API call fails, returns the last
 * successfully resolved branch.
 */
export async function resolveStackRoot(
  repo: string,
  baseBranch: string,
): Promise<string> {
  let current = baseBranch;
  const visited = new Set<string>();
  while (!visited.has(current)) {
    visited.add(current);
    const result = await gh<Array<{ baseRefName: string }>>([
      "pr",
      "list",
      "-R",
      repo,
      "--head",
      current,
      "--state",
      "open",
      "--json",
      "baseRefName",
      "--limit",
      "1",
    ]);
    if (!result.ok) break;
    const first = result.data[0];
    if (!first) break;
    current = first.baseRefName;
  }
  return current;
}

// ---------------------------------------------------------------------------
// Collector
// ---------------------------------------------------------------------------

/**
 * Required check configuration for a branch — the authoritative set of
 * check names that must pass before merge.
 *
 * Queries two co-equal sources and unions the results:
 *
 *   1. **Rulesets** (modern): `GET /repos/{o}/{r}/rules/branches/{base}`
 *      Returns only `enforcement: "active"` rules. Covers repo-level
 *      and org-level rulesets.
 *
 *   2. **Branch Protection** (legacy): `GET /repos/{o}/{r}/branches/{base}/protection/required_status_checks`
 *      Returns the check list from classic branch protection rules.
 *      404 = no protection configured (valid, not an error).
 *
 * Neither source alone is complete — repos may use rulesets, legacy
 * protection, or both. The union by `context` is the full definition.
 */
export async function collectRequiredCheckConfig(
  repo: string,
  baseBranch: string,
): Promise<readonly RequiredCheckConfig[]> {
  const [rulesResult, protectionResult] = await Promise.all([
    gh<BranchRule[]>(["api", `repos/${repo}/rules/branches/${baseBranch}`]),
    gh<BranchProtectionChecks>([
      "api",
      `repos/${repo}/branches/${baseBranch}/protection/required_status_checks`,
    ]),
  ]);

  // Rulesets: must succeed (empty array is valid — no ruleset-based checks).
  if (!rulesResult.ok) {
    return match(rulesResult.error, ghErrorThrow("required-checks"));
  }

  // Branch protection: 404 = not configured (valid). Other errors throw.
  let protectionChecks: ReadonlyArray<{
    context: string;
    app_id: number | null;
  }> = [];
  if (protectionResult.ok) {
    protectionChecks = protectionResult.data.checks;
  } else if (protectionResult.error.kind !== "not_found") {
    return match(protectionResult.error, ghErrorThrow("required-checks"));
  }

  // Union and deduplicate by context name.
  const seen = new Set<string>();
  const configs: RequiredCheckConfig[] = [];

  for (const rule of rulesResult.data) {
    if (rule.type !== "required_status_checks") continue;
    if (!isRequiredStatusChecksParams(rule.parameters)) continue;
    for (const check of rule.parameters.required_status_checks) {
      if (!seen.has(check.context)) {
        seen.add(check.context);
        configs.push({
          context: check.context,
          integration_id: check.integration_id,
        });
      }
    }
  }

  for (const check of protectionChecks) {
    if (!seen.has(check.context)) {
      seen.add(check.context);
      configs.push({
        context: check.context,
        integration_id: check.app_id,
      });
    }
  }

  return configs;
}
