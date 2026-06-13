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
use std::time::Duration;

use crate::decide::action::{Action, ActionEffect, ActionKind};
use crate::ids::{CodexReasoningLevel, PullRequestNumber, RepoSlug};
use crate::observe::codex::batch_dir as codex_batch_dir;
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::state::WIP_LABEL;
use ooda_core::{SpawnError, SpawnLimits, run_with_limits};

/// `gt sync` rebases the local stack onto the latest base and may
/// fetch refs from the remote, so 2m gives room for a multi-PR
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
    /// `gt sync` subprocess failed. Surfaced with stderr so the
    /// agent path that owns triage sees the underlying reason.
    GraphiteSync(String),
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

/// Codex-axis attachment for [`ActContext`].
///
/// Static-per-invocation fields name the side-effect surface (binary,
/// batch tree root). `head_sha` and `base_branch` refresh each
/// iteration from observe so the side-effects continue to anchor on
/// the PR's current head and base — without that the batch tree and
/// the spawned argv would silently desynchronise.
///
/// `_lock` is an advisory file lock held FD-tied for the invocation's
/// lifetime; concurrent drivers against the same PR would otherwise
/// race on batch directory writes. The lock releases on process exit
/// by any path (including SIGKILL), so a crashed process never leaves
/// a stale lock that blocks subsequent invocations.
///
/// The codex spawn's working directory is taken from
/// [`ActContext::repo_root`] (the same field that pins `gt sync`);
/// there is no per-codex `repo_root` field — one resolved working
/// tree per invocation.
#[derive(Debug)]
pub(crate) struct CodexActContext {
    pub codex_bin: PathBuf,
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
    /// FD-tied advisory lock. Released on FD close (held via the
    /// `ooda_core::FileLock` RAII guard so the sidecar inherits
    /// 0o600 mode and the kernel-advisory release-on-fd-close
    /// contract).
    pub _lock: ooda_core::FileLock,
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
///
/// `repo_root` is the resolved working tree the driver targets; it
/// pins every `gt` subprocess (sync) and the codex subprocess to the
/// same path. See [`crate::resolve_repo_root`] for the resolution
/// policy.
#[derive(Debug)]
pub(crate) struct ActContext {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub action_lock_path: PathBuf,
    pub repo_root: PathBuf,
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
            spawn_codex_review_batch(codex, &ctx.repo_root, *level, *n)?;
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
/// stack — see the [`ActContext`] docs on the threading rationale.
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

fn spawn_codex_review_batch(
    codex: &CodexActContext,
    repo_root: &Path,
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

    // Per-iteration: spawn all n slots first; only stamp
    // `head_sha.txt` once every spawn has succeeded. The observe
    // side's identity gate keys off `head_sha.txt`; if a partial
    // spawn leaves k<n logs and a matching head_sha, the next
    // observe sees `Running { total = k, completed = ... }` and
    // never re-spawns (head matches, no missing axis trigger),
    // deadlocking the loop until the next push.
    //
    // Writing the SHA last preserves the invariant: head_sha
    // present ⇒ every slot's spawn syscall returned Ok. A
    // partial-spawn failure leaves head_sha absent, the observe
    // gate projects `NotStarted`, and the next decide re-emits
    // `RunReviews` to retry from clean. The truncate-on-entry of
    // per-slot log files in the loop below makes the retry
    // idempotent against the residue of the failed attempt.
    let head_sha_path = dir.join("head_sha.txt");

    for slot in 1..=n {
        let log_path = dir.join(format!("{}-{slot}.log", level.as_str()));
        let exit_path = dir.join(format!("{}-{slot}.exit", level.as_str()));
        ooda_core::atomic_io::open_secure_truncate(&log_path).map_err(|source| {
            cleanup_partial_batch(&dir, level, n);
            ActError::CodexSpawn { slot, source }
        })?;
        if let Err(source) = std::fs::remove_file(&exit_path)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            cleanup_partial_batch(&dir, level, n);
            return Err(ActError::CodexSpawn { slot, source });
        }

        let mut cmd =
            build_logged_codex_command(&codex.codex_bin, &codex_args, &log_path, &exit_path);
        cmd.current_dir(repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        let mut child = cmd.spawn().map_err(|source| {
            cleanup_partial_batch(&dir, level, n);
            ActError::CodexSpawn { slot, source }
        })?;
        // Detached reaper thread per child. The observe layer reads
        // `.exit` for completion signal; this thread's only job is
        // to call `waitpid` so the OS reclaims the zombie when the
        // child exits. Dropping `Child` without `wait()` leaves a
        // zombie in the process table until the parent exits.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }

    // Every slot spawned successfully — stamp the SHA last so the
    // observe-side gate unblocks the batch.
    let mut sha_file = ooda_core::atomic_io::open_secure_truncate(&head_sha_path)
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    sha_file
        .write_all(codex.head_sha.as_bytes())
        .map_err(|source| ActError::CodexSpawn { slot: 0, source })?;
    Ok(())
}

/// Delete any per-slot log/exit files that a partially-failed
/// spawn pass may have left behind. Belt-and-braces with the
/// "write `head_sha.txt` last" invariant: on retry, the observe
/// gate already projects `NotStarted` (`head_sha` absent), so the
/// loop body's truncate-on-entry would clobber stale logs
/// anyway; cleaning up preemptively keeps `ls <batch_dir>` from
/// surfacing zero-byte logs as artifacts of the failed attempt.
///
/// Best-effort: a `NotFound` is the steady state of a slot that
/// never opened its log. Other errors are swallowed — the next
/// successful spawn pass will overwrite whatever survives, and
/// surfacing a secondary cleanup error would shadow the primary
/// `CodexSpawn` cause.
fn cleanup_partial_batch(dir: &Path, level: CodexReasoningLevel, n: u32) {
    for slot in 1..=n {
        let log_path = dir.join(format!("{}-{slot}.log", level.as_str()));
        let exit_path = dir.join(format!("{}-{slot}.exit", level.as_str()));
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&exit_path);
    }
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

    #[test]
    fn partial_spawn_failure_leaves_head_sha_absent_for_clean_retry() {
        // Regression: the previous shape wrote `head_sha.txt`
        // FIRST, before the per-slot spawn loop. A partial-spawn
        // failure (fd exhaustion, transient ENOENT, an existing
        // file/dir collision on `.exit`) left k<n logs plus a
        // matching `head_sha.txt`, and the next observe saw
        // `Running { total = k }` forever — the head SHA matched,
        // so no axis re-triggered `RunReviews` — until a fresh
        // push moved the SHA out from under the deadlock.
        //
        // The new shape stamps `head_sha.txt` LAST, after every
        // slot's spawn syscall returned Ok. A partial-spawn
        // failure leaves it absent; the observe gate projects
        // `NotStarted`; the next decide re-emits `RunReviews`.

        let test_root = std::env::temp_dir().join(format!(
            "ooda-pr-codex-review-act-site-a-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&test_root);
        std::fs::create_dir_all(&test_root).unwrap();

        let codex_pr_root = test_root.join("codex_root");
        let level = CodexReasoningLevel::Low;
        let head_sha = "abc123def456";
        let n: u32 = 3;

        // Pre-compute the batch dir and inject a slot-2 obstacle
        // before invoking. `low-2.exit` as a directory makes the
        // mid-loop `fs::remove_file` fail with `kind != NotFound`,
        // forcing the partial-spawn error path AFTER slot 1 has
        // already spawned successfully.
        let dir = crate::observe::codex::batch_dir(&codex_pr_root, level, head_sha);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join(format!("{}-2.exit", level.as_str()))).unwrap();

        // The codex_bin field is what's passed as argv[0] inside
        // `/bin/sh -c '"$@" ...'`. Using `/bin/true` keeps the
        // detached child cheap and lets slot 1 spawn successfully.
        let lock = ooda_core::FileLock::try_acquire(&test_root.join("act"))
            .unwrap()
            .expect("fresh lock acquires");
        let codex = CodexActContext {
            codex_bin: PathBuf::from("/bin/true"),
            codex_pr_root: codex_pr_root.clone(),
            n,
            head_sha: head_sha.to_string(),
            base_branch: "main".to_string(),
            _lock: lock,
        };

        let err = spawn_codex_review_batch(&codex, &test_root, level, n).unwrap_err();
        match err {
            ActError::CodexSpawn { slot, .. } => assert_eq!(slot, 2),
            other => panic!("expected CodexSpawn at slot 2, got {other:?}"),
        }

        assert!(
            !dir.join("head_sha.txt").exists(),
            "head_sha.txt must be absent on partial-spawn failure \
             so the observe gate projects NotStarted and the next \
             decide re-emits RunReviews from clean",
        );

        let _ = std::fs::remove_dir_all(&test_root);
    }
}
