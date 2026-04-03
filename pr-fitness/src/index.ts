/**
 * pr-fitness — Live PR merge readiness assessment.
 *
 * @example
 * ```ts
 * import { prFitness } from "pr-fitness";
 *
 * const report = await prFitness("example/widgets", 1563);
 * console.log(report.mergeable);  // false
 * console.log(report.blockers);   // ["not_approved"]
 * ```
 */
export { prFitness } from "./pr-fitness.js";

export type {
  PrFitnessReport,
  Lifecycle,
  CiSummary,
  FailedCheck,
  ReviewSummary,
  ReviewDecision,
  PrState,
  ConflictState,
  GraphiteCheck,
  GraphiteStatus,
} from "./types/output.js";

export type { Action, Automation, ActionType } from "./types/action.js";
