/**
 * Copilot domain model — distilled view of Copilot configuration,
 * lifecycle state, review rounds, threads, and scoring tier.
 *
 * This module is a public shape: `CopilotReport` is embedded in
 * `PullRequestFitnessReport`. It is deliberately decoupled from the
 * raw GitHub API types in `copilot-input.ts` — collectors produce
 * these distilled shapes so downstream consumers never touch raw
 * parameters bags or timeline events.
 */

import type { GitCommitSha, Timestamp } from "./branded.js";

// Identity normalization vocabulary lives with the copilot domain.
export type { CopilotIdentitySource } from "./copilot-identity.js";

// ---------------------------------------------------------------------------
// CopilotRepoConfig — distilled from a GitHubRuleset with a
// copilot_code_review rule. Captures only the signals that drive
// downstream behavior: whether Copilot is active at all, and the two
// review-trigger flags.
// ---------------------------------------------------------------------------

export interface CopilotRepoConfig {
  /** True iff a copilot_code_review rule exists AND enforcement is "active". */
  readonly enabled: boolean;
  /** Auto-review on every push (vs only on explicit request). */
  readonly reviewOnPush: boolean;
  /** Review pull requests that are still in draft state. */
  readonly reviewDraftPullRequests: boolean;
}

// ---------------------------------------------------------------------------
// CopilotReviewRound — one complete request→ack→review cycle with
// body-parsed comment counts. All fields from the latest round are
// most relevant for scoring; every round is recorded for history.
// ---------------------------------------------------------------------------

export interface CopilotReviewRound {
  /** 1-indexed ordinal within this PR's Copilot review history. */
  readonly round: number;
  /** When the review was requested (timeline: review_requested event). */
  readonly requestedAt: Timestamp;
  /** When Copilot acknowledged (timeline: copilot_work_started event). Null if not yet acked. */
  readonly ackAt: Timestamp | null;
  /** When the review was submitted. Null if still in progress. */
  readonly reviewedAt: Timestamp | null;
  /** Commit Copilot reviewed. Null if not yet reviewed. */
  readonly commit: GitCommitSha | null;
  /** Number of visible inline comments (parsed from review body: "generated N comments"). */
  readonly commentsVisible: number;
  /** Number of suppressed low-confidence findings (parsed from review body details block). */
  readonly commentsSuppressed: number;
}

// ---------------------------------------------------------------------------
// CopilotThreadSummary — counts of Copilot-authored review threads by
// resolution status. Invariant (enforced by the collector, not the
// type): total === resolved + unresolved.
// ---------------------------------------------------------------------------

export interface CopilotThreadSummary {
  readonly total: number;
  readonly resolved: number;
  readonly unresolved: number;
}

// ---------------------------------------------------------------------------
// CopilotActivity — discriminated union on `state`.
//
// Lifecycle states:
//   unconfigured  no Copilot rule; Copilot is not part of review at all
//   idle          configured but no review activity yet
//   requested     review_requested event without subsequent copilot_work_started
//   working       copilot_work_started without subsequent submitted review
//   reviewed      at least one review completed
// ---------------------------------------------------------------------------

export type CopilotActivity =
  | { readonly state: "unconfigured" }
  | { readonly state: "idle" }
  | { readonly state: "requested"; readonly requestedAt: Timestamp }
  | {
      readonly state: "working";
      readonly requestedAt: Timestamp;
      readonly ackAt: Timestamp;
    }
  | {
      readonly state: "reviewed";
      /** The latest completed round. */
      readonly latest: CopilotReviewRound;
    };

// ---------------------------------------------------------------------------
// CopilotTier — scoring tier.
//
// Formal rules:
//   PLATINUM: reviewed ∧ unresolved=0 ∧ ¬suppressed ∧ latestCommit=HEAD
//   GOLD:     reviewed ∧ unresolved=0 ∧ ¬suppressed ∧ latestCommit≠HEAD
//   SILVER:   reviewed ∧ unresolved=0 ∧ suppressed
//   BRONZE:   unreviewed ∨ unresolved>0
// ---------------------------------------------------------------------------

export type CopilotTier = "bronze" | "silver" | "gold" | "platinum";

/** Total order on `CopilotTier`, ascending. */
export const COPILOT_TIER_ORDER: readonly CopilotTier[] = [
  "bronze",
  "silver",
  "gold",
  "platinum",
] as const;

/** Returns negative if a < b, positive if a > b, 0 if equal. */
export function compareCopilotTier(a: CopilotTier, b: CopilotTier): number {
  return COPILOT_TIER_ORDER.indexOf(a) - COPILOT_TIER_ORDER.indexOf(b);
}

// ---------------------------------------------------------------------------
// CopilotReport — composite embedded in PullRequestFitnessReport.
//
// Discriminated union on `configured`: consumers cannot access tier,
// activity, rounds, or threads when Copilot is not configured.
//
// `fresh` is stored explicitly (derived: latest.commit === head) so
// downstream consumers don't recompute. Always false when no review
// has completed.
// ---------------------------------------------------------------------------

export type CopilotReport =
  | {
      /** Copilot is not configured for this repo. */
      readonly configured: false;
    }
  | {
      readonly configured: true;
      readonly config: CopilotRepoConfig;
      readonly activity: CopilotActivity;
      /** All review rounds, oldest first. Empty array if no reviews yet. */
      readonly rounds: readonly CopilotReviewRound[];
      readonly threads: CopilotThreadSummary;
      readonly tier: CopilotTier;
      /**
       * True when the latest review is on HEAD. Derived (latest.commit === head).
       * Stored explicitly so downstream consumers don't recompute.
       * Always false when no reviews exist.
       */
      readonly fresh: boolean;
    };
