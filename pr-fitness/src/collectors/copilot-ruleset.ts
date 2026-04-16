import type { RepoSlug } from "../types/branded.js";
import type {
  CopilotRepoConfig,
  GitHubCopilotRule,
  GitHubRule,
  GitHubRuleset,
} from "../types/index.js";
import { gh } from "../util/gh.js";

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
 */
export async function collectCopilotRuleset(
  repo: RepoSlug,
): Promise<CopilotRepoConfig> {
  const summaries = await gh<readonly RulesetSummary[]>([
    "api",
    `repos/${repo}/rulesets?per_page=100`,
  ]);

  if (summaries.length === 0) return DISABLED;

  const rulesets = await Promise.all(
    summaries.map((s) =>
      gh<GitHubRuleset>(["api", `repos/${repo}/rulesets/${String(s.id)}`]),
    ),
  );

  for (const ruleset of rulesets) {
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
