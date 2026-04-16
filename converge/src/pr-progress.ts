/**
 * Post /converge progress as PR comments.
 *
 * Policy (v1 — deliberately noisy):
 *   - One comment per iteration where /converge takes or delegates an
 *     action (full, llm, wait, human).
 *   - One comment on every halt (success, stalled, timeout, error,
 *     llm_needed, hil, pr_terminal, cancelled, fitness_unavailable).
 *   - Hygiene actions (`target_effect: "neutral"`) don't appear because
 *     /converge never picks them as the iteration's action.
 *
 * Failure is non-fatal: the loop must not crash or slow down because a
 * `gh` call failed. Errors are logged at verbose level and swallowed.
 *
 * Auto-enabled when the fitness skill is `pr-fitness` and `args[0]`
 * matches `owner/name` + `args[1]` is an integer. Otherwise disabled.
 */

import { spawn } from "node:child_process";

import type {
  Action,
  FitnessId,
  FitnessReport,
  HaltReport,
} from "./types/index.js";
import { PR_FITNESS } from "./types/index.js";
import { verbose } from "./util/log.js";

// ---------------------------------------------------------------------------

export interface PrProgressTarget {
  /** `owner/name`, passed to `gh -R`. */
  readonly repo: string;
  /** Pull request number as string, passed to `gh pr comment <n>`. */
  readonly pr: string;
}

const REPO_RE = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;
const PR_RE = /^[0-9]+$/;

/**
 * Try to build a PrProgressTarget from /converge's fitness args. Returns
 * null when the shape doesn't match — callers should treat that as
 * "progress reporting disabled" rather than an error.
 */
export function detectPrProgressTarget(
  fitness: FitnessId,
  args: readonly string[],
): PrProgressTarget | null {
  if (fitness !== PR_FITNESS) return null;
  const repo = args[0];
  const pr = args[1];
  if (repo === undefined || pr === undefined) return null;
  if (!REPO_RE.test(repo) || !PR_RE.test(pr)) return null;
  return { repo, pr };
}

// ---------------------------------------------------------------------------

const COMMENT_TIMEOUT_MS = 15_000;
const HARD_KILL_GRACE_MS = 2_000;

async function postComment(
  target: PrProgressTarget,
  body: string,
): Promise<void> {
  return new Promise<void>((resolve) => {
    const child = spawn(
      "gh",
      ["pr", "comment", target.pr, "-R", target.repo, "--body-file", "-"],
      { stdio: ["pipe", "ignore", "pipe"] },
    );

    // If gh ignores SIGTERM we still need the Promise to resolve, so
    // escalate to SIGKILL after a short grace.
    let hardKill: NodeJS.Timeout | undefined;
    const softKill = setTimeout(() => {
      child.kill("SIGTERM");
      hardKill = setTimeout(() => {
        child.kill("SIGKILL");
      }, HARD_KILL_GRACE_MS);
    }, COMMENT_TIMEOUT_MS);

    const clearTimers = (): void => {
      clearTimeout(softKill);
      if (hardKill !== undefined) clearTimeout(hardKill);
    };

    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (c: string) => {
      stderr += c;
    });

    child.on("error", (err) => {
      clearTimers();
      verbose(`pr-progress: spawn error: ${String(err)}`);
      resolve();
    });

    child.on("close", (code) => {
      clearTimers();
      if (code !== 0) {
        verbose(
          `pr-progress: gh pr comment exit=${String(code)} stderr=${stderr.trim()}`,
        );
      }
      resolve();
    });

    child.stdin.end(body);
  });
}

// ---------------------------------------------------------------------------

/**
 * Score hero line. Target emoji hidden until score reaches it — the
 * reveal IS the dopamine hit. Below target: `{emoji} {label} → {text}`.
 * At target: `{emoji} {label}`.
 */
function scoreLine(report: FitnessReport): string {
  const emoji = report.score_emoji ?? "";
  const label = report.score_label ?? String(report.score as number);
  if (report.score >= report.target) return `${emoji} ${label}`.trim();
  const target = report.target_label ?? String(report.target as number);
  return `${emoji} ${label} → ${target}`.trim();
}

function appendAxes(lines: string[], axes: FitnessReport["axes"]): void {
  if (axes === undefined || axes.length === 0) return;
  lines.push("");
  for (const a of axes) {
    const summary = a.summary.length > 0 ? ` ${a.summary}` : "";
    lines.push(`${a.emoji} ${a.name}${summary}`);
  }
}

function appendNotes(lines: string[], notes: FitnessReport["notes"]): void {
  if (notes === undefined || notes.length === 0) return;
  lines.push("");
  for (const n of notes) {
    lines.push(n);
  }
}

