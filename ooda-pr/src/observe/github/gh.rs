//! Thin wrapper around the `gh` CLI for the observe stage.
//!
//! Runs `gh` as a subprocess, captures stdout, and deserializes it
//! into a caller-supplied type. Auth, pagination, and transport
//! concerns live with `gh`; this module is purely process + JSON.

use std::process::Command;

use ooda_core::{PollingInterval, RateLimitHit, RateLimitScope};
use serde::de::DeserializeOwned;

use crate::recorder;

#[derive(Debug)]
pub enum GhError {
    /// Could not spawn `gh` (missing binary, permission denied, …).
    Spawn(std::io::Error),
    /// The endpoint returned HTTP 404. Some endpoints (e.g. legacy
    /// branch protection) use 404 to signal "not configured", so this
    /// is its own variant rather than a generic non-zero exit.
    NotFound,
    /// A rate-limit response from GitHub. The scope identifies which
    /// quota fired (GraphQL primary, REST primary, secondary); the
    /// observe layer surfaces this as a typed observation so decide
    /// can emit a `WaitForRateLimit` action instead of crashing the
    /// loop. See [`classify_rate_limit`] for the detection rules.
    RateLimited(RateLimitHit),
    /// `gh` exited with a non-zero status for any other reason.
    NonZero { code: Option<i32>, stderr: String },
    /// `gh` output was not valid JSON matching the expected shape.
    Parse(serde_json::Error),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to spawn `gh`: {e}"),
            Self::NotFound => write!(f, "`gh`: not found (HTTP 404)"),
            Self::RateLimited(hit) => write!(
                f,
                "`gh`: rate-limited on {} (retry after {:?})",
                hit.scope.name(),
                hit.retry_after.as_duration()
            ),
            Self::NonZero { code, stderr } => {
                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                write!(f, "`gh` exited {code}: {}", stderr.trim())
            }
            Self::Parse(e) => write!(f, "failed to parse `gh` output: {e}"),
        }
    }
}

impl std::error::Error for GhError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) => Some(e),
            Self::NotFound | Self::NonZero { .. } | Self::RateLimited(_) => None,
            Self::Parse(e) => Some(e),
        }
    }
}

/// Detect a rate-limit response in `gh` stderr. Returns `Some(hit)`
/// when the message matches one of GitHub's documented rate-limit
/// signals; `None` otherwise (caller falls back to `NonZero`).
///
/// Scope is determined by inspecting `args`:
///   - `gh api graphql …` → [`RateLimitScope::GitHubGraphqlPrimary`]
///   - any other `gh api …` → [`RateLimitScope::GitHubRestPrimary`]
///   - secondary-limit messages override to
///     [`RateLimitScope::GitHubSecondary`] regardless of bucket.
///
/// `retry_after` is a conservative default — without parsing
/// `X-RateLimit-Reset` from response headers we don't know the exact
/// reset time. Primary defaults to 15 minutes (worst case is ~60 min
/// if we hit at the start of a fresh window); secondary defaults to
/// 60 seconds (GitHub's documented recommendation in their rate-limit
/// docs). Both are floors — the runner sleeps at least this long,
/// re-observes, and re-classifies if still throttled.
pub(crate) fn classify_rate_limit(args: &[&str], stderr: &str) -> Option<RateLimitHit> {
    let lower = stderr.to_lowercase();

    // Secondary rate limits get specific language and a shorter
    // back-off window. Match before primary so a stderr that happens
    // to contain both phrases routes to secondary (lower wait).
    if lower.contains("secondary rate limit") {
        return Some(RateLimitHit {
            scope: RateLimitScope::GitHubSecondary,
            retry_after: PollingInterval::from_secs(60),
        });
    }

    // Primary rate limits: GitHub's response body contains
    // "API rate limit exceeded" verbatim; `gh` passes this through
    // to stderr. Match the broader "rate limit exceeded" as a
    // fall-through for forward compatibility with minor wording
    // changes.
    let primary = lower.contains("api rate limit exceeded")
        || (lower.contains("rate limit exceeded") && !lower.contains("secondary"));
    if primary {
        let scope =
            if args.first().copied() == Some("api") && args.get(1).copied() == Some("graphql") {
                RateLimitScope::GitHubGraphqlPrimary
            } else {
                RateLimitScope::GitHubRestPrimary
            };
        return Some(RateLimitHit {
            scope,
            retry_after: PollingInterval::from_secs(15 * 60),
        });
    }

    None
}

