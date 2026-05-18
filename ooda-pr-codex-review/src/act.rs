//! Act stage: realise an action's side effect.
//!
//! Domain invariant: only the two driver-side action effects reach
//! this stage. Decide is responsible for halting on the external-
//! resolver arms (Agent / Human) before they get here; an
//! external-resolver action arriving at this boundary is a
//! programmer error and surfaces as `UnsupportedAutomation`.
//!
//! Runtime configuration travels alongside the action via
//! [`ActContext`] rather than on the action payload, keeping the
//! decide-stage type narrow. Optional axes (codex) attach optional
//! sub-contexts that act draws on only when their action arms fire.

pub(crate) mod address_claude_review;
mod ci;
pub(crate) mod closeout;
mod copilot;
pub(crate) mod review_docs;
pub(crate) mod sync_pull_request_metadata;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::decide::action::{Action, ActionEffect, ActionKind};
use crate::ids::{CodexReasoningLevel, PullRequestNumber, RepoSlug};
use crate::observe::codex::batch_dir as codex_batch_dir;
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::state::WIP_LABEL;

#[derive(Debug)]
pub enum ActError {
    /// An external-resolver action reached the driver. Decide is
    /// contractually obliged to halt on those; reaching here is a
    /// programmer error rather than a runtime condition.
    UnsupportedAutomation,
    /// Subprocess invocation for a driver-side action failed.
    Gh(GhError),
    /// A codex-axis action dispatched while the per-iteration
    /// context lacks the codex sub-context. Programmer error: the
    /// sub-context is the witness that the axis is enabled.
    CodexDisabled,
    /// Codex subprocess spawn or backing I/O failed.
    CodexSpawn { slot: u32, source: std::io::Error },
    /// Failed to acquire the per-PR action lock. The sidecar open
    /// or `flock` syscall failed; concurrent-invocation exclusion
    /// could not be established for this action.
    Lock(std::io::Error),
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
            Self::Lock(e) => write!(f, "acquire per-PR action lock: {e}"),
        }
    }
}

impl std::error::Error for ActError {}

impl From<GhError> for ActError {
    fn from(e: GhError) -> Self {
        Self::Gh(e)
    }
}

/// Codex-axis attachment for [`ActContext`].
///
/// Static-per-invocation fields name the side-effect surface (binary,
/// repo root, batch tree root). `head_sha` and `base_branch` refresh
/// each iteration from observe so the side-effects continue to anchor
/// on the PR's current head and base — without that the batch tree
/// and the spawned argv would silently desynchronise.
///
/// `_lock` is an advisory file lock held FD-tied for the invocation's
/// lifetime; concurrent drivers against the same PR would otherwise
/// race on batch directory writes. The lock releases on process exit
/// by any path (including SIGKILL), so a crashed process never leaves
/// a stale lock that blocks subsequent invocations.
#[derive(Debug)]
pub(crate) struct CodexActContext {
    pub codex_bin: PathBuf,
    pub repo_root: PathBuf,
    /// Root of the per-PR codex batch tree.
    pub codex_pr_root: PathBuf,
    /// Configured spawn count per batch.
    pub n: u32,
    /// PR head SHA at this iteration. Partitions the batch tree
    /// by head — stale heads survive as cache rather than being
    /// overwritten.
    pub head_sha: String,
    /// PR base branch at this iteration. Forwarded to the codex
    /// subprocess so the diff base tracks the PR's recorded base.
    pub base_branch: String,
    /// FD-tied advisory lock. Released on FD close.
    pub _lock: std::fs::File,
}

/// Per-iteration act-stage context. The action enum stays narrow
/// because runtime data lives here.
///
/// `action_lock_path` is the per-PR advisory-lock sidecar target;
/// every mutating action arm acquires a [`ooda_core::FileLock`] on
/// the path before dispatching and releases on Drop. This serialises
/// concurrent OODA invocations against the same PR — distinct from
/// the codex sub-context's `_lock`, which excludes concurrent
/// invocations from sharing the codex spawn directory.
#[derive(Debug)]
pub(crate) struct ActContext {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub action_lock_path: PathBuf,
    pub codex: Option<CodexActContext>,
}

