/**
 * Action — what a fitness skill prescribes for the next iteration.
 *
 * Discriminated on `automation`. Each variant carries only the fields its
 * executor needs. /converge drives `full` actions itself, surfaces `llm`
 * and `human` actions as halt reasons, and sleeps on `wait`.
 */

import type { JsonValue, PositiveSeconds } from "./branded.js";

/**
 * Effect this action has on reaching the report's target score.
 * - `advances`: executing this action can raise score toward target
 * - `blocks`:   hard blocker; score stays low until cleared
 * - `neutral`:  hygiene — surfaced but does not drive the loop
 */
export type TargetEffect = "advances" | "blocks" | "neutral";

interface ActionBase {
  /** Stable identifier for the action variety (e.g. `"rerequest_copilot"`). */
  readonly kind: string;
  /** Human-readable summary. */
  readonly description: string;
  /** How this action relates to the target score. */
  readonly target_effect: TargetEffect;
}

/** Fully automatable: spawn `execute` argv with optional timeout. */
export interface FullAction extends ActionBase {
  readonly automation: "full";
  readonly execute: readonly string[];
  readonly timeout_seconds?: PositiveSeconds;
}

/** Requires LLM judgment. /converge halts `llm_needed` with context attached. */
export interface LlmAction extends ActionBase {
  readonly automation: "llm";
  readonly context?: JsonValue;
}

/** Nothing to do but wait. /converge sleeps `next_poll_seconds` and re-polls. */
export interface WaitAction extends ActionBase {
  readonly automation: "wait";
  readonly next_poll_seconds: PositiveSeconds;
}

/** Requires a human. /converge halts `hil` with the action attached. */
export interface HumanAction extends ActionBase {
  readonly automation: "human";
}

export type Action = FullAction | LlmAction | WaitAction | HumanAction;
