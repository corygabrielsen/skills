//! Subprocess spawn with per-call deadline.
//!
//! Wraps `Command::output()` such that the parent never blocks
//! indefinitely on a stuck child. On deadline elapsed, the child
//! is `SIGKILL`ed via [`Child::kill`] and reaped, and the call
//! returns [`SpawnError::Timeout`].
//!
//! # Why this exists
//!
//! Bare `Command::output()` reads the child's stdout to EOF, which
//! is unbounded — a wedged `gh`, `gt`, or `git` blocks the calling
//! thread forever. Under the observe-stage fan-out, one stuck child
//! wedges the whole pass. Every OODA subprocess invocation routes
//! through this helper so a deadline is structural rather than
//! per-callsite discipline.
//!
//! # Per-call deadline
//!
//! The helper takes a deadline parameter; it does NOT hard-code
//! one. Each callsite picks a value appropriate to what it spawns
//! (network-bound `gh` tolerates more than a local `git rev-parse`).
//! The justified policy lives at the callsite as a named constant.
//!
//! # Kill discipline
//!
//! On timeout: `Child::kill()` (SIGKILL) followed by `Child::wait()`
//! to reap the zombie. SIGTERM-then-SIGKILL is not used — the
//! children this crate spawns (`gh`, `gt`, `git`, `codex`) have no
//! cleanup invariant that depends on graceful shutdown, and stdlib
//! does not expose SIGTERM without pulling in `libc`. The pragmatic
//! choice keeps the helper dependency-free.
//!
//! # stdout / stderr drain
//!
//! Both pipes are drained on background threads so the kernel pipe
//! buffer (typically 64 KiB) never fills and stalls the child. The
//! drain threads exit when the child's pipe ends close (on normal
//! exit or after `kill()` reaps the fds).

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Failure modes of [`run_with_deadline`].
#[derive(Debug)]
pub enum SpawnError {
    /// The subprocess could not be spawned. Typically: binary not
    /// on `$PATH`, or fd exhaustion.
    Spawn(std::io::Error),
    /// `try_wait` or `wait` on the child failed (rare; usually
    /// `ECHILD` from a double-reap, which the helper avoids by
    /// owning the child).
    Wait(std::io::Error),
    /// The child did not exit within the deadline. The helper
    /// attempted `Child::kill()` and reaped the zombie via
    /// `Child::wait()`; `killed` records whether the kill syscall
    /// itself succeeded (it can fail if the child raced to exit
    /// between the deadline check and the kill).
    Timeout { deadline: Duration, killed: bool },
    /// Reading the child's stdout or stderr pipe failed.
    Read(std::io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "spawn subprocess: {e}"),
            Self::Wait(e) => write!(f, "wait for subprocess: {e}"),
            Self::Timeout { deadline, killed } => {
                let suffix = if *killed { "killed" } else { "kill failed" };
                write!(
                    f,
                    "subprocess timed out after {}s ({suffix})",
                    deadline.as_secs()
                )
            }
            Self::Read(e) => write!(f, "read subprocess pipe: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) | Self::Wait(e) | Self::Read(e) => Some(e),
            Self::Timeout { .. } => None,
        }
    }
}

