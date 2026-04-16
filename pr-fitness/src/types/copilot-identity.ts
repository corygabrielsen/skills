/**
 * Copilot identity normalization.
 *
 * GitHub exposes the Copilot reviewer under multiple distinct login
 * strings depending on the API surface:
 *
 *   REST pulls/{pr}/reviews              user.login                 = "copilot-pull-request-reviewer[bot]"
 *   REST pulls/{pr}/comments             user.login                 = "Copilot"
 *   REST pulls/{pr}/requested_reviewers  users[].login              = "Copilot"
 *   REST issues/{pr}/events              requested_reviewer.login   = "Copilot"
 *   GraphQL (all surfaces)               login                      = "copilot-pull-request-reviewer"
 *
 * Write-side asymmetry:
 *   POST   requested_reviewers  requires  "copilot-pull-request-reviewer[bot]"
 *   DELETE requested_reviewers  requires  "Copilot"
 *
 * Canonical form: `copilot-pull-request-reviewer[bot]` — the `[bot]`
 * suffix rules out human-user collision, and it matches the primary
 * REST reviews API.
 */

import { GitHubLogin } from "./branded.js";

export const CANONICAL_COPILOT_LOGIN: GitHubLogin = GitHubLogin(
  "copilot-pull-request-reviewer[bot]",
);

/** Raw string for POST /pulls/{pr}/requested_reviewers. */
export const COPILOT_ADD_LOGIN: string = CANONICAL_COPILOT_LOGIN;

/** Raw string for DELETE /pulls/{pr}/requested_reviewers. */
export const COPILOT_REMOVE_LOGIN: string = "Copilot";

/** Every known Copilot login variant. Exact-match only — never regex. */
export const KNOWN_COPILOT_LOGINS: readonly GitHubLogin[] = Object.freeze([
  CANONICAL_COPILOT_LOGIN,
  GitHubLogin("Copilot"),
  GitHubLogin("copilot-pull-request-reviewer"),
]);

const COPILOT_LOGIN_SET: ReadonlySet<string> = new Set(KNOWN_COPILOT_LOGINS);

export type CopilotIdentitySource =
  | "rest-reviews"
  | "rest-comments"
  | "rest-requested"
  | "rest-events"
  | "graphql";

const LOGIN_BY_SOURCE: Readonly<Record<CopilotIdentitySource, GitHubLogin>> = {
  "rest-reviews": CANONICAL_COPILOT_LOGIN,
  "rest-comments": GitHubLogin("Copilot"),
  "rest-requested": GitHubLogin("Copilot"),
  "rest-events": GitHubLogin("Copilot"),
  graphql: GitHubLogin("copilot-pull-request-reviewer"),
};

/** Raw login that API surface `source` produces. */
export function copilotLoginFor(source: CopilotIdentitySource): GitHubLogin {
  return LOGIN_BY_SOURCE[source];
}

/** True iff `login` is any known Copilot variant. */
export function isCopilot(login: GitHubLogin | string): boolean {
  return COPILOT_LOGIN_SET.has(login);
}

/** Map any Copilot variant to `CANONICAL_COPILOT_LOGIN`; else identity. */
export function normalizeCopilotLogin(login: GitHubLogin): GitHubLogin {
  return isCopilot(login) ? CANONICAL_COPILOT_LOGIN : login;
}
