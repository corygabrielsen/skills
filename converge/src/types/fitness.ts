/**
 * FitnessReport — the structured payload a fitness skill emits to stdout.
 *
 * /converge consumes: `score`, `target`, `actions`, and optionally
 * `terminal`. `status` is presentation-only (the skill's own summary line).
 */

import type { Action } from "./action.js";
import type { Score } from "./branded.js";

/**
 * One row in the progress display. The fitness skill constructs
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
  /** Decomposed score components for progress rendering. */
  readonly score_emoji?: string;
  readonly score_label?: string;
  /**
   * Target label WITHOUT emoji — used in the "→ platinum" arrow when
   * score < target. The target emoji is revealed only when score reaches
   * target (via score_emoji matching at that point).
   */
  readonly target_label?: string;
  /**
   * Per-axis status lines for progress rendering. Each entry is
   * rendered as `{emoji} {name} {summary}`. The fitness skill constructs
   * them; converge renders them.
   */
  readonly axes?: readonly AxisLine[];
  /**
   * Curated JSON-serializable state snapshot for machine readers.
   * The fitness skill constructs the base; /converge enriches with
   * iter + action + halt fields and forwards it to progress callbacks.
   */
  readonly snapshot?: Readonly<Record<string, unknown>>;
  /**
   * Skill-owned informational lines — forwarded verbatim by /converge to
   * progress callbacks. Intended for context the reader should see but
   * that doesn't drive convergence (e.g. advisory warnings). Each entry
   * is one line.
   */
  readonly notes?: readonly string[];
  /**
   * Current blockers preventing fitness from advancing. Opaque string
   * tokens — /converge uses these for iteration-key dedup (blocker-set
   * changes advance the iteration) but doesn't interpret them.
   */
  readonly blockers?: readonly string[];
  /**
   * Blockers split by resolution authority. /converge uses this to
   * distinguish halt behavior when score reaches target:
   *   - agent non-empty → keep working (shouldn't happen at target)
   *   - structural non-empty → halt with re-entry hint
   *   - human non-empty → halt with "your turn"
   */
  readonly blocker_split?: {
    readonly agent: readonly string[];
    readonly human: readonly string[];
    readonly structural: readonly string[];
  };
  /**
   * Per-axis activity state the skill wants /converge to track for
   * iteration boundaries. E.g. a fitness skill emits `{ axis: "pending" }`
   * so a `pending → done` transition advances the iteration even
   * when the picked action and blockers haven't changed.
   *
   * Keys are skill-defined labels; values are opaque state strings. The
   * map is folded into the iteration key without interpretation.
   */
  readonly activity_state?: Readonly<Record<string, string>>;
  /**
   * Terminal state external to the fitness loop. `kind` vocabulary is
   * owned by the skill; /converge treats it opaquely and halts `terminal`.
   */
  readonly terminal?: { readonly kind: string };
}
