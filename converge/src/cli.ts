#!/usr/bin/env node

/**
 * /converge — CLI entry point.
 *
 * Parses argv, derives a session id, installs signal handlers, and runs
 * `converge()` to a terminal HaltReport. The halt line and exit.json are
 * written by the core loop; this module only maps the halt status to a
 * process exit code and surfaces a couple of synchronous failure modes
 * (bad args → 2, lock held → 9).
 *
 * stdout is reserved; all human-readable output goes to stderr.
 */

import { execFileSync } from "node:child_process";

import { converge } from "./converge.js";
import { detectPrProgressTarget } from "./pr-progress.js";
import { gcStaleSessions } from "./session.js";
import type { HaltReport, HaltStatus } from "./types/index.js";
import { LockHeldError, PreconditionError } from "./util/errors.js";
import { setVerbose } from "./util/log.js";
import { VERSION } from "./version.js";

// ---------------------------------------------------------------------------

interface ParsedArgs {
  readonly fitness: string;
  readonly fitnessArgs: readonly string[];
  readonly maxIterations: number;
  readonly verbose: boolean;
}

const DEFAULT_MAX_ITERATIONS = 20;
const SEVEN_DAYS_MS = 7 * 24 * 60 * 60 * 1000;

function usage(): never {
  process.stderr.write(`Usage: converge <fitness> <fitness-args...> [options]

Iterates observe -> decide -> act on the given fitness skill until
the target is reached, iterations are exhausted, or an unsafe
action is encountered.

Fitness skills (v1):
  pr-fitness  <owner/repo> <pr>   Pull request convergence

Options:
  --max-iterations=N    Cap on iteration count (default ${String(DEFAULT_MAX_ITERATIONS)})
  -v, --verbose         Verbose logging
  -h, --help            This message
  --version             Print version

Exit codes:
   0 success              target reached
   1 stalled              no advancing actions
   2 timeout              iteration cap hit
   3 hil                  action requires human
   4 error                runtime failure
   5 llm_needed           action requires LLM judgment
   6 pr_terminal          PR merged/closed mid-loop
   7 cancelled            SIGINT / SIGTERM
   8 fitness_unavailable  fitness skill unavailable
   9 lock_held            session lock held by another process
  64 usage                bad argument (sysexits EX_USAGE)

Examples:
  converge pr-fitness example/widgets 1716
  converge pr-fitness 1716                        (infers repo from cwd)
  converge pr-fitness example-org/infrastructure 566 --max-iterations=30
`);
  process.exit(0);
}

// Exit 64 = sysexits EX_USAGE. Keeps the halt taxonomy (0-9) clean.
function die(message: string, code = 64): never {
  process.stderr.write(`converge: ${message}\n`);
  process.exit(code);
}

function parseMaxIterations(raw: string): number {
  if (!/^\d+$/.test(raw)) {
    die(`invalid --max-iterations: ${raw} (expected positive integer)`);
  }
  const n = Number.parseInt(raw, 10);
  if (!Number.isInteger(n) || n <= 0) {
    die(`invalid --max-iterations: ${raw} (expected positive integer)`);
  }
  return n;
}

function parseArgs(argv: readonly string[]): ParsedArgs {
  const positional: string[] = [];
  let maxIterations = DEFAULT_MAX_ITERATIONS;
  let verbose = false;
  let seenDoubleDash = false;

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === undefined) continue;

    if (seenDoubleDash) {
      positional.push(arg);
      continue;
    }

    if (arg === "--") {
      seenDoubleDash = true;
      continue;
    }
    if (arg === "-h" || arg === "--help") usage();
    if (arg === "--version") {
      process.stdout.write(`converge ${VERSION}\n`);
      process.exit(0);
    }
    if (arg === "-v" || arg === "--verbose") {
      verbose = true;
      continue;
    }
    if (arg.startsWith("--max-iterations=")) {
      maxIterations = parseMaxIterations(arg.slice("--max-iterations=".length));
      continue;
    }
    if (arg === "--max-iterations") {
      const next = argv[i + 1];
      if (next === undefined) die("--max-iterations requires a value");
      maxIterations = parseMaxIterations(next);
      i++;
      continue;
    }
    // Unknown flags are only errors before the first positional — after
    // the fitness name, flags belong to the fitness skill.
    if (arg.startsWith("-") && positional.length === 0) {
      die(`unknown option: ${arg}`);
    }
    positional.push(arg);
  }

  const fitness = positional[0];
  if (fitness === undefined) {
    die("missing argument: <fitness>");
  }

  return {
    fitness,
    fitnessArgs: positional.slice(1),
    maxIterations,
    verbose,
  };
}

// ---------------------------------------------------------------------------