/// Run `gh <args>` and deserialize stdout as JSON into `T`.
pub fn gh_json<T: DeserializeOwned>(args: &[&str]) -> Result<T, GhError> {
    let output = run_raw(args)?;
    serde_json::from_slice(&output.stdout).map_err(GhError::Parse)
}

/// Percent-encode a single REST path segment so that branch names
/// containing `/` (e.g. `release/1.2`) are treated as one segment
/// rather than parsed as additional path components by `gh api`.
/// Encodes the characters that have URL-syntax meaning; passes
/// alphanumerics and the unreserved punctuation through.
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Run a paginated `gh` call and concatenate the per-page arrays
/// into one `Vec<T>`. With `--paginate`, `gh` emits each page as a
/// separate top-level JSON value (multiple `[...]` arrays back to
/// back); `--slurp` would wrap them into one outer array, but
/// `gh` rejects `--slurp` combined with `--jq`. We parse the
/// multi-document stream ourselves instead.
///
/// **Always use this for any new `--paginate` fetcher** —
/// `gh_json` would only see the first page's JSON value and either
/// truncate silently or fail with a trailing-data parse error on
/// PRs busy enough to span pages.
pub fn gh_json_paginate<T: DeserializeOwned>(args: &[&str]) -> Result<Vec<T>, GhError> {
    let output = run_raw(args)?;
    let mut out: Vec<T> = Vec::new();
    let stream = serde_json::Deserializer::from_slice(&output.stdout).into_iter::<Vec<T>>();
    for page in stream {
        out.extend(page.map_err(GhError::Parse)?);
    }
    Ok(out)
}

