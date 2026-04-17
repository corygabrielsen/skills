import type { RepoSlug } from "../types/branded.js";
import type {
  CopilotRepoConfig,
  GitHubCopilotRule,
  GitHubRule,
  GitHubRuleset,
} from "../types/index.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

interface RulesetSummary {
  readonly id: number;
}

const DISABLED: CopilotRepoConfig = {
  enabled: false,
  reviewOnPush: false,
  reviewDraftPullRequests: false,
};

function isCopilotRule(rule: GitHubRule): rule is GitHubCopilotRule {
  return rule.type === "copilot_code_review";
}

/**
 * Fetch all rulesets and distill to a `CopilotRepoConfig`.
 *
 * Returns `{ enabled: true, ... }` iff an active ruleset contains a
 * `copilot_code_review` rule; otherwise `DISABLED`.
 *
 * I₂: `empty → DISABLED` for the list call (no rulesets configured).
 * Individual ruleset fetches also handle `empty → skip` since a
 * vanished ruleset between list and fetch is benign.
 */
export async function collectCopilotRuleset(
  repo: RepoSlug,
): Promise<CopilotRepoConfig> {
  const listResult = await gh<readonly RulesetSummary[]>([
    "api",
    `repos/${repo}/rulesets?per_page=100`,
  ]);

  if (!listResult.ok) {
    return match(listResult.error, {
      ...ghErrorThrow("copilot-ruleset"),
      empty: () => DISABLED,
    });
  }

  const summaries = listResult.data;
  if (summaries.length === 0) return DISABLED;

  const rulesetResults = await Promise.all(
    summaries.map((s) =>
      gh<GitHubRuleset>(["api", `repos/${repo}/rulesets/${String(s.id)}`]),
    ),
  );

  for (const rulesetResult of rulesetResults) {
    if (!rulesetResult.ok) {
      // A vanished ruleset between list and fetch is benign on empty;
      // all other errors are fatal.
      match(rulesetResult.error, {
        ...ghErrorThrow("copilot-ruleset"),
        empty: () => undefined,
      });
      continue;
    }

    const ruleset = rulesetResult.data;
    if (ruleset.enforcement !== "active") continue;
    for (const rule of ruleset.rules) {
      if (isCopilotRule(rule)) {
        return {
          enabled: true,
          reviewOnPush: rule.parameters.review_on_push,
          reviewDraftPullRequests: rule.parameters.review_draft_pull_requests,
        };
      }
    }
  }

  return DISABLED;
}
