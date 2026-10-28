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
pub(crate) mod copilot_verdict;
pub(crate) mod review_class;
pub(crate) mod review_docs;
pub(crate) mod sync_pull_request_metadata;

use ooda_core::Wake;
use ooda_core::wait::{self, ChangeProbe, WaitEnd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use crate::decide::action::{Action, ActionEffect, ActionKind};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::observe::github::probe::GitHubProbe;
use crate::orient::state::WIP_LABEL;
use ooda_core::{FileLock, SpawnError, SpawnLimits, run_with_limits};

/// `gt sync` rebases the local stack onto the latest base and may
/// fetch refs from the remote, so 120s gives room for a multi-PR
/// stack on a slow upstream. A wedged `gt sync` surfaces as
/// [`ActError::GraphiteSync`] with an explicit timeout marker so
/// the agent path that owns triage sees the deadline name rather
/// than a bare io error.
const GT_SYNC_DEADLINE: Duration = Duration::from_mins(2);

/// Per-stream byte cap for `gt sync`. Graphite emits progress lines
/// and branch summaries; 2 MiB covers the noisiest stacks observed
/// in practice while keeping a misbehaving `gt` from growing the
/// parent's address space without bound.
const GT_SYNC_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Build the standard per-call limits for `gt sync`.
fn gt_sync_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GT_SYNC_DEADLINE,
        max_stdout_bytes: GT_SYNC_MAX_BYTES,
        max_stderr_bytes: GT_SYNC_MAX_BYTES,
    }
}

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

/// Act-stage context: the target PR and the per-PR side-effect
/// surface.
///
/// `action_lock_path` is the per-PR sidecar path obtained from
/// [`crate::recorder::Recorder::action_lock_path`]; the runner
/// refreshes it every iteration, which re-creates the per-PR index
/// directory it lives in. An advisory [`FileLock`] is acquired on it
/// before any side-effecting work runs and released when [`act`]
/// returns.
///
/// `repo_root` is the resolved working tree the driver targets.
/// Every `gt` subprocess pins to it so a caller invoking the binary
/// from a sibling repo cannot have `gt sync` rewrite that sibling's
/// stack. See [`crate::resolve_repo_root`] for the resolution
/// policy.
///
/// `probe` watches the pull request during an observing wait,
/// polled every `probe_tick`; the runner reseeds it from every
/// observe. A change ends the wait no sooner than `wake_after` — the
/// last observe's start plus `observe_spacing` — so a pull request
/// that never stops changing cannot drive observes back to back.
/// `wait_deadline`, when set,
/// is the deadline the next wait resumes instead of starting a fresh
/// interval; [`act`] consumes it.
#[derive(Debug)]
pub(crate) struct ActContext {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub action_lock_path: PathBuf,
    pub repo_root: PathBuf,
    pub probe: GitHubProbe,
    pub probe_tick: Duration,
    pub observe_spacing: Duration,
    pub wake_after: Option<Instant>,
    pub wait_deadline: Option<Instant>,
}

/// What [`act`] did: applied a side effect, or waited and ended for
/// the stated reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Acted {
    Applied,
    Waited(WaitEnd),
}

/// Realise one action's side effect; the caller re-iterates on Ok.
///
/// `Full` actions acquire the per-PR action lock before dispatching;
/// `Wait` actions skip it (sleeping has no side effect); handoff
/// arms never reach this stage.
pub(crate) fn act(action: &Action, ctx: &mut ActContext) -> Result<Acted, ActError> {
    match &action.effect {
        ActionEffect::Full { .. } => {
            let _lock = FileLock::acquire(&ctx.action_lock_path).map_err(ActError::Lock)?;
            run_full(&action.kind, ctx).map(|()| Acted::Applied)
        }
        ActionEffect::Wait { interval, wake, .. } => {
            let deadline = ctx
                .wait_deadline
                .take()
                .unwrap_or_else(|| Instant::now() + interval.as_duration());
            let ended = wait(deadline, *wake, ctx.probe_tick, &mut ctx.probe);
            Ok(Acted::Waited(hold_until(ended, ctx.wake_after, deadline)))
        }
        ActionEffect::Agent { .. } | ActionEffect::Human { .. } => {
            Err(ActError::UnsupportedAutomation)
        }
    }
}

/// Start-to-start spacing of change-probe passes when one process
/// watches one pull request.
pub(crate) const PROBE_TICK: Duration = Duration::from_secs(5);

/// Least spacing between observe starts a change can force when one
/// process watches one pull request: at most three observes a minute,
/// the rate a full observe's ~25 calls can sustain inside the hourly
/// primary quota.
pub(crate) const OBSERVE_SPACING: Duration = Duration::from_secs(20);

/// Wait until `deadline`, ending early on a trapped shutdown signal
/// or — for [`Wake::OnChange`] — on a change `probe` reports.
pub(crate) fn wait(
    deadline: Instant,
    wake: Wake,
    tick: Duration,
    probe: &mut dyn ChangeProbe,
) -> WaitEnd {
    wait::wait_until(
        deadline,
        wake,
        tick,
        probe,
        &|| crate::signal::check_shutdown().is_some(),
        &wait::SystemClock,
    )
}

/// Hold a change-ended wait until `wake_after`, never past `deadline`.
/// A shutdown signal during the hold ends it as interrupted.
fn hold_until(ended: WaitEnd, wake_after: Option<Instant>, deadline: Instant) -> WaitEnd {
    let WaitEnd::Changed { .. } = &ended else {
        return ended;
    };
    let Some(until) = wake_after.map(|w| w.min(deadline)) else {
        return ended;
    };
    match wait(until, Wake::Elapsed, PROBE_TICK, &mut wait::NeverChanges) {
        WaitEnd::Interrupted => WaitEnd::Interrupted,
        _ => ended,
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
        ActionKind::SyncGraphiteStack { .. } => run_graphite_sync(&ctx.repo_root)?,
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
    let out = run_with_limits(&mut build_gt_sync_command(repo_root), gt_sync_limits())
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
        SpawnError::OutputTooLarge {
            stream,
            limit,
            killed,
        } => format!(
            "`gt sync` {stream} exceeded {limit}-byte cap ({})",
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
