/**
 * Thin wrapper around the `gh` CLI.
 *
 * Single point of subprocess execution and error classification.
 * Mock this module in tests to make everything deterministic.
 *
 * I₁ invariant: every `gh` call site receives a discriminated
 * `GhResult<T>` — never a thrown exception with opaque stderr.
 * All stderr→error-kind classification lives here and nowhere else.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { PreconditionError } from "./errors.js";

const exec = promisify(execFile);

// ---------------------------------------------------------------------------
// GhError — discriminated union over GitHub CLI failure modes
// ---------------------------------------------------------------------------

/**
 * Classified error from a `gh` subprocess invocation.
 *
 * This is the sole classification boundary: regex on stderr and
 * exit-code patterns are evaluated HERE and nowhere else. Callers
 * pattern-match on `kind`; they never inspect raw stderr.
 */
export type GhError =
  | { readonly kind: "empty" }
  | { readonly kind: "not_found"; readonly detail: string }
  | { readonly kind: "auth"; readonly detail: string }
  | { readonly kind: "rate_limit"; readonly retryAfter: number | null }
  | { readonly kind: "network"; readonly detail: string }
  | {
      readonly kind: "unknown";
      readonly code: number | null;
      readonly stderr: string;
    };

// ---------------------------------------------------------------------------
// GhResult — ok/error coproduct
// ---------------------------------------------------------------------------

export type GhResult<T> =
  | { readonly ok: true; readonly data: T }
  | { readonly ok: false; readonly error: GhError };

// ---------------------------------------------------------------------------
// match — generic catamorphism over any { kind: string } union
// ---------------------------------------------------------------------------

/**
 * Exhaustive pattern match on a discriminated union with a `kind` field.
 *
 * This is a generic catamorphism — not specific to `GhError`. Works on
 * any union whose discriminant is named `kind`.
 */
export function match<D extends { readonly kind: string }, T>(
  value: D,
  arms: { readonly [K in D["kind"]]: (v: Extract<D, { kind: K }>) => T },
): T {
  const kind = value.kind as D["kind"];
  // Safety: the type system guarantees exhaustiveness at call sites.
  // At runtime we just dispatch on the discriminant.
  const arm = arms[kind] as (v: D) => T;
  return arm(value);
}

// ---------------------------------------------------------------------------
// GhErrorMatch — exhaustive handler that forces unknown→never
// ---------------------------------------------------------------------------

/**
 * Exhaustive handler map for `GhError` where `unknown` must diverge.
 *
 * Forces callers to handle every classified variant, and requires that
 * the `unknown` arm throws (returns `never`). This makes unhandled
 * errors a compile-time failure, not a runtime surprise.
 */
export type GhErrorMatch<T> = {
  readonly [K in Exclude<GhError["kind"], "unknown">]: (
    error: Extract<GhError, { kind: K }>,
  ) => T;
} & {
  readonly unknown: (error: Extract<GhError, { kind: "unknown" }>) => never;
};

// ---------------------------------------------------------------------------
// Classification — the sole boundary between raw stderr and typed errors
// ---------------------------------------------------------------------------

/**
 * Classification rules. Order matters: first match wins.
 * Each entry is [pattern, factory]. The pattern tests stderr;
 * the factory produces the `GhError` variant.
 *
 * This table is the ONLY place classification regexes live.
 * If a new gh failure mode appears, add a row here.
 */
const CLASSIFICATION_RULES: ReadonlyArray<
  readonly [RegExp, (stderr: string) => GhError]
> = [
  [
    /Bad credentials|authentication required|not authenticated/i,
    (stderr) => ({ kind: "auth", detail: stderr.trim() }),
  ],
  [
    /rate limit|secondary rate limit/i,
    (stderr) => {
      const m = /Retry-After:\s*(\d+)/i.exec(stderr);
      return {
        kind: "rate_limit",
        retryAfter: m ? Number(m[1]) : null,
      };
    },
  ],
  [
    /ECONNRESET|ETIMEDOUT|ENOTFOUND|EAI_AGAIN/,
    (stderr) => ({ kind: "network", detail: stderr.trim() }),
  ],
  [
    /Could not resolve to a/,
    (stderr) => ({ kind: "not_found", detail: stderr.trim() }),
  ],
  [
    /no required checks reported|no checks reported/i,
    () => ({ kind: "empty" }),
  ],
];

function classifyStderr(code: number | null, stderr: string): GhError {
  for (const [pattern, factory] of CLASSIFICATION_RULES) {
    if (pattern.test(stderr)) {
      return factory(stderr);
    }
  }
  return { kind: "unknown", code, stderr };
}

// ---------------------------------------------------------------------------
// gh() — subprocess execution with classified result
// ---------------------------------------------------------------------------

/** Run `gh` with arguments and return a classified result. */
export async function gh<T>(args: readonly string[]): Promise<GhResult<T>> {
  try {
    const { stdout } = await exec("gh", args, {
      maxBuffer: 10 * 1024 * 1024,
    });
    try {
      return { ok: true, data: JSON.parse(stdout) as T };
    } catch {
      // exit 0 but stdout isn't valid JSON
      return {
        ok: false,
        error: {
          kind: "unknown",
          code: 0,
          stderr: `JSON parse failed on stdout: ${stdout.slice(0, 200)}`,
        },
      };
    }
  } catch (error: unknown) {
    // gh binary not found — this is a setup issue, not a runtime error.
    // Remains a thrown exception (PreconditionError) by design.
    if (isExecError(error) && error.code === "ENOENT") {
      throw new PreconditionError(
        "gh not found (install: https://cli.github.com)",
      );
    }
    if (isExecError(error)) {
      const code = error.code !== undefined ? Number(error.code) : null;
      const stderr = error.stderr ?? "";
      return { ok: false, error: classifyStderr(code, stderr) };
    }
    // Truly unexpected (not an exec error at all) — surface as unknown
    const msg = error instanceof Error ? error.message : String(error);
    return {
      ok: false,
      error: { kind: "unknown", code: null, stderr: msg },
    };
  }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

interface ExecError {
  code?: string | number;
  stderr?: string;
}

function isExecError(error: unknown): error is ExecError {
  return typeof error === "object" && error !== null && "code" in error;
}
