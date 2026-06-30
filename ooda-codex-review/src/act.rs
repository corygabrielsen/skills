//! Dispatch in-loop (Full / Wait) actions.
//!
//! Behaviour by effect:
//!
//! - **Wait** — sleep the configured interval.
//! - **Full** — dispatch the per-kind handler.
//! - **Agent / Human** — unreachable; decide must halt these
//!   instead of routing them through act.
//!
//! Spawning a review batch fans out `n` subprocesses, redirects
//! each one's stdout/stderr to a per-slot log file, and records
//! its exit status to a sibling `.exit` file. Other Full handlers
//! return `NotImplemented` until they are wired up.
//!
//! [`ActContext`] supplies the per-invocation environment
//! (log directory, target, working directory, subprocess binary
//! path). The runner threads one through per invocation.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::decide::action::{Action, ActionEffect, ActionKind, CodexReasoningLevel};
use crate::ids::ReviewTarget;

/// Per-invocation environment for `act`. Stable across all
/// iterations of a single `run_loop` call.
#[derive(Debug, Clone)]
pub(crate) struct ActContext {
    /// Directory where per-slot log and exit-status files are
    /// written.
    pub batch_dir: PathBuf,
    /// What slice of changes the review is invoked against.
    pub target: ReviewTarget,
    /// Working directory for spawned subprocesses; usually the
    /// repo's git toplevel.
    pub repo_root: PathBuf,
    /// Path to the review binary. Defaults to bare name (PATH
    /// lookup); tests inject a fake path.
    pub codex_bin: PathBuf,
    /// Shared registry of spawned codex children's process group
    /// IDs. Each [`spawn_codex_reviews`] call places its wrapper
    /// shell into a fresh PGID (= the wrapper's PID, see
    /// `CommandExt::process_group(0)`) and appends the PGID here.
    /// The PGID is the rendezvous point for [`reap_spawned_children`]
    /// to kill the whole `sh → node → codex` subtree at `run_loop`
    /// shutdown — `kill` against just the wrapper PID misses the
    /// node descendants spawned by the codex CLI under `~/.nvm`.
    ///
    /// Cleared (via `take`) after a successful reap so subsequent
    /// calls in the same process are no-ops.
    pub spawned_pgids: Arc<Mutex<Vec<i32>>>,
}

#[derive(Debug)]
pub(crate) enum ActError {
    /// Agent or Human effect routed to act. Decide must halt
    /// these; reaching act is an invariant violation.
    UnsupportedAutomation,
    /// Target reached act in a form the underlying review CLI
    /// cannot execute directly. Surface-level sugar must be
    /// resolved to the underlying form before constructing
    /// [`ActContext`].
    UnsupportedTarget(String),
    /// Handler not yet wired.
    NotImplemented,
    /// Subprocess spawn failed (binary not found, log open
    /// failed, etc.). Carries the failing slot and the underlying
    /// IO error.
    Spawn { slot: u32, source: std::io::Error },
}

impl std::fmt::Display for ActError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAutomation => {
                write!(f, "act received a handoff effect — decide must halt those")
            }
            Self::UnsupportedTarget(msg) => write!(f, "unsupported review target: {msg}"),
            Self::NotImplemented => write!(f, "act handler not yet implemented"),
            Self::Spawn { slot, source } => {
                write!(f, "spawn slot {slot}: {source}")
            }
        }
    }
}

impl std::error::Error for ActError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Dispatch one action against `ctx`. See module doc for the
/// per-effect behaviour table.
pub(crate) fn act(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match &action.effect {
        ActionEffect::Wait { interval, .. } => {
            std::thread::sleep(interval.as_duration());
            Ok(())
        }
        ActionEffect::Full { .. } => dispatch_full(action, ctx),
        ActionEffect::Agent { .. } | ActionEffect::Human { .. } => {
            Err(ActError::UnsupportedAutomation)
        }
    }
}

fn dispatch_full(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match &action.kind {
        ActionKind::RunReviews { level, n } => spawn_codex_reviews(*level, *n, ctx),
        // Ladder transitions and test invocation are not yet
        // wired through act; the recorder mutates ladder state
        // out of band via side-effect commands. Verdict parsing
        // is implicit in observe (the scan returns parsed
        // verdicts in the Complete state).
        _ => Err(ActError::NotImplemented),
    }
}