/// Like `gh_json`, but ignores non-zero exit when stdout parses
/// successfully. Some `gh` subcommands behave this way:
///   - `gh pr checks` exits status 8 when checks are still pending
///     (with valid JSON stdout) and exits status 1 with empty
///     stdout + "no checks reported" stderr when there are none.
///
/// `empty_default` is returned when:
///   1. stdout is empty (only whitespace), AND
///   2. exit is non-zero, AND
///   3. stderr contains `empty_marker` (e.g. `"no checks reported"`).
///
/// All three conditions narrow the default to the documented case
/// — a transient permission/API error with empty stdout still
/// surfaces as `NonZero` rather than masking as empty data.
pub fn gh_json_lenient<T: DeserializeOwned>(
    args: &[&str],
    empty_default: Option<(T, &str)>,
) -> Result<T, GhError> {
    let guard = recorder::tool_call_started("gh", args);
    let output = match Command::new("gh").args(args).output() {
        Ok(output) => {
            if let Some(guard) = guard {
                guard.finish_output(&output);
            }
            output
        }
        Err(e) => {
            if let Some(guard) = guard {
                guard.finish_spawn_error(&e);
            }
            return Err(GhError::Spawn(e));
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if let Some((default, empty_marker)) = empty_default
        && !output.status.success()
        && output.stdout.iter().all(u8::is_ascii_whitespace)
        && stderr.contains(empty_marker)
    {
        return Ok(default);
    }

    match serde_json::from_slice(&output.stdout) {
        Ok(v) => Ok(v),
        Err(parse_err) => {
            if !output.status.success() {
                if stderr.contains("HTTP 404") {
                    return Err(GhError::NotFound);
                }
                if let Some(hit) = classify_rate_limit(args, &stderr) {
                    return Err(GhError::RateLimited(hit));
                }
                return Err(GhError::NonZero {
                    code: output.status.code(),
                    stderr,
                });
            }
            Err(GhError::Parse(parse_err))
        }
    }
}

/// Run `gh <args>` for side effects only — discard stdout.
/// Used by act-stage Full actions (e.g. `gh pr ready`) where the
/// outcome is the exit code, not the response body.
pub fn gh_run(args: &[&str]) -> Result<(), GhError> {
    run_raw(args).map(|_| ())
}

fn run_raw(args: &[&str]) -> Result<std::process::Output, GhError> {
    let guard = recorder::tool_call_started("gh", args);
    let output = match Command::new("gh").args(args).output() {
        Ok(output) => {
            if let Some(guard) = guard {
                guard.finish_output(&output);
            }
            output
        }
        Err(e) => {
            if let Some(guard) = guard {
                guard.finish_spawn_error(&e);
            }
            return Err(GhError::Spawn(e));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.contains("HTTP 404") {
            return Err(GhError::NotFound);
        }
        if let Some(hit) = classify_rate_limit(args, &stderr) {
            return Err(GhError::RateLimited(hit));
        }
        return Err(GhError::NonZero {
            code: output.status.code(),
            stderr,
        });
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_error_displays_exit_code_and_stderr() {
        let err = GhError::NonZero {
            code: Some(2),
            stderr: "bad credentials\n".into(),
        };
        let s = err.to_string();
        assert!(s.contains("2"), "display should include exit code: {s}");
        assert!(
            s.contains("bad credentials"),
            "display should include stderr: {s}"
        );
    }

    #[test]
    fn parse_error_source_is_serde() {
        use std::error::Error;
        let json_err = serde_json::from_str::<u32>("not-a-number").unwrap_err();
        let err = GhError::Parse(json_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn classify_primary_rest_from_api_args() {
        let hit = classify_rate_limit(
            &["api", "/repos/o/r/pulls/1"],
            "HTTP 403: API rate limit exceeded for user ID 1.",
        )
        .expect("primary rate-limit should classify");
        assert_eq!(hit.scope, RateLimitScope::GitHubRestPrimary);
        assert_eq!(
            hit.retry_after.as_duration(),
            std::time::Duration::from_secs(15 * 60)
        );
    }

    #[test]
    fn classify_primary_graphql_from_graphql_arg() {
        let hit = classify_rate_limit(
            &["api", "graphql", "-f", "query=..."],
            "API rate limit exceeded",
        )
        .expect("primary rate-limit should classify");
        assert_eq!(hit.scope, RateLimitScope::GitHubGraphqlPrimary);
    }

    #[test]
    fn classify_secondary_overrides_bucket() {
        // Secondary fires regardless of REST vs GraphQL.
        let rest_secondary = classify_rate_limit(
            &["api", "/repos/o/r/issues"],
            "You have exceeded a secondary rate limit. Please wait a few minutes.",
        )
        .expect("secondary rate-limit should classify");
        assert_eq!(rest_secondary.scope, RateLimitScope::GitHubSecondary);
        let graphql_secondary = classify_rate_limit(
            &["api", "graphql", "-f", "query=..."],
            "secondary rate limit",
        )
        .expect("secondary on graphql still classifies");
        assert_eq!(graphql_secondary.scope, RateLimitScope::GitHubSecondary);
    }

    #[test]
    fn classify_secondary_back_off_is_shorter_than_primary() {
        let secondary = classify_rate_limit(&["api", "/x"], "secondary rate limit").unwrap();
        let primary = classify_rate_limit(&["api", "/x"], "API rate limit exceeded").unwrap();
        assert!(secondary.retry_after.as_duration() < primary.retry_after.as_duration());
    }

    #[test]
    fn classify_returns_none_for_other_errors() {
        assert!(classify_rate_limit(&["api", "/x"], "HTTP 404: Not Found").is_none());
        assert!(classify_rate_limit(&["api", "/x"], "bad credentials").is_none());
        assert!(classify_rate_limit(&["api", "/x"], "").is_none());
    }

    #[test]
    fn rate_limited_error_display_includes_scope() {
        let hit = RateLimitHit {
            scope: RateLimitScope::GitHubGraphqlPrimary,
            retry_after: PollingInterval::from_secs(60),
        };
        let err = GhError::RateLimited(hit);
        let s = err.to_string();
        assert!(s.contains("github/graphql/primary"), "display: {s}");
    }
}
