//! act — execute Full and Wait actions.
//!
//! Phase 5b wires the `RunReviews` arm: spawn `n` parallel
//! `codex review` subprocesses with stdout/stderr redirected to
//! `<batch_dir>/<level>-<slot>.log` and exit status written to
//! `<batch_dir>/<level>-<slot>.exit`. Other Full kinds
//! (AdvanceLevel/DropLevel/RestartFromFloor/RunTests) return
//! `NotImplemented` until Phase 6b/8 wires the recorder. Wait
//! sleeps the configured interval. Agent/Human never reach act
//! under correct decide flow.
//!
//! `ActContext` supplies the per-invocation environment that
//! actions need (where to write logs, which target to review,
//! where to cd before spawn). The runner threads one through
//! per invocation.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::decide::action::{Action, ActionKind, Automation, ReasoningLevel};
use crate::ids::ReviewTarget;

/// Per-invocation environment for `act`. Stable across all
/// iterations of a single `run_loop` call.
#[derive(Debug, Clone)]
pub struct ActContext {
    /// Directory where per-slot log files (`<level>-<slot>.log`)
    /// are written.
    pub batch_dir: PathBuf,
    /// What slice of changes `codex review` is invoked against.
    pub target: ReviewTarget,
    /// Working directory for the spawned `codex` subprocesses.
    /// Usually the repo root resolved from `git rev-parse
    /// --show-toplevel`.
    pub repo_root: PathBuf,
    /// Path to the `codex` binary. Defaults to `"codex"` (PATH
    /// lookup); tests inject a fake binary path.
    pub codex_bin: PathBuf,
}

#[derive(Debug)]
pub enum ActError {
    UnsupportedAutomation,
    /// A target reached act that the current `codex review` CLI
    /// cannot execute directly. Callers should resolve user-facing
    /// sugar like `--pr` before constructing `ActContext`.
    UnsupportedTarget(String),
    NotImplemented,
    /// Spawning a `codex review` subprocess failed (binary not
    /// found, log file open failed, etc.). Carries which slot
    /// failed and the underlying io error.
    Spawn {
        slot: u32,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ActError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAutomation => write!(
                f,
                "act received an Agent/Human action — decide must halt those"
            ),
            Self::UnsupportedTarget(msg) => write!(f, "unsupported codex review target: {msg}"),
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

/// Dispatch one action against `ctx`. Wait sleeps; Full kinds
/// dispatch to handler functions; Agent/Human are an invariant
/// violation (decide should have halted instead of executing).
pub fn act(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match action.automation {
        Automation::Wait { interval } => {
            std::thread::sleep(interval);
            Ok(())
        }
        Automation::Full => dispatch_full(action, ctx),
        Automation::Agent | Automation::Human => Err(ActError::UnsupportedAutomation),
    }
}

fn dispatch_full(action: &Action, ctx: &ActContext) -> Result<(), ActError> {
    match &action.kind {
        ActionKind::RunReviews { level, n } => spawn_codex_reviews(*level, *n, ctx),
        // Phase 6b/8: AdvanceLevel/DropLevel/RestartFromFloor/RunTests
        // wire to the recorder. ParseVerdicts is implicit in observe
        // (scan_batch returns Complete with parsed verdicts).
        _ => Err(ActError::NotImplemented),
    }
}

/// Spawn `n` `codex review` subprocesses. Returns immediately
/// after spawn — observe/await polls log and exit files for
/// completion.
///
/// Failures: if any spawn fails, the function returns the first
/// error. Already-spawned children are *not* killed — they'll
/// continue and their logs will land. The next observe call will
/// see partial completion (Running with `total < expected`).
fn spawn_codex_reviews(level: ReasoningLevel, n: u32, ctx: &ActContext) -> Result<(), ActError> {
    std::fs::create_dir_all(&ctx.batch_dir)
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
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
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
        cmd.spawn()
            .map_err(|source| ActError::Spawn { slot, source })?;
    }
    Ok(())
}

/// Build the `codex review` argv for the given target and reasoning
/// level. Pure — no I/O. Public for unit tests.
pub fn build_codex_args(
    level: ReasoningLevel,
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

/// Build the direct `codex review` command for unit tests and
/// diagnostics. Runtime spawning uses [`build_logged_codex_command`]
/// so observe can see child exit status after this process returns.
pub fn build_codex_command(
    codex_bin: &std::path::Path,
    level: ReasoningLevel,
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
    cmd.arg("-c")
        .arg(
            r#""$@" > "$OODA_LOG_PATH" 2>&1; code=$?; printf '%s\n' "$code" > "$OODA_EXIT_PATH"; exit "$code""#,
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
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<&OsStr> {
        cmd.get_args().collect()
    }

    #[test]
    fn build_command_uses_codex_bin_path() {
        let cmd = build_codex_command(
            std::path::Path::new("/fake/codex"),
            ReasoningLevel::Low,
            &ReviewTarget::Uncommitted,
        )
        .unwrap();
        assert_eq!(cmd.get_program(), OsStr::new("/fake/codex"));
    }

    #[test]
    fn build_command_uncommitted() {
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            ReasoningLevel::Low,
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
            ReasoningLevel::High,
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
            ReasoningLevel::Medium,
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
            ReasoningLevel::Xhigh,
            &ReviewTarget::Pr(42),
        )
        .unwrap_err();
        assert!(matches!(err, ActError::UnsupportedTarget(_)));
    }

    #[test]
    fn act_unsupported_for_agent() {
        let action = Action {
            kind: ActionKind::Retrospective {
                level: ReasoningLevel::Low,
            },
            automation: Automation::Agent,
            target_effect: crate::decide::action::TargetEffect::Advances,
            urgency: crate::decide::action::Urgency::BlockingFix,
            description: "n/a".into(),
            blocker: crate::ids::BlockerKey::tag("retro:low"),
        };
        let ctx = ActContext {
            batch_dir: PathBuf::from("/tmp/nope"),
            target: ReviewTarget::Uncommitted,
            repo_root: PathBuf::from("/tmp/nope"),
            codex_bin: PathBuf::from("codex"),
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
        };
        let action = Action {
            kind: ActionKind::RunReviews {
                level: ReasoningLevel::Low,
                n: 2,
            },
            automation: Automation::Full,
            target_effect: crate::decide::action::TargetEffect::Advances,
            urgency: crate::decide::action::Urgency::Critical,
            description: "n/a".into(),
            blocker: crate::ids::BlockerKey::tag("runreviews:low"),
        };

        act(&action, &ctx).expect("spawn should succeed with /bin/true");

        // Wait briefly for the children to exit and the OS to flush
        // the empty log files. /bin/true exits ~immediately.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(dir.join("low-1.log").exists(), "log 1 not created");
        assert!(dir.join("low-2.log").exists(), "log 2 not created");
        assert!(dir.join("low-1.exit").exists(), "exit 1 not created");
        assert!(dir.join("low-2.exit").exists(), "exit 2 not created");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