/// Spawn `n` review subprocesses. Returns immediately after
/// spawn; completion is detected by the observe layer reading log
/// and exit files.
///
/// On partial-spawn failure: already-spawned children are not
/// killed; they run to completion and their logs land. The next
/// observe pass sees this as `Running` with `total < expected`
/// — the in-batch state machine handles it naturally.
fn spawn_codex_reviews(
    level: CodexReasoningLevel,
    n: u32,
    ctx: &ActContext,
) -> Result<(), ActError> {
    ooda_core::atomic_io::secure_create_dir_all(&ctx.batch_dir)
        .map_err(|source| ActError::Spawn { slot: 0, source })?;
    // Per-batch-dir advisory lock. A concurrent observe pass
    // walking this directory while logs are truncated would see
    // a half-truncated file and misclassify the verdict; the
    // lock excludes that window from any reader that takes the
    // same lock cooperatively.
    let _batch_lock = ooda_core::FileLock::acquire(&ctx.batch_dir.join(".batch.lock"))
        .map_err(|source| ActError::Spawn { slot: 0, source })?;

    if should_preflight_path(&ctx.codex_bin) && !ctx.codex_bin.exists() {
        return Err(ActError::Spawn {
            slot: 0,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", ctx.codex_bin.display()),
            ),
        });
    }

    let codex_args = build_codex_args(level, &ctx.target)?;

    for slot in 1..=n {
        let log_path = ctx.batch_dir.join(format!("{}-{slot}.log", level.as_str()));
        let exit_path = ctx
            .batch_dir
            .join(format!("{}-{slot}.exit", level.as_str()));
        ooda_core::atomic_io::open_secure_truncate(&log_path)
            .map_err(|source| ActError::Spawn { slot, source })?;
        if let Err(source) = std::fs::remove_file(&exit_path)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ActError::Spawn { slot, source });
        }

        let mut cmd =
            build_logged_codex_command(&ctx.codex_bin, &codex_args, &log_path, &exit_path);
        cmd.current_dir(&ctx.repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        // Place the wrapper sh into a fresh process group whose
        // ID equals the wrapper's PID. The codex node grandchild
        // inherits the PGID, so a single `killpg(pgid, SIGTERM)`
        // at shutdown reaps the entire `sh → node → codex` subtree.
        // Without this, the wrapper's PID is its own group's PID
        // but the parent's group, so signal-by-group on shutdown
        // would inadvertently target the orchestrator itself.
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|source| ActError::Spawn { slot, source })?;
        // Record the PGID = wrapper PID before handing the `Child`
        // to the reaper thread. The reaper thread takes ownership
        // of the `Child` for waitpid; the PGID is the kill handle
        // that doesn't depend on that ownership.
        let pgid = i32::try_from(child.id()).expect("PID fits in i32 on supported platforms");
        if let Ok(mut pgids) = ctx.spawned_pgids.lock() {
            pgids.push(pgid);
        }
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

/// Reap every codex subtree spawned through this context. Sends
/// `SIGTERM` to each tracked process group, waits `grace` for
/// graceful shutdown (codex shuts down its rmcp service + file
/// watchers on SIGTERM in <1s typical), then escalates to
/// `SIGKILL` for any survivors. Idempotent: calling on an already-
/// reaped context is a no-op.
///
/// Called from the `run_loop` shutdown path so subprocesses do
/// not outlive the parent. Pre-fix, the wrapper `sh` and its node
/// codex descendants kept running after the parent halted —
/// observably so in the empirical state-tree investigation: a slot
/// finished cleanly 12 minutes after its parent's `StuckCapReached`.
pub(crate) fn reap_spawned_children(ctx: &ActContext, grace: Duration) {
    let pgids = match ctx.spawned_pgids.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    if pgids.is_empty() {
        return;
    }
    for pgid in &pgids {
        // Negative PID targets the process group. Best-effort:
        // ESRCH (no such process group, already exited) is fine
        // and not logged; the only failure that matters is EPERM,
        // which would indicate a deeper sandbox issue out of scope
        // for the run_loop shutdown.
        unsafe {
            libc::killpg(*pgid, libc::SIGTERM);
        }
    }
    std::thread::sleep(grace);
    for pgid in &pgids {
        unsafe {
            libc::killpg(*pgid, libc::SIGKILL);
        }
    }
}

