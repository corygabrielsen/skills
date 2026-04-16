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
  /** What to do (human-readable). */
  readonly description: string;
  /** How automatable is this action? */
  readonly automation: Automation;
  /** The type of action — determines how to execute it. */
  readonly type: ActionType;
}

export type Automation =
  /** Agent can do this without any human input. */
  | "full"
  /** Agent needs LLM judgment (read logs, write code, respond to review). */
  | "llm"
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
  | { kind: "wait_for_review"; reviewers: readonly string[] }
  | { kind: "wait_for_human" }
  | { kind: "rerequest_copilot" }
  | { kind: "wait_for_copilot_ack" }
  | { kind: "wait_for_copilot_review" }
  | { kind: "address_copilot_suppressed"; count: number };
