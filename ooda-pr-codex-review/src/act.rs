//! Act stage: execute Full actions, sleep on Wait actions.
//!
//! Decide has already routed Agent and Human actions to Halt — they
//! never reach act. Anything that arrives here is either Full (we
//! run it) or Wait (we sleep `interval` and return).
//!
//! `ActContext` carries per-iteration runtime configuration so the
//! action enum can stay payload-slim. The PR-side fields
//! (`slug`, `pr`) are always populated. The optional `codex` field
//! is `Some` whenever the codex review axis is enabled and supplies
//! the spawn-time data (binary path, repo root, batch dir root,
//! current head SHA).

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::decide::action::{Action, ActionKind, Automation};
use crate::ids::{PullRequestNumber, ReasoningLevel, RepoSlug};
use crate::observe::codex::batch_dir as codex_batch_dir;
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::copilot::COPILOT_REVIEWER_LOGIN;
use crate::orient::state::WIP_LABEL;

#[derive(Debug)]
pub enum ActError {
    /// Decide guarantees act() only sees Full or Wait actions; an
    /// Agent or Human action here is a programmer error.
    UnsupportedAutomation,
    /// `gh` subprocess failed for a Full action.
    Gh(GhError),
    /// A codex review action fired but the runner's `ActContext`
    /// has `codex = None` (codex axis disabled). Programmer error:
    /// the codex axis must be enabled before its actions can dispatch.
    CodexDisabled,
    /// Codex review subprocess spawn / I/O error.
    CodexSpawn { slot: u32, source: std::io::Error },
}

impl std::fmt::Display for ActError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAutomation => write!(
                f,
                "act received an Agent/Human action — decide must halt those"
            ),
            Self::Gh(e) => write!(f, "{e}"),
            Self::CodexDisabled => write!(
                f,
                "codex review action dispatched without an ActContext.codex"
            ),
            Self::CodexSpawn { slot, source } => {
                write!(f, "codex review spawn slot {slot}: {source}")
            }
        }
    }
}

impl std::error::Error for ActError {}

impl From<GhError> for ActError {
    fn from(e: GhError) -> Self {
        Self::Gh(e)
    }
}

/// Codex review side of [`ActContext`]. Static per invocation
/// except `head_sha`, which the runner refreshes from each
/// iteration's observe so the batch directory naming and the
/// `head_sha.txt` stamp track the PR's head.
#[derive(Debug, Clone)]
pub struct CodexActContext {
    pub codex_bin: PathBuf,
    pub repo_root: PathBuf,
    /// `<state-root>/github.com/<owner>/<repo>/prs/<pr>/codex/`
    pub codex_pr_root: PathBuf,
    /// Configured `-n`.
    pub n: u32,
    /// PR head SHA observed this iteration. Drives the batch dir
    /// (one batch tree per head SHA — stale heads survive as cache).
    pub head_sha: String,
}

/// Per-iteration act-stage context.
#[derive(Debug, Clone)]
pub struct ActContext {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub codex: Option<CodexActContext>,
}

/// Execute (or wait for) one action. Returns Ok on success;
/// caller's loop re-iterates after this returns.
pub fn act(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match action.automation {
        Automation::Full => run_full(&action.kind, ctx),
        Automation::Wait { interval } => {
            thread::sleep(interval);
            Ok(())
        }
        Automation::Agent | Automation::Human => Err(ActError::UnsupportedAutomation),
    }
}

fn run_full(kind: &ActionKind, ctx: &ActContext) -> Result<(), ActError> {
    let pr_s = ctx.pr.to_string();
    let slug_s = ctx.slug.to_string();
    match kind {
        ActionKind::MarkReady => gh_run(&["pr", "ready", &pr_s, "-R", &slug_s])?,
        ActionKind::RemoveWipLabel => gh_run(&[
            "pr",
            "edit",
            &pr_s,
            "-R",
            &slug_s,
            "--remove-label",
            WIP_LABEL,
        ])?,
        ActionKind::RerequestCopilot => {
            let path = format!("repos/{}/pulls/{}/requested_reviewers", ctx.slug, ctx.pr);
            let reviewer = format!("reviewers[]={COPILOT_REVIEWER_LOGIN}");
            gh_run(&["api", &path, "--method", "POST", "-f", &reviewer])?;
        }
        ActionKind::RunCodexReviewBatch { level, n } => {
            let codex = ctx.codex.as_ref().ok_or(ActError::CodexDisabled)?;
            spawn_codex_review_batch(codex, *level, *n)?;
        }
        _ => return Err(ActError::UnsupportedAutomation),
    }
    Ok(())
}

fn spawn_codex_review_batch(
    codex: &CodexActContext,
    level: ReasoningLevel,
    n: u32,
) -> Result<(), ActError> {
    let dir = codex_batch_dir(&codex.codex_pr_root, level, &codex.head_sha);
    std::fs::create_dir_all(&dir).map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    let mut sha_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("head_sha.txt"))
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    sha_file
        .write_all(codex.head_sha.as_bytes())
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;

    if should_preflight_path(&codex.codex_bin) && !codex.codex_bin.exists() {
        return Err(ActError::CodexSpawn {
            slot: 0,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", codex.codex_bin.display()),
            ),
        });
    }

    let codex_args = build_codex_args(level);

    for slot in 1..=n {
        let log_path = dir.join(format!("{}-{slot}.log", level.as_str()));
        let exit_path = dir.join(format!("{}-{slot}.exit", level.as_str()));
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|source| ActError::CodexSpawn { slot, source })?;
        if let Err(source) = std::fs::remove_file(&exit_path)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ActError::CodexSpawn { slot, source });
        }

        let mut cmd =
            build_logged_codex_command(&codex.codex_bin, &codex_args, &log_path, &exit_path);
        cmd.current_dir(&codex.repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        cmd.spawn()
            .map_err(|source| ActError::CodexSpawn { slot, source })?;
    }
    Ok(())
}

/// Build the `codex review --base <PR base>` argv. The runner runs
/// the unified binary against a PR whose head is checked out locally,
/// so the `--base` selection drives codex to review the diff between
/// the current worktree and the PR's base branch.
fn build_codex_args(level: ReasoningLevel) -> Vec<OsString> {
    // The runner caller passes `--base <branch>` via the CodexActContext
    // when the underlying repo's PR base is known. For now we read it
    // from a `CODEX_REVIEW_BASE` env var fallback to keep the spawn
    // self-contained; the caller is responsible for setting it before
    // act() runs the spawn.
    let base = std::env::var("CODEX_REVIEW_BASE").unwrap_or_else(|_| "HEAD~1".to_string());
    vec![
        OsString::from("review"),
        OsString::from("--base"),
        OsString::from(base),
        OsString::from("-c"),
        OsString::from(format!("model_reasoning_effort=\"{}\"", level.as_str())),
    ]
}

fn build_logged_codex_command(
    codex_bin: &Path,
    codex_args: &[OsString],
    log_path: &Path,
    exit_path: &Path,
) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(
            r#""$@" > "$OODA_LOG_PATH" 2>&1; code=$?; printf '%s\n' "$code" > "$OODA_EXIT_PATH"; exit "$code""#,
        )
        .arg("ooda-pr-codex-review-child")
        .arg(codex_bin)
        .args(codex_args)
        .env("OODA_LOG_PATH", log_path)
        .env("OODA_EXIT_PATH", exit_path);
    cmd
}

fn should_preflight_path(path: &Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}