/// Build the review-CLI argv for the given target and reasoning
/// level. Pure — no I/O.
pub(crate) fn build_codex_args(
    level: CodexReasoningLevel,
    target: &ReviewTarget,
) -> Result<Vec<OsString>, ActError> {
    let mut args = vec![OsString::from("review")];
    match target {
        ReviewTarget::Uncommitted => {
            args.push(OsString::from("--uncommitted"));
        }
        ReviewTarget::Base(branch) => {
            args.push(OsString::from("--base"));
            args.push(OsString::from(branch.as_str()));
        }
        ReviewTarget::Commit(sha) => {
            args.push(OsString::from("--commit"));
            args.push(OsString::from(sha.as_str()));
        }
        ReviewTarget::Pr(num) => {
            return Err(ActError::UnsupportedTarget(format!(
                "--pr {num} must be resolved to its base branch before spawning codex"
            )));
        }
    }
    args.push(OsString::from("-c"));
    args.push(OsString::from(format!(
        "model_reasoning_effort=\"{}\"",
        level.as_str()
    )));
    Ok(args)
}

/// Build a direct review-CLI command for unit tests. The runtime
/// path wraps the command in a shell that captures exit status
/// so observe can see it after this process returns.
#[cfg(test)]
pub(crate) fn build_codex_command(
    codex_bin: &std::path::Path,
    level: CodexReasoningLevel,
    target: &ReviewTarget,
) -> Result<Command, ActError> {
    let mut cmd = Command::new(codex_bin);
    cmd.args(build_codex_args(level, target)?);
    Ok(cmd)
}

fn build_logged_codex_command(
    codex_bin: &std::path::Path,
    codex_args: &[OsString],
    log_path: &std::path::Path,
    exit_path: &std::path::Path,
) -> Command {
    let mut cmd = Command::new("/bin/sh");
    // `umask 077` before any redirection so the shell's `>`
    // creates `$OODA_LOG_PATH` and `$OODA_EXIT_PATH` at 0o600.
    // The `.log` is pre-created at 0o600 via `open_secure_truncate`
    // (mode preserved across `>` truncate), but the `.exit` is NOT
    // pre-created: the observe layer's completion invariant is
    // ".exit existence ⇒ subprocess terminated", so pre-creating it
    // would race against in-flight observe passes that would read
    // empty bytes and fail to parse. `umask 077` is the durable fix:
    // any file the shell creates lands at 0o600 without changing
    // when it is created.
    cmd.arg("-c")
        .arg(
            r#"umask 077; "$@" > "$OODA_LOG_PATH" 2>&1; code=$?; printf '%s\n' "$code" > "$OODA_EXIT_PATH"; exit "$code""#,
        )
        .arg("ooda-codex-review-child")
        .arg(codex_bin)
        .args(codex_args)
        .env("OODA_LOG_PATH", log_path)
        .env("OODA_EXIT_PATH", exit_path);
    cmd
}