function appendSnapshot(
  lines: string[],
  report: FitnessReport,
  extra: Record<string, unknown>,
): void {
  if (report.snapshot === undefined) return;
  const merged = { ...extra, ...report.snapshot };
  lines.push("");
  lines.push("<details><summary>Fitness snapshot</summary>");
  lines.push("");
  lines.push("```json");
  lines.push(JSON.stringify(merged, null, 2));
  lines.push("```");
  lines.push("");
  lines.push("</details>");
}

function iterationBody(
  iter: number,
  report: FitnessReport,
  action: Action,
): string {
  const lines: string[] = [];
  lines.push(`🤖 **converge** · iter ${String(iter)}`);
  lines.push("");
  lines.push(scoreLine(report));
  appendAxes(lines, report.axes);
  lines.push("");
  lines.push(`> ${action.description}`);
  appendNotes(lines, report.notes);
  appendSnapshot(lines, report, {
    type: "iteration",
    iter,
    action: {
      kind: action.kind,
      automation: action.automation,
      description: action.description,
    },
  });
  return lines.join("\n");
}

function haltBody(
  halt: HaltReport,
  lastReport: FitnessReport | undefined,
): string {
  const lines: string[] = [];
  const isSuccess = halt.status === "success";
  const suffix = isSuccess ? " 🎉" : "";
  lines.push(
    `🤖 **converge${isSuccess ? "" : " halt"}** · iter ${String(halt.iterations)}${suffix}`,
  );
  lines.push("");

  if (lastReport !== undefined && halt.status !== "fitness_unavailable") {
    lines.push(scoreLine(lastReport));
    appendAxes(lines, lastReport.axes);
  }

  switch (halt.status) {
    case "success":
      lines.push("");
      lines.push("Target reached.");
      break;
    case "pr_terminal":
      lines.push("");
      lines.push(`PR is ${halt.terminal.kind}.`);
      break;
    case "llm_needed":
      lines.push("");
      lines.push(`> ${halt.action.description}`);
      lines.push("");
      lines.push(`Resume: \`${halt.resume_cmd.join(" ")}\``);
      break;
    case "hil":
      lines.push("");
      lines.push(`> ${halt.action.description}`);
      break;
    case "stalled":
      lines.push("");
      lines.push("No advancing actions.");
      break;
    case "timeout":
      lines.push("");
      lines.push("Iteration cap reached.");
      break;
    case "error":
    case "fitness_unavailable": {
      lines.push("");
      lines.push(`Cause: ${halt.cause.message}`);
      const stderr = halt.cause.stderr;
      if (stderr !== undefined && stderr.length > 0) {
        lines.push("");
        lines.push("```");
        lines.push(stderr.trim().slice(0, 500));
        lines.push("```");
      }
      break;
    }
    case "cancelled":
      lines.push("");
      lines.push("Interrupted.");
      break;
  }

  appendNotes(lines, lastReport?.notes);

  // Build the snapshot. For error/fitness_unavailable halts where
  // lastReport may be undefined, emit a minimal snapshot with the
  // cause so agents can parse it structurally.
  const extra: Record<string, unknown> = {
    type: "halt",
    halt: halt.status,
    iter: halt.iterations,
  };
  if (halt.status === "llm_needed" || halt.status === "hil") {
    extra["action"] = {
      kind: halt.action.kind,
      automation: halt.action.automation,
      description: halt.action.description,
    };
  }
  if (halt.status === "llm_needed") {
    extra["resume_cmd"] = halt.resume_cmd;
  }
  if (halt.status === "error" || halt.status === "fitness_unavailable") {
    extra["cause"] = {
      source: halt.cause.source,
      message: halt.cause.message,
      ...(halt.cause.stderr !== undefined
        ? { stderr: halt.cause.stderr.trim().slice(0, 500) }
        : {}),
    };
  }
  if (lastReport !== undefined) {
    appendSnapshot(lines, lastReport, extra);
  } else {
    // No fitness report available (e.g. fitness_unavailable on first
    // observation). Emit a bare snapshot with just the halt context.
    lines.push("");
    lines.push("<details><summary>Fitness snapshot</summary>");
    lines.push("");
    lines.push("```json");
    lines.push(JSON.stringify(extra, null, 2));
    lines.push("```");
    lines.push("");
    lines.push("</details>");
  }
  return lines.join("\n");
}

// ---------------------------------------------------------------------------

export async function reportIteration(
  target: PrProgressTarget | null,
  iter: number,
  report: FitnessReport,
  action: Action,
): Promise<void> {
  if (target === null) return;
  await postComment(target, iterationBody(iter, report, action));
}

export async function reportHalt(
  target: PrProgressTarget | null,
  halt: HaltReport,
  lastReport: FitnessReport | undefined,
): Promise<void> {
  if (target === null) return;
  await postComment(target, haltBody(halt, lastReport));
}
