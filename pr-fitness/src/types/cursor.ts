/**
 * Cursor domain model — distilled view of Cursor Bugbot activity,
 * review rounds, threads, and scoring tier.
 *
 * Cursor differs from Copilot:
 *   - Auto-runs on every push (no request/ack cycle).
 *   - No ruleset config (managed at cursor.com/dashboard/bugbot).
 *   - Posts no review when clean — check run flips to `success`.
 *   - No suppressed-findings concept.
 *
 * Configured detection is activity-based: if we observe any Cursor
 * review or a `Cursor Bugbot` check run on the PR, Cursor is active.
 */

import type { GitCommitSha, Timestamp } from "./branded.js";
import type { CopilotTier } from "./copilot.js";

// ---------------------------------------------------------------------------
// CursorReviewRound — one review with findings. Clean runs don't
// produce a round (Cursor posts no review when clean).
// ---------------------------------------------------------------------------

export interface CursorReviewRound {
  /** 1-indexed ordinal within this PR's Cursor review history. */
  readonly round: number;
  /** When the review was submitted. */
  readonly reviewedAt: Timestamp;
  /** Commit Cursor reviewed. */
  readonly commit: GitCommitSha;
  /** Number of findings (parsed from review body: "found N potential issue(s)"). */
  readonly findingsCount: number;
}

// ---------------------------------------------------------------------------
// CursorThreadSummary — same shape as Copilot. `stale` counts
// Cursor-authored threads with non-Cursor replies after the latest
// review (reviewer hasn't seen them).
// ---------------------------------------------------------------------------

export interface CursorThreadSummary {
  readonly total: number;
  readonly resolved: number;
  readonly unresolved: number;
  readonly stale: number;
}

// ---------------------------------------------------------------------------
// CursorActivity — discriminated union on `state`. Only reachable when
// `CursorReport.configured === true`; configured-ness lives on the
// outer discriminant.
//
//   idle       configured but no review or check run at HEAD yet
//   reviewing  Cursor Bugbot check at HEAD is queued/in_progress
//   reviewed   Cursor has reviewed (with findings) at least once
//   clean      Cursor Bugbot check at HEAD completed with `success`
// ---------------------------------------------------------------------------

export type CursorActivity =
  | { readonly state: "idle" }
  | { readonly state: "reviewing" }
  | { readonly state: "reviewed"; readonly latest: CursorReviewRound }
  | { readonly state: "clean" };

// ---------------------------------------------------------------------------
// CursorTier — identical tier ladder to Copilot. Unified semantics:
//
//   BRONZE:   unreviewed ∨ unresolved>0
//   SILVER:   unresolved=0 ∧ currently re-reviewing after prior activity
//   GOLD:     unresolved=0 ∧ (reviewed at non-HEAD ∨ findings at HEAD but resolved)
//   PLATINUM: Cursor Bugbot check at HEAD = success (bot itself says clean)
// ---------------------------------------------------------------------------

export type CursorTier = CopilotTier;

// ---------------------------------------------------------------------------
// CursorReport — composite embedded in PullRequestFitnessReport.
//
// Discriminated union on `configured`. `configured` is derived from
// activity: if we've ever seen a Cursor review or Cursor check run on
// this PR, Cursor is active.
// ---------------------------------------------------------------------------

/** Severity breakdown of Cursor's unresolved findings, parsed from comment bodies. */
export interface CursorSeverityBreakdown {
  readonly high: number;
  readonly medium: number;
  readonly low: number;
}

export type CursorReport =
  | {
      readonly configured: false;
    }
  | {
      readonly configured: true;
      readonly activity: CursorActivity;
      /** All review rounds with findings, oldest first. Empty if never found issues. */
      readonly rounds: readonly CursorReviewRound[];
      readonly threads: CursorThreadSummary;
      /** Severity counts across unresolved Cursor threads only. */
      readonly severity: CursorSeverityBreakdown;
      readonly tier: CursorTier;
      readonly tier_display: string;
      /** True when the latest activity is at HEAD. */
      readonly fresh: boolean;
    };
