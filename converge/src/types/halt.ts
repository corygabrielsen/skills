/**
 * HaltReport — terminal record written to `exit.json` and mirrored to the
 * process exit code.
 *
 * Discriminated on `status`. Variants carry exactly the context a consumer
 * (agent, human, CI) needs to either resume or act on the halt reason.
 */

import type { Action, HumanAction, AgentAction } from "./action.js";
import type { Score } from "./branded.js";

export type HaltStatus =
  | "success"
  | "stalled"
  | "timeout"
  | "hil"
  | "agent_needed"
  | "terminal"
  | "error"
  | "cancelled"
  | "fitness_unavailable";

/** Per-iteration audit record appended to `history[]`. */
export interface IterLog {
  readonly iter: number;
  readonly score: Score;
  /** Preserves the discriminant so consumers can re-narrow. */
  readonly action_summary: Pick<Action, "kind" | "automation">;
}

/**
 * Structured cause for `error` and `fitness_unavailable` halts.
 *
 * `retry_after_seconds` is intentionally unbranded — it's raw from a
 * upstream `Retry-After` header, which may exceed any local clamp.
 */
export interface ErrorCause {
  readonly source:
    | "fitness"
    | "execute"
    | "parse"
    | "lock"
    | "session_io"
    | "signal"
    | "invariant";
  readonly message: string;
  readonly stderr?: string;
  readonly retry_after_seconds?: number;
  readonly action_kind?: string;
}

/**
 * Fields present on every exit.json write — both the startup in-progress
 * stub and any final halt. Consumers should check `stage` first:
 *   - `"in_progress"` → /converge is mid-run; ignore status details.
 *   - `"final"`       → halt has occurred; `status` describes the outcome.
 *
 * `timestamp` is the moment the file was last written (not iteration time).
 * `resume_cmd` is the argv the caller should re-invoke to continue —
 * identical to the original CLI invocation; deterministic sessions mean
 * iteration numbering persists on re-run.
 */
export interface ExitReportHeader {
  readonly stage: "in_progress" | "final";
  readonly timestamp: string;
  readonly session_id: string;
  readonly resume_cmd: readonly string[];
}

/**
 * Startup stub. Written to exit.json immediately after session open so a
 * consumer can never mistake a stale prior-run exit.json for the current
 * invocation.
 */
export interface InProgressReport extends ExitReportHeader {
  readonly stage: "in_progress";
}

/**
 * Halt bodies without the exit-report header. The converge loop constructs
 * these and the `finalize` helper wraps them with stage/timestamp/etc.
 */
export type HaltBody =
  | {
      readonly status: "success";
      readonly iterations: number;
      readonly final_score: Score;
      /** Non-empty when fitness target reached but structural blockers remain. */
      readonly structural_blockers?: readonly string[];
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "stalled";
      readonly iterations: number;
      readonly final_score: Score;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "timeout";
      readonly iterations: number;
      readonly final_score: Score;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "hil";
      readonly iterations: number;
      readonly final_score: Score;
      readonly action: HumanAction;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "agent_needed";
      readonly iterations: number;
      readonly final_score: Score;
      readonly action: AgentAction;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "terminal";
      readonly iterations: number;
      readonly final_score: Score;
      readonly terminal: { readonly kind: string };
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "error";
      readonly iterations: number;
      readonly final_score: Score;
      readonly cause: ErrorCause;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "cancelled";
      readonly iterations: number;
      readonly final_score: Score;
      readonly history: readonly IterLog[];
    }
  | {
      readonly status: "fitness_unavailable";
      readonly iterations: number;
      readonly cause: ErrorCause;
      readonly history: readonly IterLog[];
    };

export type HaltReport = ExitReportHeader & HaltBody & { stage: "final" };
