//! `ooda-attest` — write per-axis attestations at current HEAD.
//!
//! Each subcommand corresponds to one attestation axis. Operation:
//! resolve HEAD via `git rev-parse` inside the resolved `repo_root`,
//! then write the per-axis attestation file under
//! `<state-root>/<pr-id>/`. State-root resolution defers to
//! [`ooda_state::resolve_state_root`]; repo-root resolution mirrors
//! the OODA-PR siblings (F6): `--repo-root <PATH>` canonicalizes,
//! else `git -C <cwd> rev-parse --show-toplevel` derives it.
//!
//! Pinning matters: the per-PR path encodes only `--pr-id` (digits)
//! and `--state-root`, not repo identity. Without `current_dir()` on
//! the `git rev-parse HEAD` subprocess, an invocation from a sibling
//! repo would write the wrong HEAD SHA into the attestation —
//! defeating the integrity property the binary exists to provide.
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! |-----:|---------|
//! |    0 | success |
//! |    2 | argument parse failure (clap default) |
//! |   64 | invalid `--pr-id` / `--state-root` / `--repo-root` |
//! |   65 | `git rev-parse HEAD` failure or malformed SHA |
//! |   70 | write failure (IO / serialization) |
//! |    1 | fallback |

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use clap::{Parser, Subcommand};
use ooda_core::attest::{
    AttestError, write_claude_review_atomic, write_closeout_atomic, write_doc_review_atomic,
    write_pull_request_metadata_atomic,
};
use ooda_core::{SpawnError, run_with_deadline};
use ooda_state::resolve_state_root as resolve_ooda_state_root;

/// Per-call deadline for the two local `git rev-parse` probes
/// (`HEAD` and `--show-toplevel`). Both are zero-network local
/// operations that print one line; 10s is a generous cap that
/// still surfaces a wedged git instead of letting it stall the
/// attestation write.
const GIT_REV_PARSE_DEADLINE: Duration = Duration::from_secs(10);

const EXIT_VALIDATION: u8 = 64;
const EXIT_GIT: u8 = 65;
const EXIT_WRITE: u8 = 70;
const EXIT_FALLBACK: u8 = 1;

const PULL_REQUEST_METADATA_FILE: &str = "pr_meta_attest.json";
const DOC_REVIEW_FILE: &str = "doc_review_attest.json";
const CLAUDE_REVIEW_FILE: &str = "claude_review_attest.json";
const CLOSEOUT_FILE: &str = "closeout_attest.json";

#[derive(Parser, Debug)]
#[command(name = "ooda-attest", about = "Write OODA attestation files", version)]
struct Cli {
    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand, Debug)]
