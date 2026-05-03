//! Thin wrapper around the `gh` CLI for the observe stage.
//!
//! Runs `gh` as a subprocess, captures stdout, and deserializes it
//! into a caller-supplied type. Auth, pagination, and transport
//! concerns live with `gh`; this module is purely process + JSON.

use std::process::Command;

use serde::de::DeserializeOwned;

#[derive(Debug)]
pub enum GhError {
    /// Could not spawn `gh` (missing binary, permission denied, …).
    Spawn(std::io::Error),
    /// The endpoint returned HTTP 404. Some endpoints (e.g. legacy
    /// branch protection) use 404 to signal "not configured", so this
    /// is its own variant rather than a generic non-zero exit.
    NotFound,
    /// `gh` exited with a non-zero status for any other reason.
    NonZero {
        code: Option<i32>,
        stderr: String,
    },
    /// `gh` output was not valid JSON matching the expected shape.
    Parse(serde_json::Error),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to spawn `gh`: {e}"),
            Self::NotFound => write!(f, "`gh`: not found (HTTP 404)"),
            Self::NonZero { code, stderr } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
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
            Self::NotFound | Self::NonZero { .. } => None,
            Self::Parse(e) => Some(e),
        }
    }
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
    let stream = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<Vec<T>>();
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
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(GhError::Spawn)?;
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
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(GhError::Spawn)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.contains("HTTP 404") {
            return Err(GhError::NotFound);
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
        assert!(s.contains("bad credentials"), "display should include stderr: {s}");
    }

    #[test]
    fn parse_error_source_is_serde() {
        use std::error::Error;
        let json_err = serde_json::from_str::<u32>("not-a-number").unwrap_err();
        let err = GhError::Parse(json_err);
        assert!(err.source().is_some());
    }
}
