//! Optional coprocess for progress events.
//!
//! Spawned once via `--hook <cmd>`. Receives JSONL on stdin.
//! Fire-and-forget: converge does not wait for the hook to process
//! events. Ordered delivery is guaranteed by the stdin stream.

use std::io::Write;
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
    child: Child,
}

impl Hook {
    /// Spawn the hook command via `sh -c` so shell features work.
    pub(crate) fn spawn(cmd: &str) -> std::io::Result<Self> {
        let child = Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self { child })
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
        if let Some(stdin) = self.child.stdin.as_mut()
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
    pub(crate) fn finish(mut self) {
        // Drop stdin to signal EOF. A well-behaved hook drains its
        // input and exits cleanly within FINISH_TIMEOUT.
        drop(self.child.stdin.take());
        let deadline = Instant::now() + FINISH_TIMEOUT;
        loop {
            match self.child.try_wait() {
                // Child reaped (clean exit or signal). Done.
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // Budget exhausted: force termination so
                        // converge's own shutdown can proceed. Both
                        // kill and the post-kill wait are best-
                        // effort; the parent has no recovery path.
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return;
                    }
                    std::thread::sleep(FINISH_POLL);
                }
            }
        }
    }
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

    #[test]
    fn finish_kills_hook_that_ignores_eof() {
        // A hook that never exits on its own must be killed at the
        // deadline; finish() must return within a small slack of the
        // configured budget.
        let hook = Hook::spawn("sleep 600").expect("spawn sleep");
        let started = Instant::now();
        hook.finish();
        let elapsed = started.elapsed();
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
}
