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
  PullRequestFitnessReport,
  Lifecycle,
  CiSummary,
  FailedCheck,
  ReviewSummary,
  ReviewDecision,
  PullRequestState,
  ConflictState,
  GraphiteCheck,
  GraphiteStatus,
} from "./types/output.js";

export type { Action, Automation, ActionType } from "./types/action.js";

export type {
  CopilotReport,
  CopilotTier,
  CopilotActivity,
  CopilotReviewRound,
  CopilotRepoConfig,
  CopilotThreadSummary,
} from "./types/copilot.js";

export { COPILOT_TIER_EMOJI, formatCopilotTier } from "./types/copilot.js";
