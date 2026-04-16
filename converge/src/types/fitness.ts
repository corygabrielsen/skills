/**
 * FitnessReport — the structured payload a fitness skill emits to stdout.
 *
 * /converge consumes: `score`, `target`, `actions`, and optionally
 * `terminal`. `status` is presentation-only (the skill's own summary line).
 */

import type { Action } from "./action.js";
import type { Score } from "./branded.js";

/**
 * One row in the progress comment's axis display. Pr-fitness constructs
 * these from its computed state; /converge renders them verbatim as
 * `{emoji} {name} {summary}` lines. Converge never interprets what
 * the axes mean — the fitness skill owns the vocabulary.
 */
export interface AxisLine {
  readonly name: string;
  readonly emoji: string;
  readonly summary: string;
}

export interface FitnessReport {
  /** Current fitness. */
  readonly score: Score;
  /** Target fitness. /converge halts `success` when `score >= target`. */
  readonly target: Score;
  /** Ordered prescription for the next iteration. */
  readonly actions: readonly Action[];
  /** Opaque human-readable status from the skill (e.g. a label). */
  readonly status?: string;
  /**
   * Branded rendering of `score` for human-readable output — typically
   * `"<emoji> (<label>)"`. When absent, /converge falls back to the
   * numeric score.
   */
  readonly score_display?: string;
  /** Branded rendering of `target`, mirror of `score_display`. */
  readonly target_display?: string;
  /** Decomposed score components for the PR progress comment. */
  readonly score_emoji?: string;
  readonly score_label?: string;
  /**
   * Target label WITHOUT emoji — used in the "→ platinum" arrow when
   * score < target. The target emoji is revealed only when score reaches
   * target (via score_emoji matching at that point).
   */
  readonly target_label?: string;
  /**
   * Per-axis status lines for the PR progress comment. Each entry is
   * rendered as `{emoji} {name} {summary}`. Pr-fitness constructs them;
   * converge renders them.
   */
  readonly axes?: readonly AxisLine[];
  /**
   * Skill-owned informational lines — rendered verbatim by /converge as
   * bullet points in the PR progress comment. Intended for context the
   * reader should see but that doesn't drive convergence (e.g. advisory
   * check failures, informational warnings). Each entry is one line.
   */
  readonly notes?: readonly string[];
  /**
   * Current blockers preventing fitness from advancing. Opaque string
   * tokens — /converge uses these for iteration-key dedup (blocker-set
   * changes advance the iteration) but doesn't interpret them.
   */
  readonly blockers?: readonly string[];
  /**
   * Per-axis activity state the skill wants /converge to track for
   * iteration boundaries. E.g. pr-fitness emits `{ copilot: "working" }`
   * so a Copilot `working → reviewed` transition advances the iteration
   * even when the picked action and blockers haven't changed.
   *
   * Keys are skill-defined labels; values are opaque state strings. The
   * map is folded into the iteration key without interpretation.
   */
  readonly activity_state?: Readonly<Record<string, string>>;
  /**
   * Terminal state external to the fitness loop (e.g. PR merged/closed).
   * `kind` vocabulary is owned by the skill; /converge treats it opaquely
   * and halts `pr_terminal`.
   */
  readonly terminal?: { readonly kind: string };
}
