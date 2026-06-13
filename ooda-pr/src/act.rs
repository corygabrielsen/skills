//! Act stage: realise an action's side effect.
//!
//! Domain invariant: only the two driver-side action effects reach
//! this stage. Decide is responsible for halting on the external-
//! resolver arms (Agent / Human) before they get here; an
//! external-resolver action arriving at this boundary is a
//! programmer error and surfaces as `UnsupportedAutomation`.
//!
//! # Per-PR action lock
//!
//! Every mutating action acquires an advisory [`FileLock`] on the
//! per-PR action-lock sidecar before dispatching. The lock is
//! released by RAII Drop when the action returns. This serialises
//! concurrent OODA invocations against the same PR — two drivers
//! cannot push, re-request, or mark-ready in the same instant.
//! Wait actions skip the lock; sleeping has no side effect.

pub(crate) mod address_claude_review;
mod ci;
pub(crate) mod closeout;
mod copilot;
pub(crate) mod review_docs;
pub(crate) mod sync_pull_request_metadata;

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::decide::action::{Action, ActionEffect, ActionKind};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::state::WIP_LABEL;
use ooda_core::{FileLock, SpawnError, run_with_deadline};

/// `gt sync` rebases the local stack onto the latest base and may
/// fetch refs from the remote, so 120s gives room for a multi-PR
/// stack on a slow upstream. A wedged `gt sync` surfaces as
/// [`ActError::GraphiteSync`] with an explicit timeout marker so
/// the agent path that owns triage sees the deadline name rather
/// than a bare io error.
const GT_SYNC_DEADLINE: Duration = Duration::from_mins(2);

#[derive(Debug)]
pub enum ActError {
    /// An external-resolver action reached the driver. Decide is
    /// contractually obliged to halt on those; reaching here is a
    /// programmer error rather than a runtime condition.
    UnsupportedAutomation,
    /// Subprocess invocation for a driver-side action failed.
    Gh(GhError),
    /// Failed to acquire the per-PR action lock. The sidecar open
    /// or `flock` syscall failed; concurrent-invocation exclusion
    /// could not be established for this action.
    Lock(std::io::Error),
    /// `gt sync` subprocess failed. Surfaced with stderr so the
    /// agent path that owns triage sees the underlying reason.
    GraphiteSync(String),
}

impl std::fmt::Display for ActError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAutomation => {
                write!(
                    f,
                    "act received an Agent/Human action — decide must halt those"
                )
            }
            Self::Gh(e) => write!(f, "{e}"),
            Self::Lock(e) => write!(f, "acquire per-PR action lock: {e}"),
            Self::GraphiteSync(stderr) => write!(f, "`gt sync` failed: {stderr}"),
        }
    }
}

impl std::error::Error for ActError {}

impl From<GhError> for ActError {
    fn from(e: GhError) -> Self {
        Self::Gh(e)
    }
}

/// Realise one action's side effect; the caller re-iterates on Ok.
///
/// `action_lock_path` is the per-PR sidecar path obtained from
/// [`crate::recorder::Recorder::action_lock_path`]; an advisory
/// [`FileLock`] is acquired on it before any side-effecting work
/// runs and released when this function returns.
///
/// `repo_root` is the resolved working tree the driver targets.
/// Every `gt` subprocess pins to it so a caller invoking the binary
/// from a sibling repo cannot have `gt sync` rewrite that sibling's
/// stack. See [`crate::resolve_repo_root`] for the resolution
/// policy.
pub(crate) fn act(
    action: &Action,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    action_lock_path: &Path,
    repo_root: &Path,
) -> Result<(), ActError> {
    match &action.effect {
        ActionEffect::Full { .. } => {
            let _lock = FileLock::acquire(action_lock_path).map_err(ActError::Lock)?;
            run_full(&action.kind, slug, pr, repo_root)
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

fn run_full(
    kind: &ActionKind,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    repo_root: &Path,
) -> Result<(), ActError> {
    // Borrow targets for the subprocess's borrowed argv must
    // outlive the call.
    let pr_s = pr.to_string();
    let slug_s = slug.to_string();
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
        ActionKind::RerequestCopilot { .. } => copilot::rerequest_copilot(slug, pr)?,
        ActionKind::ReRunWorkflow { checks } => {
            // Fail-fast on the first per-check error; the next
            // iteration re-observes from a fresh upstream state.
            for c in checks {
                ci::rerun_workflow(slug, &c.run_id)?;
            }
        }
        ActionKind::SyncGraphiteStack { .. } => run_graphite_sync(repo_root)?,
        _ => return Err(ActError::UnsupportedAutomation),
    }
    Ok(())
}

/// Invoke `gt sync` inside `repo_root`. Graphite rebases the local
/// stack onto the latest base; the next observe pass picks up the
/// resulting SHA and the post-observe sticky write normalises the
/// divergence signal. Pinning to `repo_root` rather than process
/// CWD prevents a sibling-repo invocation from rewriting the wrong
/// stack — see the module-level threading doc on [`act`].
fn run_graphite_sync(repo_root: &Path) -> Result<(), ActError> {
    let out = run_with_deadline(&mut build_gt_sync_command(repo_root), GT_SYNC_DEADLINE)
        .map_err(format_gt_sync_spawn_error)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ActError::GraphiteSync(stderr));
    }
    Ok(())
}

/// Render a [`SpawnError`] into the [`ActError::GraphiteSync`]
/// payload. The agent-handoff prose includes a timeout marker so
/// triage prompts surface the deadline name rather than a bare io
/// error.
fn format_gt_sync_spawn_error(err: SpawnError) -> ActError {
    let msg = match err {
        SpawnError::Spawn(e) => format!("spawn `gt sync`: {e}"),
        SpawnError::Timeout { deadline, killed } => format!(
            "`gt sync` timed out after {}s ({})",
            deadline.as_secs(),
            if killed { "killed" } else { "kill failed" }
        ),
        SpawnError::Read(e) => format!("read `gt sync` output pipe: {e}"),
        SpawnError::Wait(e) => format!("wait on `gt sync` subprocess: {e}"),
    };
    ActError::GraphiteSync(msg)
}

/// Construct the `gt sync` command pinned to `repo_root`. Split for
/// the same CWD-scoping smoke test as the observe-side `gt` probes.
fn build_gt_sync_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new("gt");
    cmd.current_dir(repo_root).arg("sync");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gt_sync_command_targets_repo_root() {
        // `Command`'s `Debug` impl prefixes the rendered argv with
        // `cd "<path>" && ` when `current_dir` is set. Pin the
        // CWD-scoping invariant without spawning a real `gt`.
        let dir = std::env::temp_dir();
        let cmd = build_gt_sync_command(&dir);
        let rendered = format!("{cmd:?}");
        let needle = format!("cd {dir:?}");
        assert!(
            rendered.contains(&needle),
            "expected {needle:?} in {rendered:?}",
        );
    }
}