/// Polling interval for `try_wait`. Tight enough that a sub-second
/// child still completes within one tick of its actual exit; loose
/// enough that a multi-second deadline doesn't pin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn `cmd` and wait at most `deadline` for completion.
///
/// On success, returns the `Output` (status + stdout + stderr)
/// identical to what [`Command::output`] would return. On
/// deadline-elapsed, sends `SIGKILL` to the child, reaps it, and
/// returns [`SpawnError::Timeout`]; any partial output is dropped.
///
/// stdout and stderr are forced to `Stdio::piped`; stdin is forced
/// to `Stdio::null`. Any caller-set values for those three fields
/// are overridden — the helper owns the pipe topology.
///
/// # Errors
///
/// - [`SpawnError::Spawn`] if the OS could not fork/exec the
///   subprocess.
/// - [`SpawnError::Wait`] if `try_wait` or `wait` reported an
///   error other than the expected child-exit signal.
/// - [`SpawnError::Timeout`] if the child did not exit within
///   `deadline`.
/// - [`SpawnError::Read`] if draining either pipe failed.
///
/// # Panics
///
/// Panics only if the stdlib violates its own `Stdio::piped`
/// post-condition that `Child::stdout` / `Child::stderr` are
/// `Some` after `spawn`. The helper sets both fields immediately
/// above the spawn call, so the only paths that reach the
/// `expect`s are stdlib bugs.
pub fn run_with_deadline(cmd: &mut Command, deadline: Duration) -> Result<Output, SpawnError> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(SpawnError::Spawn)?;
    // `take()` removes the pipe ends from `Child` so the drain
    // threads own them; otherwise `try_wait` would see the pipes
    // still open even after the child exited and the EOF that
    // signals "done" to the drain loop would never arrive.
    let stdout = child
        .stdout
        .take()
        .expect("stdout was set to Stdio::piped above");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was set to Stdio::piped above");

    let (stdout_tx, stdout_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let result = drain_pipe(stdout, &mut buf).map(|()| buf);
        // Receiver may have already gone away on the timeout path;
        // ignore the send error — the thread's job is just to
        // drain the kernel buffer so the child can exit.
        let _ = stdout_tx.send(result);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        let result = drain_pipe(stderr, &mut buf).map(|()| buf);
        let _ = stderr_tx.send(result);
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(SpawnError::Wait)? {
            break status;
        }
        if start.elapsed() >= deadline {
            // SIGKILL the child; reap regardless so the OS
            // does not leak a zombie. The drain threads see
            // the pipe close and exit on their own.
            let killed = child.kill().is_ok();
            let _ = child.wait();
            return Err(SpawnError::Timeout { deadline, killed });
        }
        thread::sleep(POLL_INTERVAL);
    };

    // On the success path: both drain threads must have already
    // sent (or be about to send). `recv()` blocks at most as long
    // as the OS takes to flush the closed pipe.
    let stdout = stdout_rx
        .recv()
        .expect("stdout drain thread always sends before exiting")
        .map_err(SpawnError::Read)?;
    let stderr = stderr_rx
        .recv()
        .expect("stderr drain thread always sends before exiting")
        .map_err(SpawnError::Read)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn drain_pipe<R: Read>(mut r: R, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_returns_output_unchanged() {
        // /bin/true on every Unix; the helper must pass through
        // its zero exit and empty output without ceremony.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("printf hello && printf err 1>&2");
        let out = run_with_deadline(&mut cmd, Duration::from_secs(10)).expect("fast cmd succeeds");
        assert!(out.status.success(), "exit status: {:?}", out.status);
        assert_eq!(out.stdout, b"hello");
        assert_eq!(out.stderr, b"err");
    }

    #[test]
    fn slow_command_times_out_and_child_is_reaped() {
        // `sleep 5` with a 200ms deadline must return Timeout.
        // The kill+wait path keeps the child from leaking as a
        // zombie; we cannot assert process-table state from
        // within the runtime, but if reaping were broken the
        // test runner under heavy parallelism would surface
        // zombies elsewhere.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 5");
        let deadline = Duration::from_millis(200);
        let started = Instant::now();
        let err = run_with_deadline(&mut cmd, deadline).expect_err("must time out");
        let elapsed = started.elapsed();
        match err {
            SpawnError::Timeout {
                deadline: d,
                killed,
            } => {
                assert_eq!(d, deadline);
                assert!(killed, "kill syscall must succeed for a sleeping shell");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        // The helper must return shortly after the deadline,
        // not after the child's natural 5s sleep.
        assert!(
            elapsed < Duration::from_secs(2),
            "helper hung past deadline: elapsed={elapsed:?}",
        );
    }

    #[test]
    fn spawn_error_when_binary_missing() {
        let mut cmd = Command::new("/no/such/binary/ever/exists-for-tests");
        let err =
            run_with_deadline(&mut cmd, Duration::from_secs(1)).expect_err("missing binary errors");
        assert!(matches!(err, SpawnError::Spawn(_)), "got {err:?}");
    }

    #[test]
    fn nonzero_exit_propagates_via_output_status() {
        // `false` exits 1; helper returns Ok(Output) with that
        // exit code — non-zero is data, not error.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 7");
        let out = run_with_deadline(&mut cmd, Duration::from_secs(5)).expect("nonzero is data");
        assert_eq!(out.status.code(), Some(7));
        assert!(!out.status.success());
    }

    #[test]
    fn large_stdout_does_not_deadlock_on_pipe_buffer() {
        // Without a drain thread, writing more than the pipe
        // buffer (typically 64 KiB) would stall the child
        // forever. 256 KiB exercises that regression.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("head -c 262144 /dev/zero || dd if=/dev/zero bs=1024 count=256 2>/dev/null");
        let out =
            run_with_deadline(&mut cmd, Duration::from_secs(10)).expect("256 KiB drain succeeds");
        assert_eq!(out.stdout.len(), 262_144);
    }

    #[test]
    fn timeout_error_display_includes_deadline_seconds() {
        let err = SpawnError::Timeout {
            deadline: Duration::from_secs(42),
            killed: true,
        };
        let s = err.to_string();
        assert!(s.contains("42"), "display should include deadline: {s}");
        assert!(
            s.contains("timed out"),
            "display should mention timeout: {s}"
        );
    }
}