/// Realise one action's side effect; the caller re-iterates on Ok.
///
/// `Full` actions acquire the per-PR action lock before dispatching;
/// `Wait` actions skip it (sleeping has no side effect); handoff
/// arms never reach this stage.
pub(crate) fn act(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match &action.effect {
        ActionEffect::Full { .. } => {
            let _lock =
                ooda_core::FileLock::acquire(&ctx.action_lock_path).map_err(ActError::Lock)?;
            run_full(&action.kind, ctx)
        }
        ActionEffect::Wait { interval, .. } => {
            thread::sleep(interval.as_duration());
            Ok(())
        }
        ActionEffect::Agent { .. } | ActionEffect::Human { .. } => {
            Err(ActError::UnsupportedAutomation)
        }
    }
}

fn run_full(kind: &ActionKind, ctx: &ActContext) -> Result<(), ActError> {
    // Borrow targets for the subprocess's borrowed argv must
    // outlive the call.
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
        ActionKind::RerequestCopilot { .. } => copilot::rerequest_copilot(&ctx.slug, ctx.pr)?,
        ActionKind::ReRunWorkflow { checks } => {
            // Fail-fast on the first per-check error; the next
            // iteration re-observes from a fresh upstream state.
            for c in checks {
                ci::rerun_workflow(&ctx.slug, &c.run_id)?;
            }
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
    level: CodexReasoningLevel,
    n: u32,
) -> Result<(), ActError> {
    let dir = codex_batch_dir(&codex.codex_pr_root, level, &codex.head_sha);
    ooda_core::atomic_io::secure_create_dir_all(&dir)
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    // Per-batch-dir advisory lock. The outer per-PR `.lock`
    // (held FD-tied by CodexActContext) excludes other
    // ooda-pr-codex-review invocations; this inner lock excludes
    // a concurrent observe pass that walks the directory while
    // head_sha.txt and per-slot logs are being (re)written.
    let _batch_lock = ooda_core::FileLock::acquire(&dir.join(".batch.lock"))
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    let mut sha_file = ooda_core::atomic_io::open_secure_truncate(&dir.join("head_sha.txt"))
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

    let codex_args = build_codex_args(level, &codex.base_branch);

    for slot in 1..=n {
        let log_path = dir.join(format!("{}-{slot}.log", level.as_str()));
        let exit_path = dir.join(format!("{}-{slot}.exit", level.as_str()));
        ooda_core::atomic_io::open_secure_truncate(&log_path)
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
        let mut child = cmd
            .spawn()
            .map_err(|source| ActError::CodexSpawn { slot, source })?;
        // Detached reaper thread per child. The observe layer reads
        // `.exit` for completion signal; this thread's only job is
        // to call `waitpid` so the OS reclaims the zombie when the
        // child exits. Dropping `Child` without `wait()` leaves a
        // zombie in the process table until the parent exits.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(())
}

/// Build the codex-subprocess argv. The reasoning level and the
/// PR's recorded base branch are the only per-spawn parameters;
/// everything else is invariant across the batch.
fn build_codex_args(level: CodexReasoningLevel, base_branch: &str) -> Vec<OsString> {
    vec![
        OsString::from("review"),
        OsString::from("--base"),
        OsString::from(base_branch),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_codex_args_renders_review_base_and_reasoning() {
        let args = build_codex_args(CodexReasoningLevel::Low, "main");
        let strs: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert_eq!(
            strs,
            vec![
                "review",
                "--base",
                "main",
                "-c",
                "model_reasoning_effort=\"low\"",
            ]
        );
    }

    #[test]
    fn build_codex_args_passes_high_reasoning() {
        let args = build_codex_args(CodexReasoningLevel::Xhigh, "feature/release");
        let s = args.last().unwrap().to_str().unwrap();
        assert_eq!(s, "model_reasoning_effort=\"xhigh\"");
        let base_pos = args.iter().position(|a| a == "--base").unwrap();
        assert_eq!(args[base_pos + 1], "feature/release");
    }
}
