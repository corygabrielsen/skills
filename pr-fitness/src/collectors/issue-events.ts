import type { GitHubIssueEvent } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

// Shape events into the domain type. For review_requested /
// review_request_removed, synthesize a discriminated `requested` field
// combining the raw API's split `requested_reviewer` / `requested_team`
// fields. Other events get the catchall shape.
const JQ_FILTER = `[.[] | . as $e | {
  event,
  actor: (if .actor then { login: .actor.login } else null end),
  created_at
} + (
  if (.event == "review_requested" or .event == "review_request_removed") then
    if .requested_reviewer then
      { requested: { kind: "user", login: .requested_reviewer.login } }
    elif .requested_team then
      { requested: { kind: "team", slug: .requested_team.slug } }
    else {} end
  else {} end
)]`;

/**
 * Timeline events on the PR (issue-events endpoint).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectIssueEvents(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<readonly GitHubIssueEvent[]> {
  const result = await gh<GitHubIssueEvent[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/events`,
    "--paginate",
    "--jq",
    JQ_FILTER,
  ]);
  if (result.ok) return result.data;
  return match(result.error, {
    ...ghErrorThrow("issue-events"),
  });
}