enum SubCmd {
    /// Attest the PR-metadata axis at current HEAD.
    #[command(name = "pr-meta")]
    PullRequestMetadata {
        /// PR number (digits only).
        #[arg(long)]
        pr_id: String,

        /// State-root directory; the per-PR subdir is created on
        /// demand. When omitted, resolved per
        /// [`ooda_state::resolve_state_root`].
        #[arg(long)]
        state_root: Option<PathBuf>,

        /// Target working tree for the `git rev-parse HEAD`
        /// subprocess. When omitted, derived from CWD via
        /// `git rev-parse --show-toplevel`. Pinning is required so
        /// an invocation from a sibling repo cannot spoof the
        /// attested SHA: see crate docs.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },

    /// Attest the doc / comment hygiene axis at current HEAD.
    #[command(name = "doc-review")]
    DocReview {
        /// PR number (digits only).
        #[arg(long)]
        pr_id: String,

        /// State-root directory; see `pr-meta`.
        #[arg(long)]
        state_root: Option<PathBuf>,

        /// Target working tree; see `pr-meta`.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },

    /// Attest the Claude-review axis at current HEAD.
    #[command(name = "claude-review")]
    ClaudeReview {
        /// PR number (digits only).
        #[arg(long)]
        pr_id: String,

        /// State-root directory; see `pr-meta`.
        #[arg(long)]
        state_root: Option<PathBuf>,

        /// Target working tree; see `pr-meta`.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },

    /// Attest the closeout (final sign-off) axis at current HEAD.
    /// Strictly conditional on every other axis being silent.
    #[command(name = "closeout")]
    Closeout {
        /// PR number (digits only).
        #[arg(long)]
        pr_id: String,

        /// State-root directory; see `pr-meta`.
        #[arg(long)]
        state_root: Option<PathBuf>,

        /// Target working tree; see `pr-meta`.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        SubCmd::PullRequestMetadata {
            pr_id,
            state_root,
            repo_root,
        } => run_attest(
            &pr_id,
            state_root.as_deref(),
            repo_root,
            PULL_REQUEST_METADATA_FILE,
            write_pull_request_metadata_atomic,
        ),
        SubCmd::DocReview {
            pr_id,
            state_root,
            repo_root,
        } => run_attest(
            &pr_id,
            state_root.as_deref(),
            repo_root,
            DOC_REVIEW_FILE,
            write_doc_review_atomic,
        ),
        SubCmd::ClaudeReview {
            pr_id,
            state_root,
            repo_root,
        } => run_attest(
            &pr_id,
            state_root.as_deref(),
            repo_root,
            CLAUDE_REVIEW_FILE,
            write_claude_review_atomic,
        ),
        SubCmd::Closeout {
            pr_id,
            state_root,
            repo_root,
        } => run_attest(
            &pr_id,
            state_root.as_deref(),
            repo_root,
            CLOSEOUT_FILE,
            write_closeout_atomic,
        ),
    }
}

fn run_attest<F, T>(
    pr_id: &str,
    state_root: Option<&Path>,
    repo_root: Option<PathBuf>,
    filename: &str,
    writer: F,
) -> ExitCode
where
    F: FnOnce(&Path, String) -> Result<T, AttestError>,
{
    let pr_id = match validate_pr_id(pr_id) {
        Ok(s) => s,
        Err(msg) => return fail(EXIT_VALIDATION, &msg),
    };
    let state_root = match resolve_state_root(state_root) {
        Ok(p) => p,
        Err(msg) => return fail(EXIT_VALIDATION, &msg),
    };
    let repo_root = match resolve_repo_root(repo_root) {
        Ok(p) => p,
        Err(e) => return fail(EXIT_VALIDATION, &e.to_string()),
    };
    let sha = match read_head_sha(&repo_root) {
        Ok(s) => s,
        Err(msg) => return fail(EXIT_GIT, &msg),
    };

    let path = state_root.join(pr_id).join(filename);
    match writer(&path, sha.clone()) {
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

    let resolved = resolve_ooda_state_root(None);
    // 0o700 on the resolved state root and any intermediate
    // components — keep observation snapshots and attestation files
    // off the default umask 0o755 path.
    ooda_core::atomic_io::secure_create_dir_all(&resolved).map_err(|e| {
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

fn read_head_sha(repo_root: &Path) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root).args(["rev-parse", "HEAD"]);
    let output = run_with_deadline(&mut cmd, GIT_REV_PARSE_DEADLINE).map_err(|e| match e {
        SpawnError::Spawn(io) => format!("failed to invoke `git rev-parse HEAD`: {io}"),
        SpawnError::Timeout { deadline, .. } => format!(
            "`git rev-parse HEAD` timed out after {}s",
            deadline.as_secs()
        ),
        SpawnError::Read(io) => format!("read `git rev-parse HEAD` output pipe: {io}"),
        SpawnError::Wait(io) => format!("wait on `git rev-parse HEAD` subprocess: {io}"),
    })?;
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

/// Typed failures from [`resolve_repo_root`]. Mirrors the F6 shape
/// shipped in `ooda-pr`, `ooda-prs`, `ooda-pr-codex-review`; not
/// extracted into a shared crate by design — divergence here is
/// cheap, and a shared crate would lock the surface prematurely.
#[derive(Debug)]
enum RepoRootError {
    /// `--repo-root <PATH>` was supplied but the path could not be
    /// canonicalized (typically: does not exist).
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `std::env::current_dir()` failed before CWD-derivation could
    /// proceed.
    CwdUnavailable(std::io::Error),
    /// `git rev-parse --show-toplevel` exited non-zero inside the
    /// resolved CWD: the directory is not part of a git working
    /// tree.
    NotInGitTree { cwd: PathBuf, stderr: String },
    /// The `git` subprocess could not be spawned at all (typically:
    /// `git` is not on `$PATH`).
    GitSpawn(std::io::Error),
    /// `git rev-parse --show-toplevel` did not exit within the
    /// per-call deadline. The helper `SIGKILL`ed and reaped the
    /// child; surfacing the timeout as a distinct variant lets the
    /// boundary diagnostic name the deadline rather than collapse
    /// into a generic spawn failure.
    GitTimeout,
    /// `wait` / `try_wait` on the `git` subprocess reported an OS
    /// error.
    GitWait(std::io::Error),
    /// Reading the `git` subprocess's stdout / stderr pipe failed.
    GitPipe(std::io::Error),
}

impl std::fmt::Display for RepoRootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonicalize { path, source } => {
                write!(f, "--repo-root {}: {source}", path.display())
            }
            Self::CwdUnavailable(e) => write!(f, "current working directory unavailable: {e}"),
            Self::NotInGitTree { cwd, stderr } => {
                let stderr = stderr.replace('\n', " ");
                let suffix = if stderr.is_empty() {
                    String::new()
                } else {
                    format!(" ({stderr})")
                };
                write!(
                    f,
                    "{} is not inside a git working tree; invoke ooda-attest from the target repo's checkout or pass --repo-root <PATH>{suffix}",
                    cwd.display(),
                )
            }
            Self::GitSpawn(e) => write!(
                f,
                "spawn `git rev-parse --show-toplevel`: {e}; install git or pass --repo-root <PATH>",
            ),
            Self::GitTimeout => write!(
                f,
                "`git rev-parse --show-toplevel` timed out after {}s",
                GIT_REV_PARSE_DEADLINE.as_secs()
            ),
            Self::GitWait(e) => {
                write!(f, "wait on `git rev-parse --show-toplevel` subprocess: {e}")
            }
            Self::GitPipe(e) => write!(f, "read `git rev-parse --show-toplevel` output pipe: {e}"),
        }
    }
}

/// Resolve the target working tree for the `git rev-parse HEAD`
/// subprocess. Mirrors the F6 resolver in the OODA-PR siblings:
///   1. `Some(path)` → canonicalize.
///   2. `None` → derive from CWD via `git -C <cwd> rev-parse
///      --show-toplevel`.
///
/// Failing at startup is preferable to silently writing a spoofed
/// HEAD SHA into the attestation file.
fn resolve_repo_root(flag: Option<PathBuf>) -> Result<PathBuf, RepoRootError> {
    let cwd = std::env::current_dir().map_err(RepoRootError::CwdUnavailable)?;
    resolve_repo_root_with_cwd(flag, &cwd)
}

/// Test-facing variant with the CWD injected so the resolver's
/// match arms are reachable without mutating process state.
fn resolve_repo_root_with_cwd(flag: Option<PathBuf>, cwd: &Path) -> Result<PathBuf, RepoRootError> {
    if let Some(p) = flag {
        return p
            .canonicalize()
            .map_err(|source| RepoRootError::Canonicalize { path: p, source });
    }
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(["rev-parse", "--show-toplevel"]);
    let out = run_with_deadline(&mut cmd, GIT_REV_PARSE_DEADLINE).map_err(|e| match e {
        SpawnError::Spawn(io) => RepoRootError::GitSpawn(io),
        SpawnError::Timeout { .. } => RepoRootError::GitTimeout,
        SpawnError::Wait(io) => RepoRootError::GitWait(io),
        SpawnError::Read(io) => RepoRootError::GitPipe(io),
    })?;
    if !out.status.success() {
        return Err(RepoRootError::NotInGitTree {
            cwd: cwd.to_path_buf(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(RepoRootError::NotInGitTree {
            cwd: cwd.to_path_buf(),
            stderr: "`git rev-parse --show-toplevel` returned empty stdout".into(),
        });
    }
    Ok(PathBuf::from(s))
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
    use std::path::Path;
    use std::process::Command;

    use super::{RepoRootError, is_valid_sha, resolve_repo_root_with_cwd, validate_pr_id};

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

    // ── repo_root resolver ────────────────────────────────────────

    #[test]
    fn resolve_repo_root_explicit_flag_canonicalizes() {
        let dir = tempfile::tempdir().unwrap();
        // Pass the path through a `./` indirection to prove
        // canonicalize is doing the work.
        let indirect = dir.path().join(".");
        let resolved = resolve_repo_root_with_cwd(
            Some(indirect),
            // CWD irrelevant when the flag is supplied; pass `/`
            // to prove the flag branch never falls back to git.
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_repo_root_explicit_flag_nonexistent_errors() {
        let bogus = std::env::temp_dir().join("ooda-attest-resolve-nonexistent-XYZZY");
        let _ = std::fs::remove_dir_all(&bogus);
        let err = resolve_repo_root_with_cwd(Some(bogus.clone()), Path::new("/")).unwrap_err();
        match err {
            RepoRootError::Canonicalize { path, .. } => assert_eq!(path, bogus),
            other => panic!("expected Canonicalize, got {other:?}"),
        }
    }

    #[test]
    fn resolve_repo_root_cwd_in_git_tree_returns_toplevel() {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--quiet"])
            .output()
            .expect("spawn git init");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let resolved = resolve_repo_root_with_cwd(None, dir.path()).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap(),
        );
    }

    #[test]
    fn resolve_repo_root_cwd_outside_git_tree_errors() {
        // Defensive premise: tempdir is not normally inside a git
        // working tree. If a sandbox quirk places it inside one
        // (CI / dev-loop with a stray `.git` under TMPDIR), the
        // assertion would misfire; skip cleanly.
        let dir = tempfile::tempdir().unwrap();
        let probe = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "--show-toplevel"])
            .output();
        if let Ok(out) = probe.as_ref()
            && out.status.success()
        {
            eprintln!(
                "skipping: tempdir {} is unexpectedly inside a git tree (env quirk)",
                dir.path().display(),
            );
            return;
        }
        let result = resolve_repo_root_with_cwd(None, dir.path());
        match result {
            Err(RepoRootError::NotInGitTree { .. } | RepoRootError::GitSpawn(_)) => {}
            other => panic!("expected NotInGitTree or GitSpawn, got {other:?}"),
        }
    }
}
