/**
 * Cursor Bugbot identity.
 *
 * Cursor exposes two login variants depending on API surface:
 *   REST     "cursor[bot]"  (user.login in reviews, comments, events)
 *   GraphQL  "cursor"       (login on thread comment authors)
 *
 * Check runs from Cursor use the name `Cursor Bugbot`.
 */

import { GitHubLogin } from "./branded.js";

export const CANONICAL_CURSOR_LOGIN: GitHubLogin = GitHubLogin("cursor[bot]");

/** Name of the check run Cursor Bugbot posts. */
export const CURSOR_CHECK_NAME = "Cursor Bugbot";

/** Every known Cursor login variant. Exact-match only — never regex. */
export const KNOWN_CURSOR_LOGINS: readonly GitHubLogin[] = Object.freeze([
  CANONICAL_CURSOR_LOGIN,
  GitHubLogin("cursor"),
]);

const CURSOR_LOGIN_SET: ReadonlySet<string> = new Set(KNOWN_CURSOR_LOGINS);

/** True iff `login` is any known Cursor variant. */
export function isCursor(login: GitHubLogin | string): boolean {
  return CURSOR_LOGIN_SET.has(login);
}