/**
 * djb2 string hash. Deterministic, dependency-free, adequate for session-id
 * disambiguation — collisions merely share a session dir, they don't lose data.
 */
function djb2(s: string): string {
  let h = 5381;
  for (let i = 0; i < s.length; i++) {
    h = ((h << 5) + h + s.charCodeAt(i)) | 0;
  }
  // Unsigned 32-bit, base-36 for compactness.
  return (h >>> 0).toString(36);
}

function sanitize(s: string): string {
  return s.replace(/[^A-Za-z0-9_-]/g, "-");
}

function deriveSessionId(fitness: string, args: readonly string[]): string {
  if (fitness === "pr-fitness") {
    const repo = args[0];
    const pr = args[1];
    if (repo !== undefined && pr !== undefined) {
      return `pr-${sanitize(repo)}-${sanitize(pr)}`;
    }
  }
  return `${sanitize(fitness)}-${djb2(args.join(" "))}`;
}

// ---------------------------------------------------------------------------

function haltToExitCode(status: HaltStatus): number {
  switch (status) {
    case "success":
      return 0;
    case "stalled":
      return 1;
    case "timeout":
      return 2;
    case "hil":
      return 3;
    case "error":
      return 4;
    case "llm_needed":
      return 5;
    case "pr_terminal":
      return 6;
    case "cancelled":
      return 7;
    case "fitness_unavailable":
      return 8;
  }
}

// ---------------------------------------------------------------------------

/**
 * Build a resume command in skill-form — what a user or outer LLM agent
 * types as a prompt. NOT the underlying CLI argv. Leading `/` prefixes
 * are how Claude Code identifies skill invocations.
 */
function buildResumeCmd(
  fitness: string,
  fitnessArgs: readonly string[],
): readonly string[] {
  return ["/converge", `/${fitness}`, ...fitnessArgs];
}

/**
 * When pr-fitness is invoked with just a PR number (no owner/repo),
 * infer the repo from the current directory's git remote via `gh`.
 * Returns the normalized args with the repo prepended.
 */
function normalizePrFitnessArgs(
  fitness: string,
  args: readonly string[],
): readonly string[] {
  if (fitness !== "pr-fitness") return args;
  if (args.length !== 1 || !/^\d+$/.test(args[0] ?? "")) return args;
  try {
    const repo = execFileSync(
      "gh",
      ["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
      { encoding: "utf8", timeout: 10_000 },
    ).trim();
    if (repo.length > 0) {
      return [repo, ...args];
    }
  } catch {
    // Fall through — the missing-arg error from pr-fitness will be
    // more informative than a generic "couldn't detect repo."
  }
  return args;
}

async function main(): Promise<void> {
  const parsed = parseArgs(process.argv.slice(2));
  setVerbose(parsed.verbose);

  // Best-effort, non-blocking stale-session sweep.
  gcStaleSessions(SEVEN_DAYS_MS).catch(() => undefined);

  const fitnessArgs = normalizePrFitnessArgs(
    parsed.fitness,
    parsed.fitnessArgs,
  );
  const sessionId = deriveSessionId(parsed.fitness, fitnessArgs);
  const resumeCmd = buildResumeCmd(parsed.fitness, fitnessArgs);
  const prProgressTarget = detectPrProgressTarget(parsed.fitness, fitnessArgs);
  process.stderr.write(`session: /tmp/converge/${sessionId}/\n`);

  const abortController = new AbortController();
  const onSignal = (): void => {
    abortController.abort();
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);

  let report: HaltReport;
  try {
    report = await converge({
      fitness: parsed.fitness,
      args: fitnessArgs,
      maxIterations: parsed.maxIterations,
      sessionId,
      resumeCmd,
      prProgressTarget,
      signal: abortController.signal,
    });
  } catch (err) {
    if (err instanceof LockHeldError) {
      process.stderr.write(
        `converge: session locked by pid ${String(err.pid)} (${err.lockPath})\n`,
      );
      process.exit(9);
    }
    throw err;
  }

  process.exit(haltToExitCode(report.status));
}

main().catch((error: unknown) => {
  if (error instanceof PreconditionError) {
    die(error.message);
  }
  if (error instanceof LockHeldError) {
    process.stderr.write(
      `converge: session locked by pid ${String(error.pid)} (${error.lockPath})\n`,
    );
    process.exit(9);
  }
  if (error instanceof Error) {
    process.stderr.write(`converge: ${error.message}\n`);
    if (process.env["VERBOSE"] !== undefined || process.env["DEBUG"]) {
      process.stderr.write(`${error.stack ?? ""}\n`);
    }
    process.exit(1);
  }
  process.stderr.write(`converge: unexpected error: ${String(error)}\n`);
  process.exit(1);
});
