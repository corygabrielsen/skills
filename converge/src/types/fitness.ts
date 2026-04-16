/**
 * FitnessReport — the structured payload a fitness skill emits to stdout.
 *
 * /converge consumes: `score`, `target`, `actions`, and optionally
 * `terminal`. `status` is presentation-only (the skill's own summary line).
 */

import type { Action } from "./action.js";
import type { Score } from "./branded.js";

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
   * Terminal state external to the fitness loop (e.g. PR merged/closed).
   * `kind` vocabulary is owned by the skill; /converge treats it opaquely
   * and halts `pr_terminal`.
   */
  readonly terminal?: { readonly kind: string };
}
