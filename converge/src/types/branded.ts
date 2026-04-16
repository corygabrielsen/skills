/**
 * Branded newtypes for /converge domain primitives.
 *
 * Each type is a bare `number` (or `string`) at runtime with a
 * compile-time-only `unique symbol` tag. Constructor functions share
 * the type name, validate, and throw `PreconditionError` on invalid
 * input. Zero runtime cost beyond the validation itself.
 */

import { PreconditionError } from "../util/errors.js";

// ---------------------------------------------------------------------------
// Score — finite, non-negative number. Fitness reports emit a `score` and a
// `target`; /converge compares them directly. Skills may define stricter
// brands atop this (e.g. tier ordinal 0..4) — /converge accepts any Score.
// Rejects NaN, +/-Infinity, negatives, and -0.
// ---------------------------------------------------------------------------

declare const scoreBrand: unique symbol;
export type Score = number & { readonly [scoreBrand]: never };

export function Score(n: number): Score {
  if (!Number.isFinite(n) || n < 0 || Object.is(n, -0)) {
    throw new PreconditionError(`invalid score: ${String(n)}`);
  }
  return n as Score;
}

// ---------------------------------------------------------------------------
// PositiveSeconds — integer seconds in (0, 3600]. Used for poll intervals
// and subprocess timeouts. /converge clamps unbranded inputs to this range.
// ---------------------------------------------------------------------------

declare const positiveSecondsBrand: unique symbol;
export type PositiveSeconds = number & {
  readonly [positiveSecondsBrand]: never;
};

export function PositiveSeconds(n: number): PositiveSeconds {
  if (!Number.isInteger(n) || n <= 0 || n > 3600) {
    throw new PreconditionError(`invalid seconds: ${String(n)}`);
  }
  return n as PositiveSeconds;
}

// ---------------------------------------------------------------------------
// JsonValue — JSON-safe recursive union. Used for `LlmAction.context`
// payloads, which are serialized to disk and forwarded to the LLM verbatim.
// ---------------------------------------------------------------------------

export type JsonValue =
  | string
  | number
  | boolean
  | null
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };
