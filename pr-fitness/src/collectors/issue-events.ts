import type { GitHubIssueEvent } from "../types/index.js";
import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { gh, match } from "../util/gh.js";
import { ghErrorThrow } from "../util/collector-error.js";

/** Raw shape from GET /repos/{o}/{r}/issues/{n}/events. */
export interface RawIssueEvent {
  event: string;
  actor: { login: string } | null;
  created_at: string | null;
  requested_reviewer?: { login: string } | null;
  requested_team?: { slug: string } | null;
}

/**
 * Shape a raw API event into the domain discriminated union.
 *
 * For review_requested / review_request_removed, synthesize a
 * `requested` field from the raw API's split requested_reviewer /
 * requested_team fields.
 */
export function shapeEvent(raw: RawIssueEvent): GitHubIssueEvent {
  const base = {
    event: raw.event,
    actor: raw.actor ? { login: raw.actor.login } : null,
    created_at: raw.created_at,
  };

  if (
    raw.event === "review_requested" ||
    raw.event === "review_request_removed"
  ) {
    const requested = raw.requested_reviewer
      ? ({ kind: "user", login: raw.requested_reviewer.login } as const)
      : raw.requested_team
        ? ({ kind: "team", slug: raw.requested_team.slug } as const)
        : null;
    if (requested) {
      return { ...base, event: raw.event, requested } as GitHubIssueEvent;
    }
  }

  return base as GitHubIssueEvent;
}

/**
 * Timeline events on the PR (issue-events endpoint).
 *
 * Field selection done in TypeScript so --paginate can merge
 * pages into a single array (--jq breaks multi-page responses).
 *
 * I₂: All GhError variants throw CollectorError. I₃ degrades non-fatal.
 */
export async function collectIssueEvents(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<readonly GitHubIssueEvent[]> {
  const result = await gh<RawIssueEvent[]>([
    "api",
    `repos/${repo}/issues/${String(pr)}/events`,
    "--paginate",
  ]);
  if (result.ok) return result.data.map(shapeEvent);
  return match(result.error, ghErrorThrow("issue-events"));
}
