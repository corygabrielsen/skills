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

// ---------------------------------------------------------------------------
// FitnessId — internal dispatch key for a fitness skill. Always bare,
// never slash-prefixed. Canonicalized at the CLI boundary by stripping
// any leading slashes so `/pr-fitness` and `pr-fitness` resolve the same.
// ---------------------------------------------------------------------------

declare const fitnessIdBrand: unique symbol;
export type FitnessId = string & { readonly [fitnessIdBrand]: never };

export function FitnessId(raw: string): FitnessId {
  const id = raw.replace(/^\/+/, "");
  if (id.length === 0) {
    throw new PreconditionError("empty fitness id");
  }
  return id as FitnessId;
}

export const PR_FITNESS: FitnessId = "pr-fitness" as FitnessId;

// ---------------------------------------------------------------------------
// SkillRef — a skill reference as an LLM types it in a prompt or as it
// appears in a resume_cmd. Always has a leading `/`. Built from a
// FitnessId — the only construction path guarantees the slash is applied
// exactly once.
// ---------------------------------------------------------------------------

declare const skillRefBrand: unique symbol;
export type SkillRef = string & { readonly [skillRefBrand]: never };

export function SkillRef(id: FitnessId): SkillRef {
  return `/${id}` as SkillRef;
}
