/**
 * Branded newtypes for domain primitives.
 *
 * Each type is a bare `string` (or `number`) at runtime with a
 * compile-time-only `unique symbol` tag. Constructor functions share
 * the type name, validate, and throw `PreconditionError` on invalid
 * input. Zero runtime cost beyond the validation itself.
 */

import { PreconditionError } from "../util/errors.js";

// ---------------------------------------------------------------------------
// GitCommitSha — git commit hash (full 40 hex or short ≥7 hex).
// Distinct from non-git hashes and from other git-object SHAs (tree, blob, tag).
// ---------------------------------------------------------------------------

declare const gitCommitShaBrand: unique symbol;
export type GitCommitSha = string & { readonly [gitCommitShaBrand]: never };

const GIT_COMMIT_SHA_RE = /^[0-9a-f]{7,40}$/;

export function GitCommitSha(raw: string): GitCommitSha {
  if (!GIT_COMMIT_SHA_RE.test(raw)) {
    throw new PreconditionError(`invalid git commit SHA: ${raw}`);
  }
  return raw as GitCommitSha;
}

/** Truncate to first 8 hex chars; result is still a valid `GitCommitSha`. */
export function shortSha(s: GitCommitSha): GitCommitSha {
  return s.slice(0, 8) as GitCommitSha;
}

// ---------------------------------------------------------------------------
// GitHubLogin — GitHub user or bot login.
// Distinct from logins on other systems (OS, SSO, other forges).
// ---------------------------------------------------------------------------

declare const gitHubLoginBrand: unique symbol;
export type GitHubLogin = string & { readonly [gitHubLoginBrand]: never };

const GITHUB_LOGIN_RE = /^[A-Za-z0-9][A-Za-z0-9-]*(\[bot\])?$/;

export function GitHubLogin(raw: string): GitHubLogin {
  if (raw.length === 0 || !GITHUB_LOGIN_RE.test(raw)) {
    throw new PreconditionError(`invalid GitHub login: ${raw}`);
  }
  return raw as GitHubLogin;
}

// ---------------------------------------------------------------------------
// Timestamp — ISO 8601 string (contains `T`, ends with `Z` or ±HH:MM offset).
// ---------------------------------------------------------------------------

declare const timestampBrand: unique symbol;
export type Timestamp = string & { readonly [timestampBrand]: never };

const TIMESTAMP_RE = /^[^T]*T[^T]*(Z|[+-]\d{2}:?\d{2})$/;

export function Timestamp(raw: string): Timestamp {
  if (!TIMESTAMP_RE.test(raw) || Number.isNaN(Date.parse(raw))) {
    throw new PreconditionError(`invalid ISO 8601 timestamp: ${raw}`);
  }
  return raw as Timestamp;
}

/** Current wall-clock time as an ISO 8601 `Timestamp`. */
export function now(): Timestamp {
  return new Date().toISOString() as Timestamp;
}

// ---------------------------------------------------------------------------
// PullRequestNumber — positive integer.
// ---------------------------------------------------------------------------

declare const prNumberBrand: unique symbol;
export type PullRequestNumber = number & { readonly [prNumberBrand]: never };

export function PullRequestNumber(n: number): PullRequestNumber {
  if (!Number.isInteger(n) || n <= 0) {
    throw new PreconditionError(`invalid PR number: ${String(n)}`);
  }
  return n as PullRequestNumber;
}

/** Parse a decimal string into a `PullRequestNumber`, validating in one step. */
export function PullRequestNumberFromString(s: string): PullRequestNumber {
  if (!/^\d+$/.test(s)) {
    throw new PreconditionError(`invalid PR number: ${s}`);
  }
  return PullRequestNumber(Number(s));
}

// ---------------------------------------------------------------------------
// RepoSlug — "owner/name" with restricted character set on each side.
// ---------------------------------------------------------------------------

declare const repoSlugBrand: unique symbol;
export type RepoSlug = string & { readonly [repoSlugBrand]: never };

const REPO_SEGMENT_RE = /^[A-Za-z0-9._-]+$/;

export function RepoSlug(raw: string): RepoSlug {
  const parts = raw.split("/");
  if (parts.length !== 2) {
    throw new PreconditionError(`invalid repo slug: ${raw}`);
  }
  const [ownerPart, namePart] = parts as [string, string];
  if (
    ownerPart.length === 0 ||
    namePart.length === 0 ||
    !REPO_SEGMENT_RE.test(ownerPart) ||
    !REPO_SEGMENT_RE.test(namePart)
  ) {
    throw new PreconditionError(`invalid repo slug: ${raw}`);
  }
  return raw as RepoSlug;
}

export function owner(r: RepoSlug): string {
  const idx = r.indexOf("/");
  return r.slice(0, idx);
}

export function name(r: RepoSlug): string {
  const idx = r.indexOf("/");
  return r.slice(idx + 1);
}
