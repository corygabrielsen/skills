//! `ooda-attest` — CLI wrapper around `ooda_core::attest`.
//!
//! Single subcommand: `pr-meta`. Looks up HEAD via `git rev-parse`
//! in the current working directory, then writes the attestation
//! file at `<state-root>/<pr-id>/pr_meta_attest.json`.
//!
//! `--state-root` is optional. When omitted, the state root is
//! resolved via `ooda_core::state_root::resolve_ooda_pr_state_root`:
//! `$OODA_PR_STATE_HOME`, then `$XDG_STATE_HOME/ooda-pr`, then
//! `$HOME/.local/state/ooda-pr`, then `$TMPDIR/ooda-pr`. The
//! resolved directory is created on demand.
//!
//! Exit codes:
//!   0  success
//!   2  clap argument parse failure (clap default; preserved)
//!   64 invalid `--pr-id` / `--state-root` format or existence
//!   65 `git rev-parse HEAD` failure or malformed SHA
//!   70 write failure (IO / serialization)
//!   1  fallback

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use ooda_core::attest::{AttestError, write_pull_request_metadata_atomic};
use ooda_core::state_root::resolve_ooda_pr_state_root;

const EXIT_VALIDATION: u8 = 64;
const EXIT_GIT: u8 = 65;
const EXIT_WRITE: u8 = 70;
const EXIT_FALLBACK: u8 = 1;

const PULL_REQUEST_METADATA_FILE: &str = "pr_meta_attest.json";

#[derive(Parser, Debug)]
#[command(name = "ooda-attest", about = "Write OODA attestation files", version)]
struct Cli {
    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand, Debug)]
enum SubCmd {
    /// Attest that PR title, description, and labels are correct
    /// for the current HEAD.
    #[command(name = "pr-meta")]
    PullRequestMetadata {
        /// PR number (digits only).
        #[arg(long)]
        pr_id: String,

        /// State-root directory; the per-PR subdir is created on
        /// demand by the underlying writer. When omitted, resolved
        /// from `$OODA_PR_STATE_HOME`, `$XDG_STATE_HOME/ooda-pr`,
        /// `$HOME/.local/state/ooda-pr`, or `$TMPDIR/ooda-pr` (in
        /// that order). The resolved directory is created if missing.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        SubCmd::PullRequestMetadata { pr_id, state_root } => {
            run_pull_request_metadata(&pr_id, state_root.as_deref())
        }
    }
}

fn run_pull_request_metadata(pr_id: &str, state_root: Option<&Path>) -> ExitCode {
    let pr_id = match validate_pr_id(pr_id) {
        Ok(s) => s,
        Err(msg) => return fail(EXIT_VALIDATION, &msg),
    };
    let state_root = match resolve_state_root(state_root) {
        Ok(p) => p,
        Err(msg) => return fail(EXIT_VALIDATION, &msg),
    };
    let sha = match read_head_sha() {
        Ok(s) => s,
        Err(msg) => return fail(EXIT_GIT, &msg),
    };

    let path = state_root.join(pr_id).join(PULL_REQUEST_METADATA_FILE);
    match write_pull_request_metadata_atomic(&path, sha.clone()) {
        Ok(_) => {
            println!("{} {}", path.display(), sha);
            ExitCode::SUCCESS
        }
        Err(AttestError::BadShaFormat(s)) => fail(
            EXIT_GIT,
            &format!("git rev-parse HEAD returned malformed SHA: {s:?}"),
        ),
        Err(e @ (AttestError::Io(_) | AttestError::Parse(_))) => fail(
            EXIT_WRITE,
            &format!("failed to write {}: {e}", path.display()),
        ),
        Err(e) => fail(EXIT_FALLBACK, &format!("attestation failed: {e}")),
    }
}

fn validate_pr_id(s: &str) -> Result<&str, String> {
    if s.is_empty() {
        return Err("--pr-id must not be empty".to_string());
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("--pr-id must be digits only (got {s:?})"));
    }
    Ok(s)
}

fn resolve_state_root(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        if !path.exists() {
            return Err(format!("--state-root does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(format!(
                "--state-root is not a directory: {}",
                path.display()
            ));
        }
        return path.canonicalize().map_err(|e| {
            format!(
                "failed to canonicalize --state-root {}: {e}",
                path.display()
            )
        });
    }

    let resolved = resolve_ooda_pr_state_root(None);
    fs::create_dir_all(&resolved).map_err(|e| {
        format!(
            "failed to create resolved state root {}: {e}",
            resolved.display()
        )
    })?;
    resolved.canonicalize().map_err(|e| {
        format!(
            "failed to canonicalize resolved state root {}: {e}",
            resolved.display()
        )
    })
}

fn read_head_sha() -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("failed to invoke `git rev-parse HEAD`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`git rev-parse HEAD` failed (status {}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !is_valid_sha(&sha) {
        return Err(format!(
            "`git rev-parse HEAD` returned non-40-hex output: {sha:?}"
        ));
    }
    Ok(sha)
}

fn is_valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn fail(code: u8, msg: &str) -> ExitCode {
    eprintln!("ooda-attest: {msg}");
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::{is_valid_sha, validate_pr_id};

    #[test]
    fn validate_pr_id_accepts_digits() {
        assert_eq!(validate_pr_id("1").unwrap(), "1");
        assert_eq!(validate_pr_id("12345").unwrap(), "12345");
    }

    #[test]
    fn validate_pr_id_rejects_empty() {
        assert!(validate_pr_id("").is_err());
    }

    #[test]
    fn validate_pr_id_rejects_non_digits() {
        assert!(validate_pr_id("abc").is_err());
        assert!(validate_pr_id("12a").is_err());
        assert!(validate_pr_id("-1").is_err());
        assert!(validate_pr_id(" 1").is_err());
    }

    #[test]
    fn is_valid_sha_accepts_40_lowercase_hex() {
        assert!(is_valid_sha("0123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn is_valid_sha_rejects_wrong_length_or_case() {
        assert!(!is_valid_sha(""));
        assert!(!is_valid_sha("abc"));
        assert!(!is_valid_sha(&"a".repeat(39)));
        assert!(!is_valid_sha(&"a".repeat(41)));
        assert!(!is_valid_sha("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_valid_sha("0123456789abcdef0123456789abcdef0123456g"));
    }
}
