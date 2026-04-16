/**
 * Raw GitHub API response types for Copilot-adjacent endpoints.
 *
 * These model the exact shapes returned by:
 *   - GET repos/{owner}/{repo}/issues/{pr}/events
 *   - GET repos/{owner}/{repo}/pulls/{pr}/requested_reviewers
 *   - GET repos/{owner}/{repo}/rulesets (filtered to copilot_code_review)
 *
 * Only fields consumed downstream are typed.
 */

import type { GitHubLogin, Timestamp } from "./branded.js";

// ---------------------------------------------------------------------------
// Actor — a user or bot that performed an event.
// ---------------------------------------------------------------------------

export interface Actor {
  readonly login: GitHubLogin;
}

// ---------------------------------------------------------------------------
// GitHubIssueEvent — timeline event on a PR (issue-events endpoint).
//
// Discriminated union on `event`. Three structural events carry
// extra fields (review_requested, review_request_removed,
// copilot_work_started); everything else degrades to the `"other"`
// catchall.
// ---------------------------------------------------------------------------

/** User or bot reviewer (has `.login`). */
export interface RequestedUserReviewer {
  readonly kind: "user";
  readonly login: GitHubLogin;
}

/** Team reviewer (has `.slug`). */
export interface RequestedTeamReviewer {
  readonly kind: "team";
  readonly slug: string;
}

export type RequestedReviewerRef =
  | RequestedUserReviewer
  | RequestedTeamReviewer;

export interface GitHubPullRequestReviewRequestedEvent {
  readonly event: "review_requested";
  readonly actor: Actor | null;
  readonly created_at: Timestamp | null;
  readonly requested: RequestedReviewerRef;
}

export interface GitHubPullRequestReviewRequestRemovedEvent {
  readonly event: "review_request_removed";
  readonly actor: Actor | null;
  readonly created_at: Timestamp | null;
  readonly requested: RequestedReviewerRef;
}

export interface GitHubCopilotWorkStartedEvent {
  readonly event: "copilot_work_started";
  readonly actor: Actor | null;
  readonly created_at: Timestamp | null;
}

export interface GitHubOtherIssueEvent {
  readonly event: string;
  readonly actor: Actor | null;
  readonly created_at: Timestamp | null;
}

export type GitHubIssueEvent =
  | GitHubPullRequestReviewRequestedEvent
  | GitHubPullRequestReviewRequestRemovedEvent
  | GitHubCopilotWorkStartedEvent
  | GitHubOtherIssueEvent;

// ---------------------------------------------------------------------------
// GitHubRequestedReviewers — pulls/{pr}/requested_reviewers response.
// ---------------------------------------------------------------------------

export type GitHubUserType = "User" | "Bot" | "Organization" | "Mannequin";

export interface GitHubRequestedReviewerUser {
  readonly login: GitHubLogin;
  readonly type: GitHubUserType;
}

export interface GitHubRequestedReviewerTeam {
  readonly slug: string;
}

export interface GitHubRequestedReviewers {
  readonly users: readonly GitHubRequestedReviewerUser[];
  readonly teams: readonly GitHubRequestedReviewerTeam[];
}

// ---------------------------------------------------------------------------
// GitHubRuleset — ruleset with a discriminated union over rule types.
//
// Discriminated union on `type`. `copilot_code_review` is modeled
// precisely. Everything else is the `"other"` catchall with an
// unknown parameters bag; callers narrow by `type` before reading.
// ---------------------------------------------------------------------------

export interface GitHubCopilotRule {
  readonly type: "copilot_code_review";
  readonly parameters: {
    readonly review_on_push: boolean;
    readonly review_draft_pull_requests: boolean;
  };
}

export interface GitHubOtherRule {
  readonly type: string;
  readonly parameters?: Readonly<Record<string, unknown>>;
}

export type GitHubRule = GitHubCopilotRule | GitHubOtherRule;

export type GitHubRulesetEnforcement = "active" | "disabled" | "evaluate";

export interface GitHubRuleset {
  readonly id: number;
  readonly name: string;
  readonly enforcement: GitHubRulesetEnforcement;
  readonly rules: readonly GitHubRule[];
}
