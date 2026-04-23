/**
 * Post /converge progress as PR comments.
 *
 * Policy (v1 — deliberately noisy):
 *   - One comment per iteration where /converge takes or delegates an
 *     action (full, agent, wait, human).
 *   - One comment on every halt (success, stalled, timeout, error,
 *     agent_needed, hil, terminal, cancelled, fitness_unavailable).
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

// ---------------------------------------------------------------------------
// Minimal type surface consumed from /converge's domain. These are the
// shapes pr-progress reads from structured reports — they don't need to
// be the full converge types. Defining them locally avoids a cross-package
// dependency from pr-fitness → converge.
// ---------------------------------------------------------------------------

interface AxisLine {
  readonly name: string;
  readonly emoji: string;
  readonly summary: string;
}

/** Subset of converge's FitnessReport used for rendering. */
export interface FitnessReportView {
  readonly score: number;
  readonly target: number;
  readonly score_emoji?: string;
  readonly score_label?: string;
  readonly target_label?: string;
  readonly axes?: readonly AxisLine[];
  readonly notes?: readonly string[];
  readonly snapshot?: Readonly<Record<string, unknown>>;
  readonly blocker_split?: {
    readonly agent: readonly string[];
    readonly human: readonly string[];
    readonly structural: readonly string[];
  };
}

/** Subset of converge's Action used for rendering. */
export interface ActionView {
  readonly kind: string;
  readonly automation: string;
  readonly description: string;
}

/** Subset of converge's HaltReport used for rendering. */
export interface HaltReportView {
  readonly status: string;
  readonly iterations: number;
  readonly resume_cmd: readonly string[];
  readonly action?: ActionView;
  readonly terminal?: { readonly kind: string };
  readonly cause?: {
    readonly source: string;
    readonly message: string;
    readonly stderr?: string;
  };
  readonly structural_blockers?: readonly string[];
  readonly final_score?: number;
}

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

let verboseEnabled = false;

/** Enable verbose logging for pr-progress. */
export function setPrProgressVerbose(v: boolean): void {
  verboseEnabled = v;
}

function verbose(message: string): void {
  if (verboseEnabled) {
    process.stderr.write(`${message}\n`);
  }
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
function scoreLine(report: FitnessReportView): string {
  const emoji = report.score_emoji ?? "";
  const label = report.score_label ?? String(report.score);
  if (report.score >= report.target) return `${emoji} ${label}`.trim();
  const target = report.target_label ?? String(report.target);
  return `${emoji} ${label} → ${target}`.trim();
}

function appendAxes(
  lines: string[],
  axes: FitnessReportView["axes"],
): void {
  if (axes === undefined || axes.length === 0) return;
  lines.push("");
  for (const a of axes) {
    const summary = a.summary.length > 0 ? ` ${a.summary}` : "";
    lines.push(`${a.emoji} ${a.name}${summary}`);
  }
}

function appendNotes(
  lines: string[],
  notes: FitnessReportView["notes"],
): void {
  if (notes === undefined || notes.length === 0) return;
  lines.push("");
  for (const n of notes) {
    lines.push(n);
  }
}

function appendSnapshot(
  lines: string[],
  report: FitnessReportView,
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
  report: FitnessReportView,
  action: ActionView,
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
  halt: HaltReportView,
  lastReport: FitnessReportView | undefined,
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
      if (
        halt.structural_blockers !== undefined &&
        halt.structural_blockers.length > 0
      ) {
        lines.push("Target reached — structural blockers remain.");
        lines.push("");
        lines.push(`Resume: \`${halt.resume_cmd.join(" ")}\``);
      } else {
        lines.push("Target reached.");
      }
      break;
    case "terminal":
      lines.push("");
      if (halt.terminal !== undefined) {
        lines.push(`PR is ${halt.terminal.kind}.`);
      }
      break;
    case "agent_needed":
      lines.push("");
      if (halt.action !== undefined) {
        lines.push(`> ${halt.action.description}`);
      }
      lines.push("");
      lines.push(`Resume: \`${halt.resume_cmd.join(" ")}\``);
      break;
    case "hil":
      lines.push("");
      if (halt.action !== undefined) {
        lines.push(`> ${halt.action.description}`);
      }
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
      const cause = halt.cause;
      if (cause !== undefined) {
        lines.push(`Cause: ${cause.message}`);
        const stderr = cause.stderr;
        if (stderr !== undefined && stderr.length > 0) {
          lines.push("");
          lines.push("```");
          lines.push(stderr.trim().slice(0, 500));
          lines.push("```");
        }
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
  if (
    (halt.status === "agent_needed" || halt.status === "hil") &&
    halt.action !== undefined
  ) {
    extra["action"] = {
      kind: halt.action.kind,
      automation: halt.action.automation,
      description: halt.action.description,
    };
  }
  if (halt.status === "agent_needed") {
    extra["resume_cmd"] = halt.resume_cmd;
  }
  if (
    (halt.status === "error" || halt.status === "fitness_unavailable") &&
    halt.cause !== undefined
  ) {
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

// ---------------------------------------------------------------------------
// Standalone fitness comment (no converge context)
// ---------------------------------------------------------------------------

function fitnessCommentBody(
  report: FitnessReportView,
  topAction: ActionView | undefined,
): string {
  const lines: string[] = [];
  lines.push(scoreLine(report));
  appendAxes(lines, report.axes);
  if (topAction !== undefined) {
    lines.push("");
    lines.push(`> ${topAction.description}`);
  }
  appendNotes(lines, report.notes);
  appendSnapshot(lines, report, {
    type: "fitness",
    action: topAction
      ? {
          kind: topAction.kind,
          automation: topAction.automation,
          description: topAction.description,
        }
      : undefined,
  });
  return lines.join("\n");
}

/**
 * Render a standalone fitness comment body. Used by `--comment`
 * mode — no converge iteration context, just current PR state.
 */
export function renderFitnessComment(
  report: FitnessReportView,
  topAction: ActionView | undefined,
): string {
  return fitnessCommentBody(report, topAction);
}

/** Post a pre-rendered comment body on a PR. */
export { postComment };

// ---------------------------------------------------------------------------

export async function reportIteration(
  target: PrProgressTarget | null,
  iter: number,
  report: FitnessReportView,
  action: ActionView,
): Promise<void> {
  if (target === null) return;
  await postComment(target, iterationBody(iter, report, action));
}

export async function reportHalt(
  target: PrProgressTarget | null,
  halt: HaltReportView,
  lastReport: FitnessReportView | undefined,
): Promise<void> {
  if (target === null) return;
  await postComment(target, haltBody(halt, lastReport));
}
