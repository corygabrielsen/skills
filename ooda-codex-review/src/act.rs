//! act — execute Full and Wait actions.
//!
//! Phase 5b wires the `RunReviews` arm: spawn `n` parallel
//! `codex review` subprocesses with stdout/stderr piped to
//! `<batch_dir>/<level>-<slot>.log`. Other Full kinds
//! (AdvanceLevel/DropLevel/RestartFromFloor/RunTests) return
//! `NotImplemented` until Phase 6b/8 wires the recorder. Wait
//! sleeps the configured interval. Agent/Human never reach act
//! under correct decide flow.
//!
//! `ActContext` supplies the per-invocation environment that
//! actions need (where to write logs, which target to review,
//! where to cd before spawn). The runner threads one through
//! per invocation.

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
    /// Optional review criteria — a free-form prompt passed to
    /// `codex review` as a positional argument after the mode
    /// flag. When `None`, codex uses its default criteria.
    pub criteria: Option<String>,
}

#[derive(Debug)]
pub enum ActError {
    UnsupportedAutomation,
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
/// after spawn — observe/await polls log files for completion.
///
/// Failures: if any spawn fails, the function returns the first
/// error. Already-spawned children are *not* killed — they'll
/// continue and their logs will land. The next observe call will
/// see partial completion (Running with `total < expected`).
fn spawn_codex_reviews(level: ReasoningLevel, n: u32, ctx: &ActContext) -> Result<(), ActError> {
    std::fs::create_dir_all(&ctx.batch_dir)
        .map_err(|source| ActError::Spawn { slot: 0, source })?;

    for slot in 1..=n {
        let log_path = ctx.batch_dir.join(format!("{}-{slot}.log", level.as_str()));
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|source| ActError::Spawn { slot, source })?;
        let log_err = log
            .try_clone()
            .map_err(|source| ActError::Spawn { slot, source })?;

        let mut cmd =
            build_codex_command(&ctx.codex_bin, level, &ctx.target, ctx.criteria.as_deref());
        cmd.current_dir(&ctx.repo_root)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .stdin(Stdio::null());
        cmd.spawn()
            .map_err(|source| ActError::Spawn { slot, source })?;
    }
    Ok(())
}

/// Build the `codex review` command for the given target and
/// reasoning level. Pure — no I/O. Public for unit tests.
///
/// `criteria` is an optional free-form prompt that codex review
/// accepts as a positional argument after the mode flag (e.g.
/// `codex review --uncommitted "check for SQL injection" -c ...`).
pub fn build_codex_command(
    codex_bin: &std::path::Path,
    level: ReasoningLevel,
    target: &ReviewTarget,
    criteria: Option<&str>,
) -> Command {
    let mut cmd = Command::new(codex_bin);
    cmd.arg("review");
    match target {
        ReviewTarget::Uncommitted => {
            cmd.arg("--uncommitted");
        }
        ReviewTarget::Base(branch) => {
            cmd.arg("--base").arg(branch.as_str());
        }
        ReviewTarget::Commit(sha) => {
            cmd.arg("--commit").arg(sha.as_str());
        }
        ReviewTarget::Pr(num) => {
            cmd.arg("--pr").arg(num.to_string());
        }
    }
    if let Some(c) = criteria {
        cmd.arg(c);
    }
    cmd.arg("-c")
        .arg(format!("model_reasoning_effort=\"{}\"", level.as_str()));
    cmd
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
            None,
        );
        assert_eq!(cmd.get_program(), OsStr::new("/fake/codex"));
    }

    #[test]
    fn build_command_uncommitted() {
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            ReasoningLevel::Low,
            &ReviewTarget::Uncommitted,
            None,
        );
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
            None,
        );
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
            None,
        );
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
    fn build_command_pr_number() {
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            ReasoningLevel::Xhigh,
            &ReviewTarget::Pr(42),
            None,
        );
        let expected: Vec<&OsStr> = vec![
            OsStr::new("review"),
            OsStr::new("--pr"),
            OsStr::new("42"),
            OsStr::new("-c"),
            OsStr::new("model_reasoning_effort=\"xhigh\""),
        ];
        assert_eq!(args_of(&cmd), expected);
    }

    #[test]
    fn build_command_with_criteria_inserts_after_mode() {
        // codex review --uncommitted "check for SQL injection" -c ...
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            ReasoningLevel::High,
            &ReviewTarget::Uncommitted,
            Some("check for SQL injection"),
        );
        let expected: Vec<&OsStr> = vec![
            OsStr::new("review"),
            OsStr::new("--uncommitted"),
            OsStr::new("check for SQL injection"),
            OsStr::new("-c"),
            OsStr::new("model_reasoning_effort=\"high\""),
        ];
        assert_eq!(args_of(&cmd), expected);
    }

    #[test]
    fn build_command_with_criteria_after_base_branch() {
        let branch = BranchName::parse("master").unwrap();
        let cmd = build_codex_command(
            std::path::Path::new("codex"),
            ReasoningLevel::Low,
            &ReviewTarget::Base(branch),
            Some("focus on auth"),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "review");
        assert_eq!(args[1], "--base");
        assert_eq!(args[2], "master");
        assert_eq!(args[3], "focus on auth");
        assert_eq!(args[4], "-c");
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
            criteria: None,
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
            criteria: None,
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

        let _ = std::fs::remove_dir_all(&dir);
    }
}