fn should_preflight_path(path: &std::path::Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BranchName, GitCommitSha};
    use ooda_core::MidTier;
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<&OsStr> {
        cmd.get_args().collect()
    }

    #[test]
    fn build_command_uses_codex_bin_path() {
        let cmd = build_codex_command(
            std::path::Path::new("/fake/codex"),
            CodexReasoningLevel::Low,
            &ReviewTarget::Uncommitted,
        )
        .unwrap();
        assert_eq!(cmd.get_program(), OsStr::new("/fake/codex"));
    }

    #[test]
    fn build_command_uncommitted() {
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            CodexReasoningLevel::Low,
            &ReviewTarget::Uncommitted,
        )
        .unwrap();
        let expected: Vec<&OsStr> = vec![
            OsStr::new("review"),
            OsStr::new("--uncommitted"),
            OsStr::new("-c"),
            OsStr::new("model_reasoning_effort=\"low\""),
        ];
        assert_eq!(args_of(&cmd), expected);
    }

    #[test]
    fn build_command_base_branch() {
        let branch = BranchName::parse("master").unwrap();
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            CodexReasoningLevel::High,
            &ReviewTarget::Base(branch),
        )
        .unwrap();
        let expected: Vec<&OsStr> = vec![
            OsStr::new("review"),
            OsStr::new("--base"),
            OsStr::new("master"),
            OsStr::new("-c"),
            OsStr::new("model_reasoning_effort=\"high\""),
        ];
        assert_eq!(args_of(&cmd), expected);
    }

    #[test]
    fn build_command_commit_sha() {
        let sha = GitCommitSha::parse(&"a".repeat(40)).unwrap();
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            CodexReasoningLevel::Medium,
            &ReviewTarget::Commit(sha),
        )
        .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "review");
        assert_eq!(args[1], "--commit");
        assert_eq!(args[2], "a".repeat(40));
        assert_eq!(args[3], "-c");
        assert_eq!(args[4], "model_reasoning_effort=\"medium\"");
    }

    #[test]
    fn build_command_rejects_unresolved_pr_target() {
        let err = build_codex_command(
            std::path::Path::new("codex"),
            CodexReasoningLevel::Xhigh,
            &ReviewTarget::Pr(42),
        )
        .unwrap_err();
        assert!(matches!(err, ActError::UnsupportedTarget(_)));
    }

    #[test]
    fn act_unsupported_for_agent() {
        let action = Action {
            kind: ActionKind::Retrospective {
                level: CodexReasoningLevel::Low,
            },
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("n/a"),
            },
            target_effect: crate::decide::action::TargetEffect::Advances,
            urgency: crate::decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: crate::ids::BlockerKey::from_static("retro:low"),
        };
        let ctx = ActContext {
            batch_dir: PathBuf::from("/tmp/nope"),
            target: ReviewTarget::Uncommitted,
            repo_root: PathBuf::from("/tmp/nope"),
            codex_bin: PathBuf::from("codex"),
            spawned_pgids: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(matches!(
            act(&action, &ctx),
            Err(ActError::UnsupportedAutomation)
        ));
    }

    #[test]
    fn spawn_uses_fake_binary_writes_to_log() {
        // Spawn /bin/true as a stand-in for codex. Verifies the
        // log file is created and the working directory is honored.
        let dir = std::env::temp_dir().join(format!(
            "ooda-codex-review-spawn-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ctx = ActContext {
            batch_dir: dir.clone(),
            target: ReviewTarget::Uncommitted,
            repo_root: std::env::current_dir().unwrap(),
            codex_bin: PathBuf::from("/bin/true"),
            spawned_pgids: Arc::new(Mutex::new(Vec::new())),
        };
        let action = Action {
            kind: ActionKind::RunReviews {
                level: CodexReasoningLevel::Low,
                n: 2,
            },
            effect: ActionEffect::Full {
                log: "n/a".into(),
                upstream: ooda_core::UpstreamConsistency::Eventual(
                    ooda_core::PollingInterval::from_secs(60),
                ),
            },
            target_effect: crate::decide::action::TargetEffect::Advances,
            urgency: crate::decide::action::Urgency::Mid(MidTier::Critical),
            blocker: crate::ids::BlockerKey::from_static("runreviews:low"),
        };

        act(&action, &ctx).expect("spawn should succeed with /bin/true");

        // Wait briefly for the children to exit and the OS to flush
        // the empty log files. /bin/true exits ~immediately.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(dir.join("low-1.log").exists(), "log 1 not created");
        assert!(dir.join("low-2.log").exists(), "log 2 not created");
        assert!(dir.join("low-1.exit").exists(), "exit 1 not created");
        assert!(dir.join("low-2.exit").exists(), "exit 2 not created");

        // Spawned-PGID tracking: per-slot wrapper PGID landed in
        // the shared registry. This is the rendezvous reap_spawned
        // _children uses to kill the whole sh → node → codex
        // subtree at run_loop shutdown.
        let pgids = ctx.spawned_pgids.lock().unwrap();
        assert_eq!(pgids.len(), 2, "expected 2 spawned PGIDs, got {pgids:?}");
        assert!(
            pgids.iter().all(|&p| p > 0),
            "PGIDs must be positive PIDs, got {pgids:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reap_spawned_children_clears_pgid_registry() {
        // Reap walks the registry and clears it. Idempotent: a
        // second call (or a call on an empty context) is a no-op.
        // We don't actually spawn here — fake PGID entries verify
        // the registry mechanics without needing real children.
        // The kill paths target nonexistent PGIDs; killpg returns
        // ESRCH which is intentionally ignored.
        let ctx = ActContext {
            batch_dir: PathBuf::from("/tmp/nope"),
            target: ReviewTarget::Uncommitted,
            repo_root: PathBuf::from("/tmp/nope"),
            codex_bin: PathBuf::from("/bin/true"),
            spawned_pgids: Arc::new(Mutex::new(vec![999_999, 999_998])),
        };
        assert_eq!(ctx.spawned_pgids.lock().unwrap().len(), 2);
        // Use a tiny grace — the killpg ESRCH path doesn't block.
        reap_spawned_children(&ctx, std::time::Duration::from_millis(10));
        assert!(
            ctx.spawned_pgids.lock().unwrap().is_empty(),
            "reap must clear the registry",
        );
        // Idempotent.
        reap_spawned_children(&ctx, std::time::Duration::from_millis(10));
        assert!(ctx.spawned_pgids.lock().unwrap().is_empty());
    }
}
