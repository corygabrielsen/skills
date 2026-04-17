/**
 * Fitness skill invocation with bounded retry/backoff.
 *
 * v1 dispatch is hardcoded: only `pr-fitness` is recognized. The subprocess
 * is `npx tsx ${HOME}/code/skills/pr-fitness/src/cli.ts -q -c <...args>`.
 * Output is JSON on stdout; stderr is captured for error classification.
 *
 * ## Error boundary role (post-I₃)
 *
 * This module classifies pr-fitness's *stderr* at the subprocess boundary.
 * This is distinct from I₁ (gh() classifies gh's stderr inside pr-fitness).
 * With I₁–I₃, application-level failures (empty API results, individual
 * collector errors for non-fatal collectors) are handled inside pr-fitness:
 * the subprocess exits 0 with a degraded report. Only infrastructure
 * failures that prevent pr-fitness from producing any report reach here.
 *
 * ## Failure taxonomy (by stderr regex)
 *
 * Error classes that reach this layer:
 *   - rate-limit  → transient; honor `Retry-After` if present, else backoff.
 *   - network     → transient; exponential backoff (1s, 2s, 4s).
 *   - auth        → permanent; rethrow immediately (`FitnessUnavailableError`).
 *   - other       → permanent; rethrow immediately (e.g. pr-fitness crash,
 *                   fatal collector failure, unexpected runtime error).
 *
 * Error classes that NO LONGER reach this layer (handled by I₁–I₃):
 *   - "no required checks reported" → I₂: empty → []
 *   - non-fatal collector failures  → I₃: degrade, still exit 0
 *   - individual API empty results  → I₂: per-collector domain semantics
 *
 * Max 3 attempts. On exhaustion, throw `FitnessUnavailableError` carrying
 * the last stderr.
 */

import { spawn } from "node:child_process";
import * as os from "node:os";
import * as path from "node:path";

import type {
  Action,
  FitnessId,
  FitnessReport,
  JsonValue,
} from "./types/index.js";
import { PR_FITNESS, PositiveSeconds, Score } from "./types/index.js";
import { FitnessUnavailableError, PreconditionError } from "./util/errors.js";
import { verbose } from "./util/log.js";
import { sleep } from "./util/sleep.js";

const MAX_ATTEMPTS = 3;
const BASE_BACKOFF_MS = 1_000;

// Subprocess stderr classifiers. Regex is appropriate here because this is a
// process boundary: pr-fitness is a child process whose only error channel is
// stderr text. Inside pr-fitness, I₁ classifies gh errors structurally via
// GhResult — no regex needed there. These regexes target infrastructure
// failures (rate-limit, network, auth) that prevent pr-fitness from running
// at all. Application-level errors are handled inside pr-fitness (I₂/I₃).
const RATE_LIMIT_RE = /rate limit|secondary rate limit/i;
const RETRY_AFTER_RE = /Retry-After:\s*(\d+)/i;
const AUTH_RE = /Bad credentials|authentication required|not authenticated/i;
const NETWORK_RE = /ECONNRESET|ETIMEDOUT|ENOTFOUND|EAI_AGAIN/i;

interface InvokeResult {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number | null;
}

// ---------------------------------------------------------------------------

function isRateLimit(stderr: string): boolean {
  return RATE_LIMIT_RE.test(stderr);
}

function isAuth(stderr: string): boolean {
  return AUTH_RE.test(stderr);
}

function isNetwork(stderr: string): boolean {
  return NETWORK_RE.test(stderr);
}

function retryAfterSeconds(stderr: string): number | null {
  const m = RETRY_AFTER_RE.exec(stderr);
  if (!m?.[1]) return null;
  const n = Number.parseInt(m[1], 10);
  return Number.isFinite(n) && n > 0 ? n : null;
}

function exponentialBackoffMs(attempt: number): number {
  return BASE_BACKOFF_MS * 2 ** (attempt - 1);
}

// ---------------------------------------------------------------------------

function resolveCommand(
  fitnessName: FitnessId,
  fitnessArgs: readonly string[],
): readonly string[] {
  if (fitnessName === PR_FITNESS) {
    const cli = path.join(
      os.homedir(),
      "code",
      "skills",
      "pr-fitness",
      "src",
      "cli.ts",
    );
    return ["npx", "tsx", cli, "-q", "-c", ...fitnessArgs];
  }
  throw new PreconditionError(`unknown fitness skill: ${fitnessName}`);
}

// ---------------------------------------------------------------------------

