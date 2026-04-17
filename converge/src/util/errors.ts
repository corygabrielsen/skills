/**
 * Typed error classes for /converge.
 *
 * Each class carries exactly the structured context its catch sites need —
 * nothing more. Stringly-typed `.message` is for humans; the readonly fields
 * are for programmatic dispatch.
 */

/** A runtime precondition failed (invalid input, missing prerequisite). */
export class PreconditionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PreconditionError";
  }
}

/**
 * Fitness invocation failed after all retries (rate limit, transient 5xx,
 * parse failure). Terminal for the /converge loop — mapped to
 * `HaltStatus: "fitness_unavailable"`.
 */
export class FitnessUnavailableError extends Error {
  readonly attempts: number;
  readonly lastStderr: string;

  constructor(attempts: number, lastStderr: string) {
    super(
      `fitness unavailable after ${String(attempts)} attempt${attempts === 1 ? "" : "s"}`,
    );
    this.name = "FitnessUnavailableError";
    this.attempts = attempts;
    this.lastStderr = lastStderr;
  }
}

/**
 * A `full`-automation action's subprocess exited non-zero, timed out, or
 * otherwise failed. Carries the argv's originating action kind for
 * telemetry.
 */
export class ExecuteError extends Error {
  readonly actionKind: string;
  readonly exitCode: number | null;
  readonly stderr: string;

  constructor(
    actionKind: string,
    exitCode: number | null,
    stderr: string,
    message?: string,
  ) {
    const trimmed = stderr.trim();
    const detail = trimmed ? `\n  ${trimmed.split("\n").join("\n  ")}` : "";
    super(
      message ??
        `execute failed (${actionKind}, exit ${String(exitCode ?? "?")})${detail}`,
    );
    this.name = "ExecuteError";
    this.actionKind = actionKind;
    this.exitCode = exitCode;
    this.stderr = stderr;
  }
}

/**
 * Session lock is held by another live process. Carries the conflicting
 * PID so the caller can surface it to the human.
 */
export class LockHeldError extends Error {
  readonly pid: number;
  readonly lockPath: string;

  constructor(pid: number, lockPath: string) {
    super(`session lock held by pid ${String(pid)} at ${lockPath}`);
    this.name = "LockHeldError";
    this.pid = pid;
    this.lockPath = lockPath;
  }
}
