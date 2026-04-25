import type { PositiveSeconds } from "./branded.js";

/**
 * An action that could increase PR fitness.
 *
 * Each action is derived from a specific blocker. The consumer
 * (agent, skill, loop) decides what to execute. The plan function
 * only prescribes — it never acts.
 */
export interface Action {
  /** Which blocker this action addresses. */
  readonly blocker: string;
  /** Discriminant at top level — mirrors `type.kind`. Required by the /converge fitness contract. */
  readonly kind: string;
  /** What to do (human-readable). */
  readonly description: string;
  /** How automatable is this action? */
  readonly automation: Automation;
  /**
   * Effect this action has on reaching the report's target score.
   * - "advances": executing this action can raise score toward target
   * - "blocks":   this action represents a hard blocker; score stays low until cleared
   * - "neutral":  hygiene — surfaced but does not drive the loop
   */
  readonly target_effect: TargetEffect;
  /** The type of action — determines how to execute it. */
  readonly type: ActionType;
  /**
   * Argv for full-automation actions. Required when `automation === "full"`;
   * absent otherwise. /converge spawns this directly.
   */
  readonly execute?: readonly string[];
  /**
   * Next poll interval for wait actions. Required when `automation === "wait"`;
   * absent otherwise.
   */
  readonly next_poll_seconds?: PositiveSeconds;
}

export type TargetEffect = "advances" | "blocks" | "neutral";

export type Automation =
  /** Agent can do this without any human input. */
  | "full"
  /** Agent needs judgment (read logs, write code, respond to review). */
  | "agent"
  /** Requires a human (approve, clarify, decide). */
  | "human"
  /** Nothing to do — just wait. */
  | "wait";

export type ActionType =
  | { kind: "rerun_flake"; run_id: string }
  | { kind: "fix_ci"; check_name: string }
  | { kind: "address_threads"; count: number }
  | { kind: "address_bot_comments"; count: number }
  | { kind: "request_approval" }
  | { kind: "self_approve" }
  | { kind: "rebase" }
  | { kind: "mark_ready" }
  | { kind: "remove_wip_label" }
  | { kind: "shorten_title"; current_len: number }
  | { kind: "add_content_label" }
  | { kind: "add_assignee" }
  | { kind: "add_reviewer" }
  | { kind: "add_description" }
  | { kind: "wait_for_ci"; pending: readonly string[] }
  | { kind: "triage_wait"; blocked_checks: readonly string[] }
  | { kind: "wait_for_review"; reviewers: readonly string[] }
  | { kind: "wait_for_human" }
  | { kind: "rerequest_copilot" }
  | { kind: "wait_for_copilot_ack" }
  | { kind: "wait_for_copilot_review" }
  | { kind: "address_copilot_suppressed"; count: number }
  | { kind: "wait_for_cursor_review" };