async function invokeOnce(
  argv: readonly string[],
  signal: AbortSignal,
): Promise<InvokeResult> {
  return new Promise<InvokeResult>((resolve, reject) => {
    const [cmd, ...args] = argv;
    if (cmd === undefined) {
      reject(new PreconditionError("empty fitness argv"));
      return;
    }

    const child = spawn(cmd, args, {
      env: { ...process.env },
      signal,
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    const STDOUT_CAP = 8 * 1024 * 1024;
    const STDERR_CAP = 1 * 1024 * 1024;

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");

    child.stdout.on("data", (chunk: string) => {
      if (stdout.length < STDOUT_CAP) {
        stdout += chunk.slice(0, STDOUT_CAP - stdout.length);
      }
    });
    child.stderr.on("data", (chunk: string) => {
      if (stderr.length < STDERR_CAP) {
        stderr += chunk.slice(0, STDERR_CAP - stderr.length);
      }
    });

    child.on("error", (err) => {
      reject(err);
    });

    child.on("close", (code) => {
      resolve({ stdout, stderr, exitCode: code });
    });
  });
}

// ---------------------------------------------------------------------------

function parseAndValidate(stdout: string): FitnessReport {
  let parsed: unknown;
  try {
    parsed = JSON.parse(stdout);
  } catch (err) {
    throw new PreconditionError(
      `fitness stdout is not valid JSON: ${String(err)}`,
    );
  }

  if (typeof parsed !== "object" || parsed === null) {
    throw new PreconditionError("fitness report is not an object");
  }
  const obj = parsed as Record<string, unknown>;

  const rawScore = obj["score"];
  const rawTarget = obj["target"];
  const rawActions = obj["actions"];

  if (typeof rawScore !== "number") {
    throw new PreconditionError("fitness report missing numeric `score`");
  }
  if (typeof rawTarget !== "number") {
    throw new PreconditionError("fitness report missing numeric `target`");
  }
  if (!Array.isArray(rawActions)) {
    throw new PreconditionError("fitness report missing `actions` array");
  }

  const actions: Action[] = rawActions.map((a, i) => {
    if (typeof a !== "object" || a === null) {
      throw new PreconditionError(`actions[${i}] is not an object`);
    }
    const ao = a as Record<string, unknown>;
    const kind = ao["kind"];
    const automation = ao["automation"];
    const description = ao["description"];
    const target_effect = ao["target_effect"];
    if (typeof kind !== "string") {
      throw new PreconditionError(`actions[${i}].kind missing`);
    }
    if (typeof description !== "string") {
      throw new PreconditionError(`actions[${i}].description missing`);
    }
    if (
      target_effect !== "advances" &&
      target_effect !== "blocks" &&
      target_effect !== "neutral"
    ) {
      throw new PreconditionError(
        `actions[${i}].target_effect invalid: ${String(target_effect)}`,
      );
    }
    const type = isJsonValue(ao["type"]) ? { type: ao["type"] } : {};
    switch (automation) {
      case "full": {
        const execute = ao["execute"];
        if (!Array.isArray(execute) || execute.length === 0) {
          throw new PreconditionError(
            `actions[${i}].execute must be a non-empty argv array for automation=full`,
          );
        }
        for (let j = 0; j < execute.length; j++) {
          if (typeof execute[j] !== "string") {
            throw new PreconditionError(
              `actions[${i}].execute[${String(j)}] must be a string`,
            );
          }
        }
        const rawTimeout = ao["timeout_seconds"];
        if (rawTimeout !== undefined && typeof rawTimeout !== "number") {
          throw new PreconditionError(
            `actions[${i}].timeout_seconds must be a number if present`,
          );
        }
        return {
          kind,
          description,
          target_effect,
          automation,
          ...type,
          execute: execute as readonly string[],
          ...(rawTimeout !== undefined
            ? { timeout_seconds: PositiveSeconds(rawTimeout) }
            : {}),
        };
      }
      case "wait": {
        const nps = ao["next_poll_seconds"];
        if (typeof nps !== "number") {
          throw new PreconditionError(
            `actions[${i}].next_poll_seconds must be a number for automation=wait`,
          );
        }
        return {
          kind,
          description,
          target_effect,
          automation,
          ...type,
          next_poll_seconds: PositiveSeconds(nps),
        };
      }
      case "llm": {
        return {
          kind,
          description,
          target_effect,
          automation,
          ...type,
          ...(isJsonValue(ao["context"]) ? { context: ao["context"] } : {}),
        };
      }
      case "human": {
        return { kind, description, target_effect, automation, ...type };
      }
      default:
        throw new PreconditionError(
          `actions[${i}].automation invalid: ${String(automation)}`,
        );
    }
  });

  const report: FitnessReport = {
    score: Score(rawScore),
    target: Score(rawTarget),
    actions,
    ...(nonEmptyString(obj["status"]) ? { status: obj["status"] } : {}),
    ...(nonEmptyString(obj["score_display"])
      ? { score_display: obj["score_display"] }
      : {}),
    ...(nonEmptyString(obj["target_display"])
      ? { target_display: obj["target_display"] }
      : {}),
    ...(isNonEmptyStringArray(obj["notes"]) ? { notes: obj["notes"] } : {}),
    ...(isStringArray(obj["blockers"]) ? { blockers: obj["blockers"] } : {}),
    ...(isStringMap(obj["activity_state"])
      ? { activity_state: obj["activity_state"] }
      : {}),
    ...(nonEmptyString(obj["score_emoji"])
      ? { score_emoji: obj["score_emoji"] }
      : {}),
    ...(nonEmptyString(obj["score_label"])
      ? { score_label: obj["score_label"] }
      : {}),
    ...(nonEmptyString(obj["target_label"])
      ? { target_label: obj["target_label"] }
      : {}),
    ...(isAxisArray(obj["axes"]) ? { axes: obj["axes"] } : {}),
    ...(isPlainObject(obj["snapshot"])
      ? { snapshot: obj["snapshot"] as Record<string, unknown> }
      : {}),
    ...(isTerminal(obj["terminal"]) ? { terminal: obj["terminal"] } : {}),
  };
  return report;
}

// Empty strings carry no information; treat them as absent so consumers
// only need to check for `undefined`.
function nonEmptyString(v: unknown): v is string {
  return typeof v === "string" && v.length > 0;
}

function isNonEmptyStringArray(v: unknown): v is readonly string[] {
  return Array.isArray(v) && v.length > 0 && v.every((x) => nonEmptyString(x));
}

function isStringArray(v: unknown): v is readonly string[] {
  return Array.isArray(v) && v.every((x) => typeof x === "string");
}

function isStringMap(v: unknown): v is Record<string, string> {
  if (typeof v !== "object" || v === null) return false;
  const proto: unknown = Object.getPrototypeOf(v);
  if (proto !== Object.prototype && proto !== null) return false;
  for (const val of Object.values(v as Record<string, unknown>)) {
    if (typeof val !== "string") return false;
  }
  return true;
}

function isAxisArray(
  v: unknown,
): v is readonly { name: string; emoji: string; summary: string }[] {
  if (!Array.isArray(v)) return false;
  return v.every(
    (x) =>
      typeof x === "object" &&
      x !== null &&
      typeof (x as Record<string, unknown>)["name"] === "string" &&
      typeof (x as Record<string, unknown>)["emoji"] === "string" &&
      typeof (x as Record<string, unknown>)["summary"] === "string",
  );
}

function isPlainObject(v: unknown): boolean {
  if (typeof v !== "object" || v === null) return false;
  const proto: unknown = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}

function isTerminal(v: unknown): v is { readonly kind: string } {
  if (typeof v !== "object" || v === null) return false;
  const kind = (v as Record<string, unknown>)["kind"];
  return typeof kind === "string";
}

function isJsonValue(v: unknown): v is JsonValue {
  if (v === null) return true;
  const t = typeof v;
  if (t === "string" || t === "number" || t === "boolean") return true;
  if (Array.isArray(v)) return (v as readonly unknown[]).every(isJsonValue);
  if (t !== "object") return false;
  // Plain objects only — reject class instances, Maps, Sets, etc.
  const proto: unknown = Object.getPrototypeOf(v as object);
  if (proto !== Object.prototype && proto !== null) return false;
  for (const k of Object.keys(v as object)) {
    if (!isJsonValue((v as Record<string, unknown>)[k])) return false;
  }
  return true;
}

// ---------------------------------------------------------------------------

export async function invokeFitness(
  fitnessName: FitnessId,
  fitnessArgs: readonly string[],
  signal: AbortSignal,
): Promise<FitnessReport> {
  const argv = resolveCommand(fitnessName, fitnessArgs);

  let lastStderr = "";

  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    if (signal.aborted) {
      throw new DOMException("Aborted", "AbortError");
    }

    verbose(`fitness: invoke attempt ${attempt}/${MAX_ATTEMPTS}`);

    let result: InvokeResult;
    try {
      result = await invokeOnce(argv, signal);
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") throw err;
      if (
        err instanceof Error &&
        "code" in err &&
        (err as NodeJS.ErrnoException).code === "ABORT_ERR"
      ) {
        throw new DOMException("Aborted", "AbortError");
      }
      lastStderr = err instanceof Error ? err.message : String(err);
      if (attempt === MAX_ATTEMPTS) {
        throw new FitnessUnavailableError(attempt, lastStderr);
      }
      await sleep(exponentialBackoffMs(attempt), signal);
      continue;
    }

    lastStderr = result.stderr;

    if (result.exitCode === 0) {
      return parseAndValidate(result.stdout);
    }

    // Classify failure. Post-I₃, non-zero exit means an infrastructure failure
    // (rate-limit, network, auth) or a genuine pr-fitness crash. Application-
    // level "data not ready" cases exit 0 with a degraded report.
    if (isAuth(result.stderr)) {
      throw new FitnessUnavailableError(attempt, result.stderr);
    }

    const isTransient = isNetwork(result.stderr) || isRateLimit(result.stderr);
    if (!isTransient) {
      // Non-transient non-zero exit — permanent. Includes pr-fitness crashes,
      // fatal collector errors, and unknown runtime failures.
      throw new FitnessUnavailableError(attempt, result.stderr);
    }

    if (attempt === MAX_ATTEMPTS) {
      throw new FitnessUnavailableError(attempt, result.stderr);
    }

    const retryAfter = retryAfterSeconds(result.stderr);
    const waitMs =
      retryAfter !== null ? retryAfter * 1000 : exponentialBackoffMs(attempt);
    verbose(`fitness: transient failure, sleeping ${waitMs}ms`);
    await sleep(waitMs, signal);
  }

  throw new FitnessUnavailableError(MAX_ATTEMPTS, lastStderr);
}
