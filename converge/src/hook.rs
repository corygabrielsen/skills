//! Optional coprocess for progress events.
//!
//! Spawned once via `--hook <cmd>`. Receives JSONL on stdin.
//! Fire-and-forget: converge does not wait for the hook to process
//! events. Ordered delivery is guaranteed by the stdin stream.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ooda_core::spawn::kill_process_group;

use crate::halt::{HaltReport, HookEvent};
use crate::protocol::{Action, FitnessReport};

/// Poll interval while waiting for the hook to exit. Short enough
/// that termination latency is bounded near the kill granularity;
/// long enough that the parent CPU cost on a quick-exiting hook is
/// negligible.
const FINISH_POLL: Duration = Duration::from_millis(50);

pub(crate) struct Hook {
    /// `None` once [`Self::finish`] has consumed the child;
    /// [`Drop`] becomes a no-op past that point. Belt-and-braces
    /// against cancellation paths that drop the [`Hook`] without
    /// calling [`Self::finish`] — the process group is killed and
    /// the child reaped rather than left as a zombie.
    child: Option<Child>,
    /// Wall-clock budget for the hook to drain stdin and exit after
    /// converge closes its end of the pipe. A hook that exceeds it
    /// is killed: an unbounded wait would block converge's own
    /// shutdown on a misbehaving coprocess (e.g., one that swallows
    /// EOF or blocks on an unrelated handle).
    finish_timeout: Duration,
}

impl Hook {
    /// Spawn the hook command via `sh -c` so shell features work.
    ///
    /// The shell is placed in a fresh process group whose ID equals
    /// its PID. Every descendant the shell forks inherits the group,
    /// so [`kill_process_group`] reaches the whole subtree: killing
    /// only the shell would orphan its children, and an orphan that
    /// inherited converge's stderr keeps that pipe open for the rest
    /// of its lifetime.
    pub(crate) fn spawn(cmd: &str, finish_timeout: Duration) -> std::io::Result<Self> {
        let child = Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        Ok(Self {
            child: Some(child),
            finish_timeout,
        })
    }

    /// Send an iteration event. Non-blocking, best-effort.
    pub(crate) fn send_iteration(&mut self, iter: u32, report: &FitnessReport, action: &Action) {
        let event = HookEvent::Iteration {
            iter,
            report,
            action,
        };
        self.send(&event);
    }

    /// Send a halt event. Non-blocking, best-effort.
    pub(crate) fn send_halt(&mut self, halt: &HaltReport, last_report: Option<&FitnessReport>) {
        let event = HookEvent::Halt { halt, last_report };
        self.send(&event);
    }

    fn send(&mut self, event: &HookEvent) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Some(stdin) = child.stdin.as_mut()
            && let Ok(line) = serde_json::to_string(event)
        {
            let _ = writeln!(stdin, "{line}");
            let _ = stdin.flush();
        }
    }

    /// Close stdin and wait for the child to exit, bounded by the
    /// finish timeout. A hook that fails to exit within the budget
    /// has its whole process group killed; failures (kill, reap)
    /// are swallowed because converge is itself shutting down and
    /// has no recovery channel.
    ///
    /// Consumes the child handle so [`Drop`]'s belt-and-braces
    /// kill+reap becomes a no-op past this call.
    pub(crate) fn finish(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Drop stdin to signal EOF. A well-behaved hook drains its
        // input and exits cleanly within the budget.
        drop(child.stdin.take());
        let deadline = Instant::now() + self.finish_timeout;
        loop {
            match child.try_wait() {
                // Child reaped (clean exit or signal). Done.
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        kill_process_group(&mut child);
                        return;
                    }
                    std::thread::sleep(FINISH_POLL);
                }
            }
        }
    }
}

impl Drop for Hook {
    /// Belt-and-braces against cancellation paths that drop the
    /// [`Hook`] without calling [`Self::finish`]: kill+reap the
    /// whole process group so nothing lingers holding converge's
    /// stderr. Skipped when `finish` already consumed the child
    /// handle.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_process_group(&mut child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_millis(100);

    /// `true` if any live process's argv equals `args` exactly.
    /// Reads `/proc/<pid>/cmdline`; unreadable entries are skipped.
    /// A zombie's cmdline is empty, so a dead-but-unreaped
    /// grandchild never matches.
    fn process_with_args_exists(args: &[&str]) -> bool {
        let want: Vec<u8> = args
            .iter()
            .flat_map(|a| a.bytes().chain(std::iter::once(0)))
            .collect();
        std::fs::read_dir("/proc")
            .expect("/proc readable")
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .any(|cmdline| cmdline == want)
    }

    /// Spawn a hook whose shell forks `sleep <marker>` and block
    /// until that grandchild is visible, so the kill under test is
    /// exercised against a two-level tree rather than racing the
    /// shell's fork.
    fn spawn_with_grandchild(marker: &str) -> Hook {
        let hook = Hook::spawn(&format!("sleep {marker}"), TIMEOUT).expect("spawn sleep");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_with_args_exists(&["sleep", marker]) {
            assert!(Instant::now() < deadline, "grandchild never appeared");
            std::thread::sleep(Duration::from_millis(5));
        }
        hook
    }

    #[test]
    fn finish_returns_promptly_for_clean_hook() {
        // A hook that drains stdin and exits on EOF joins well
        // under the budget: finish() must not wait it out on the
        // happy path.
        let hook = Hook::spawn("cat >/dev/null", Duration::from_secs(5)).expect("spawn cat");
        let started = Instant::now();
        hook.finish();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "finish() blocked {elapsed:?}; expected sub-budget exit",
        );
    }

    #[test]
    fn finish_kills_hook_that_ignores_eof() {
        const MARKER: &str = "600.271828";
        let hook = spawn_with_grandchild(MARKER);
        let started = Instant::now();
        hook.finish();
        let elapsed = started.elapsed();
        assert!(
            !process_with_args_exists(&["sleep", MARKER]),
            "grandchild `sleep {MARKER}` survived finish()",
        );
        assert!(
            elapsed >= TIMEOUT,
            "finish() returned {elapsed:?} before deadline {TIMEOUT:?}",
        );
        // Slack covers poll granularity + kill + group exit.
        assert!(
            elapsed < TIMEOUT + Duration::from_secs(2),
            "finish() overran budget by {:?}",
            elapsed.checked_sub(TIMEOUT).unwrap_or_default(),
        );
    }

    #[test]
    fn drop_kills_whole_process_group() {
        const MARKER: &str = "600.141592";
        let hook = spawn_with_grandchild(MARKER);
        drop(hook);
        assert!(
            !process_with_args_exists(&["sleep", MARKER]),
            "grandchild `sleep {MARKER}` survived drop",
        );
    }
}
