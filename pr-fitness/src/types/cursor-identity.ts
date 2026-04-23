/**
 * Cursor Bugbot identity.
 *
 * Cursor exposes a single login string across all API surfaces:
 *   `cursor[bot]` (GitHub App, App slug "cursor", App ID 1210556).
 *
 * Check runs from Cursor use the name `Cursor Bugbot`.
 */

import { GitHubLogin } from "./branded.js";

export const CURSOR_LOGIN: GitHubLogin = GitHubLogin("cursor[bot]");

/** Name of the check run Cursor Bugbot posts. */
export const CURSOR_CHECK_NAME = "Cursor Bugbot";

/** True iff `login` is Cursor's identity. */
export function isCursor(login: GitHubLogin | string): boolean {
  return login === CURSOR_LOGIN;
}
