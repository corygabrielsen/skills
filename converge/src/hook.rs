//! Optional coprocess for progress events.
//!
//! Spawned once via `--hook <cmd>`. Receives JSONL on stdin.
//! Fire-and-forget: converge does not wait for the hook to process
//! events. Ordered delivery is guaranteed by the stdin stream.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::halt::{HaltReport, HookEvent};
use crate::protocol::{Action, FitnessReport};

/// Wall-clock budget for the hook to drain stdin and exit after
/// converge closes its end of the pipe. A hook that exceeds this is
/// killed: an unbounded wait would block converge's own shutdown on
/// a misbehaving coprocess (e.g., one that swallows EOF or blocks on
/// an unrelated handle).
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

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
}

impl Hook {
    /// Spawn the hook command via `sh -c` so shell features work.
    ///
    /// The shell is placed in a fresh process group whose ID equals
    /// its PID. Every descendant the shell forks inherits the group,
    /// so [`kill_group`] reaches the whole subtree: killing only the
    /// shell would orphan its children, and an orphan that inherited
    /// converge's stderr keeps that pipe open for the rest of its
    /// lifetime.
    pub(crate) fn spawn(cmd: &str) -> std::io::Result<Self> {
        let child = Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        Ok(Self { child: Some(child) })
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

    /// Close stdin and wait for the child to exit, bounded by
    /// [`FINISH_TIMEOUT`]. A hook that fails to exit within the
    /// budget is killed; failures (kill, reap) are swallowed because
    /// converge is itself shutting down and has no recovery channel.
    ///
    /// Consumes the child handle so [`Drop`]'s belt-and-braces
    /// kill+reap becomes a no-op past this call.
    pub(crate) fn finish(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Drop stdin to signal EOF. A well-behaved hook drains its
        // input and exits cleanly within FINISH_TIMEOUT.
        drop(child.stdin.take());
        let deadline = Instant::now() + FINISH_TIMEOUT;
        loop {
            match child.try_wait() {
                // Child reaped (clean exit or signal). Done.
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // Budget exhausted: force termination so
                        // converge's own shutdown can proceed. Both
                        // kill and the post-kill wait are best-
                        // effort; the parent has no recovery path.
                        kill_group(&mut child);
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
    /// child so it cannot linger as a zombie. Skipped when
    /// `finish` already consumed the child handle.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_group(&mut child);
        }
    }
}

/// `SIGKILL` the child's process group, then reap the child. The
/// group ID equals the child's PID by construction in
/// [`Hook::spawn`]. Best-effort: `ESRCH` (group already gone) and
/// `EPERM` are ignored, as is the reap result.
fn kill_group(child: &mut Child) {
    if let Ok(pgid) = i32::try_from(child.id()) {
        // SAFETY: `killpg` takes plain integers and reports every
        // failure through its return value; no memory is touched.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_returns_promptly_for_clean_hook() {
        // A hook that drains stdin and exits immediately on EOF
        // should join well under the timeout — finish() must not
        // wait for the full budget on the happy path.
        let hook = Hook::spawn("cat >/dev/null").expect("spawn cat");
        let started = Instant::now();
        hook.finish();
        let elapsed = started.elapsed();
        assert!(
            elapsed < FINISH_TIMEOUT,
            "finish() blocked {elapsed:?}; expected sub-budget exit",
        );
    }

    /// `true` once no process remains in group `pgid`. Signal 0
    /// probes without delivering; `ESRCH` is the empty-group answer.
    fn group_is_empty(pgid: i32) -> bool {
        // SAFETY: signal 0 probes existence only; see `kill_group`.
        let rc = unsafe { libc::killpg(pgid, 0) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn pgid_of(hook: &Hook) -> i32 {
        let child = hook.child.as_ref().expect("child present before finish");
        i32::try_from(child.id()).expect("PID fits in i32")
    }

    #[test]
    fn finish_kills_hook_that_ignores_eof() {
        // A hook that never exits on its own must be killed at the
        // deadline; finish() must return within a small slack of the
        // configured budget. `sh -c "sleep 600"` forks `sleep` as a
        // grandchild, so the post-finish assertion proves the whole
        // group died, not just the shell.
        let hook = Hook::spawn("sleep 600").expect("spawn sleep");
        let pgid = pgid_of(&hook);
        let started = Instant::now();
        hook.finish();
        let elapsed = started.elapsed();
        assert!(
            group_is_empty(pgid),
            "process group {pgid} still has members after finish()"
        );
        assert!(
            elapsed >= FINISH_TIMEOUT,
            "finish() returned {elapsed:?} before deadline {FINISH_TIMEOUT:?}",
        );
        // Slack covers poll granularity + kill + reap.
        assert!(
            elapsed < FINISH_TIMEOUT + Duration::from_secs(2),
            "finish() overran budget by {:?}",
            elapsed.checked_sub(FINISH_TIMEOUT).unwrap_or_default(),
        );
    }

    #[test]
    fn drop_kills_whole_process_group() {
        let hook = Hook::spawn("sleep 600").expect("spawn sleep");
        let pgid = pgid_of(&hook);
        drop(hook);
        assert!(
            group_is_empty(pgid),
            "process group {pgid} still has members after drop"
        );
    }
}
