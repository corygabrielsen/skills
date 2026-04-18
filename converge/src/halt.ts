/**
 * Halt reporting: exit.json persistence and human-readable trace lines.
 *
 * - `writeInProgress`: stub written on CLI startup so stale exit.json from
 *   a prior run is never mistaken for the current invocation.
 * - `writeHalt`:       finalize exit.json with the halt report.
 * - `printHaltLine`:   one-line summary to stderr for the human operator.
 * - `printTraceLine`:  per-iteration breadcrumb to stderr before each action.
 *
 * stdout is reserved for a machine-readable view; this module only writes
 * files and stderr.
 */

import { promises as fs } from "node:fs";
import * as path from "node:path";

import type {
  Action,
  HaltBody,
  HaltReport,
  InProgressReport,
  Score,
} from "./types/index.js";
import { log } from "./util/log.js";

function nowIso(): string {
  return new Date().toISOString();
}

/** Write an `{stage: "in_progress"}` sentinel to exit.json. */
export async function writeInProgress(
  sessionDir: string,
  sessionId: string,
  resumeCmd: readonly string[],
): Promise<void> {
  const stub: InProgressReport = {
    stage: "in_progress",
    timestamp: nowIso(),
    session_id: sessionId,
    resume_cmd: resumeCmd,
  };
  const exitPath = path.join(sessionDir, "exit.json");
  await fs.writeFile(exitPath, `${JSON.stringify(stub, null, 2)}\n`, "utf8");
}

/** Finalize exit.json with the halt report. */
export async function writeHalt(
  sessionDir: string,
  report: HaltReport,
): Promise<void> {
  const exitPath = path.join(sessionDir, "exit.json");
  await fs.writeFile(exitPath, `${JSON.stringify(report, null, 2)}\n`, "utf8");
}

/**
 * Wrap a body with the exit-report header fields. Converge constructs
 * bodies internally; this adds stage/timestamp/session_id/resume_cmd
 * to yield a final HaltReport ready for writeHalt + printHaltLine.
 */
export function finalizeHalt(
  body: HaltBody,
  sessionId: string,
  resumeCmd: readonly string[],
): HaltReport {
  return {
    ...body,
    stage: "final",
    timestamp: nowIso(),
    session_id: sessionId,
    resume_cmd: resumeCmd,
  };
}

export function printHaltLine(report: HaltReport): void {
  if (report.status === "fitness_unavailable") {
    log(`halt ${report.status} iter ${report.iterations}`);
    return;
  }
  const score = String(report.final_score as number);
  let suffix = "";
  if (report.status === "agent_needed" || report.status === "hil") {
    suffix = ` action=${report.action.kind} desc="${report.action.description}"`;
  }
  if (
    report.status === "success" &&
    report.structural_blockers !== undefined &&
    report.structural_blockers.length > 0
  ) {
    suffix = ` structural=${report.structural_blockers.join(",")}`;
  }
  log(
    `halt ${report.status} iter ${report.iterations} score=${score}${suffix}`,
  );
}

/** Print the "to resume" hint when the caller is expected to re-invoke. */
export function printResumeHint(report: HaltReport): void {
  if (report.status === "agent_needed") {
    log(`to resume: ${report.resume_cmd.join(" ")}`);
    return;
  }
  if (
    report.status === "success" &&
    report.structural_blockers !== undefined &&
    report.structural_blockers.length > 0
  ) {
    log(`to resume: ${report.resume_cmd.join(" ")}`);
  }
}

export function printTraceLine(
  iter: number,
  score: Score,
  action: Action,
): void {
  log(
    `iter ${iter} score=${String(score as number)} action=${action.kind} (${action.automation})`,
  );
}
