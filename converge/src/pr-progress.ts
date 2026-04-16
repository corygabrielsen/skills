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

import type { Action, FitnessReport, HaltReport } from "./types/index.js";
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
  fitness: string,
  args: readonly string[],
): PrProgressTarget | null {
  if (fitness !== "pr-fitness") return null;
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
 * Render the score line for a progress comment.
 *
 * `score_display → target_display` when below target (the convergence
 * journey); just `score_display` when at/above target. Displays are
 * fitness-owned; converge falls back to em-dashes if unset so it stays
 * agnostic to the skill's vocabulary.
 */
function scoreLine(report: FitnessReport): string {
  const s = report.score_display ?? "—";
  const t = report.target_display ?? "—";
  if (report.score >= report.target) return s;
  return `${s} → ${t}`;
}

function formatAction(action: Action): string {
  return `\`${action.kind}\` · ${action.automation} · ${action.target_effect}`;
}

function iterationBody(
  iter: number,
  report: FitnessReport,
  action: Action,
): string {
  const lines: string[] = [];
  lines.push(`🤖 **/converge** · iter ${String(iter)}`);
  lines.push("");
  lines.push(`**Score:** ${scoreLine(report)}`);
  lines.push(`**Action:** ${formatAction(action)}`);
  if (report.status !== undefined) {
    lines.push(`**Status:** ${report.status}`);
  }
  lines.push("");
  lines.push(`> ${action.description}`);
  if (action.automation === "full") {
    lines.push("");
    lines.push("```");
    lines.push(action.execute.join(" "));
    lines.push("```");
  }
  return lines.join("\n");
}

function haltBody(
  halt: HaltReport,
  lastReport: FitnessReport | undefined,
): string {
  const lines: string[] = [];
  const statusSuffix = halt.status === "success" ? " 🎉" : "";
  lines.push(
    `🤖 **/converge halt** · iter ${String(halt.iterations)}${statusSuffix}`,
  );
  lines.push("");
  lines.push(`**Status:** \`${halt.status}\``);

  // Reuse the iteration shape. `halt.final_score` equals `lastReport.score`
  // by construction (converge sets final_score from the same observation).
  // Skipped for fitness_unavailable where no score was ever observed.
  if (halt.status !== "fitness_unavailable" && lastReport !== undefined) {
    lines.push(`**Score:** ${scoreLine(lastReport)}`);
  }

  switch (halt.status) {
    case "success":
      break;
    case "pr_terminal":
      lines.push("");
      lines.push(`PR is ${halt.terminal.kind}.`);
      break;
    case "llm_needed":
      lines.push(`**Handoff:** ${formatAction(halt.action)}`);
      lines.push("");
      lines.push(`> ${halt.action.description}`);
      lines.push("");
      lines.push(`Resume: \`${halt.resume_cmd.join(" ")}\``);
      break;
    case "hil":
      lines.push(`**Handoff:** ${formatAction(halt.action)}`);
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
    case "fitness_unavailable":
      lines.push(`**Cause:** ${halt.cause.message}`);
      break;
    case "cancelled":
      lines.push("");
      lines.push("Interrupted.");
      break;
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
