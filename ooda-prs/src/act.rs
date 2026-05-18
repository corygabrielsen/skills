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
use std::thread;

use crate::decide::action::{Action, ActionEffect, ActionKind};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::state::WIP_LABEL;
use ooda_core::FileLock;

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
pub(crate) fn act(
    action: &Action,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    action_lock_path: &Path,
) -> Result<(), ActError> {
    match &action.effect {
        ActionEffect::Full { .. } => {
            let _lock = FileLock::acquire(action_lock_path).map_err(ActError::Lock)?;
            run_full(&action.kind, slug, pr)
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

fn run_full(kind: &ActionKind, slug: &RepoSlug, pr: PullRequestNumber) -> Result<(), ActError> {
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
        _ => return Err(ActError::UnsupportedAutomation),
    }
    Ok(())
}
