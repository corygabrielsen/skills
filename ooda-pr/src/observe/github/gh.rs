//! Subprocess + JSON wrapper around the host CLI.
//!
//! # Invariants
//!
//! - **Auth/transport off-loaded**: the host CLI owns auth,
//!   pagination, and transport. This module is purely
//!   process-spawn + JSON-decode.
//! - **Rate-limit is data, not error**: rate-limit responses are
//!   typed and surfaced as a distinct error variant so the observe
//!   layer can lift them to outcome-data; transport, parse, and
//!   non-2xx failures remain errors.
//! - **Page-stream tolerance**: paginated fetchers parse a stream
//!   of top-level JSON values rather than a single document, so any
//!   list fetcher works correctly across page boundaries.

use std::fmt::Write as _;
use std::process::Command;

use ooda_core::{PollingInterval, RateLimitHit, RateLimitScope};
use serde::de::DeserializeOwned;

use crate::state;

#[derive(Debug)]
pub enum GhError {
    /// Subprocess could not be spawned.
    Spawn(std::io::Error),
    /// Endpoint returned a not-found response. Some endpoints
    /// overload not-found to mean "unconfigured"; the dedicated
    /// variant lets callers branch on that without parsing stderr.
    NotFound,
    /// Quota-exceeded response, typed by scope. Surfaced as
    /// observe-layer data so decide can emit a wait action instead
    /// of crashing the loop.
    RateLimited(RateLimitHit),
    /// Any other non-zero exit.
    NonZero { code: Option<i32>, stderr: String },
    /// Output did not parse as the expected shape.
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
                let code = code.map_or_else(|| "?".into(), |c| c.to_string());
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

/// Classify a non-2xx response as a typed rate-limit hit, or absent.
///
/// Scope is inferred from the argv shape (the bucket the call
/// consumed); secondary-limit wording overrides scope regardless of
/// bucket. `retry_after` is a conservative floor — the runner waits
/// at least the floor, re-observes, and re-classifies if still
/// throttled.
pub(crate) fn classify_rate_limit(args: &[&str], stderr: &str) -> Option<RateLimitHit> {
    let lower = stderr.to_lowercase();

    // Secondary-before-primary: if both phrases appear, the
    // secondary route wins (shorter back-off) — the conservative
    // path under ambiguity.
    if lower.contains("secondary rate limit") {
        return Some(RateLimitHit {
            scope: RateLimitScope::GitHubSecondary,
            retry_after: PollingInterval::from_secs(60),
        });
    }

    // Primary detection accepts both the canonical phrase and the
    // shortened form as forward-compatibility for minor wording
    // shifts.
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
pub(crate) fn gh_json<T: DeserializeOwned>(args: &[&str]) -> Result<T, GhError> {
    let output = run_raw(args)?;
    serde_json::from_slice(&output.stdout).map_err(GhError::Parse)
}

/// Percent-encode a string for use as one URL path segment.
/// Preserves alphanumerics and unreserved punctuation; encodes
/// every URL-syntax-significant byte. Required for any identifier
/// that may legally contain `/`.
pub(crate) fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => write!(out, "%{b:02X}").expect("writing to String never fails"),
        }
    }
    out
}

/// Decode a paginated call as a concatenated stream of per-page
/// arrays. Required for every paginated fetcher: single-document
/// decoders would silently truncate at page boundaries on busy PRs.
pub(crate) fn gh_json_paginate<T: DeserializeOwned>(args: &[&str]) -> Result<Vec<T>, GhError> {
    let output = run_raw(args)?;
    let mut out: Vec<T> = Vec::new();
    let stream = serde_json::Deserializer::from_slice(&output.stdout).into_iter::<Vec<T>>();
    for page in stream {
        out.extend(page.map_err(GhError::Parse)?);
    }
    Ok(out)
}

/// JSON decoder that tolerates host subcommands which signal status
/// via exit code. Non-zero exit is ignored when stdout parses;
/// otherwise, an optional empty-marker triple (whitespace-only
/// stdout AND non-zero exit AND marker substring in stderr) maps to
/// a caller-supplied default. The triple narrows the default to a
/// documented empty case — transient errors with empty stdout still
/// surface as errors.
pub(crate) fn gh_json_lenient<T: DeserializeOwned>(
    args: &[&str],
    empty_default: Option<(T, &str)>,
) -> Result<T, GhError> {
    let guard = state::tool_call_started("gh", args);
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

/// Run for side effects, discarding stdout. Outcome is the exit
/// code; response body is not consumed.
pub(crate) fn gh_run(args: &[&str]) -> Result<(), GhError> {
    run_raw(args).map(|_| ())
}

fn run_raw(args: &[&str]) -> Result<std::process::Output, GhError> {
    let guard = state::tool_call_started("gh", args);
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
        assert!(s.contains('2'), "display should include exit code: {s}");
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
            std::time::Duration::from_mins(15)
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
