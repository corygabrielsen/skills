//! Subprocess spawn with per-call deadline + per-stream byte cap.
//!
//! Wraps `Command::output()` such that the parent never blocks
//! indefinitely on a stuck child AND never grows unbounded buffers
//! from a child that emits gigabytes of output. On deadline elapsed,
//! the child is `SIGKILL`ed via [`Child::kill`] and reaped, and the
//! call returns [`SpawnError::Timeout`]. On either pipe's buffer
//! growing past its per-stream cap, the helper kills + reaps the
//! child and returns [`SpawnError::OutputTooLarge`].
//!
//! # Why this exists
//!
//! Bare `Command::output()` reads the child's stdout to EOF, which
//! is unbounded — a wedged `gh`, `gt`, or `git` blocks the calling
//! thread forever, and a misbehaving child that floods stdout/stderr
//! grows the parent's address space until the kernel OOM-kills it.
//! Under the observe-stage fan-out, either failure mode wedges the
//! whole pass. Every OODA subprocess invocation routes through this
//! helper so a deadline AND a byte cap are structural rather than
//! per-callsite discipline.
//!
//! # Per-call limits
//!
//! The helper takes a [`SpawnLimits`] parameter; it does NOT
//! hard-code one. Each callsite picks values appropriate to what it
//! spawns (network-bound `gh` tolerates more output than a local
//! `git rev-parse`; long-running `gt sync` tolerates more wall-time
//! than a `--version` probe). The justified policy lives at the
//! callsite as named constants.
//!
//! # Kill discipline
//!
//! On timeout or overflow: `Child::kill()` (SIGKILL) followed by
//! `Child::wait()` to reap the zombie. SIGTERM-then-SIGKILL is not
//! used — the children this crate spawns (`gh`, `gt`, `git`,
//! `codex`) have no cleanup invariant that depends on graceful
//! shutdown, and stdlib does not expose SIGTERM without pulling in
//! `libc`. The pragmatic choice keeps the helper dependency-free.
//!
//! # stdout / stderr drain
//!
//! Both pipes are drained on background threads so the kernel pipe
//! buffer (typically 64 KiB) never fills and stalls the child. Each
//! drain thread tracks its accumulated buffer length and signals
//! the main thread via an `AtomicBool` if it overflows its cap; the
//! main thread then kills the child and returns
//! [`SpawnError::OutputTooLarge`]. The drain threads exit when the
//! child's pipe ends close (on normal exit or after `kill()` reaps
//! the fds).

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Which child pipe overflowed its per-stream cap. Carried by
/// [`SpawnError::OutputTooLarge`] so callers can name the offending
/// stream in their diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// Short identifier for diagnostic strings.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl std::fmt::Display for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Per-call resource limits for [`run_with_limits`].
///
/// `deadline` caps wall-time before the helper kills the child.
/// `max_stdout_bytes` / `max_stderr_bytes` each cap the accumulated
/// buffer the drain thread builds for that stream; on overflow the
/// helper kills the child and returns [`SpawnError::OutputTooLarge`].
/// The two caps are independent — a child that emits 100 MiB on
/// stderr does not benefit from headroom on a tight stdout cap.
#[derive(Debug, Clone, Copy)]
pub struct SpawnLimits {
    pub deadline: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

/// Failure modes of [`run_with_limits`].
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
    /// One of the child pipes accumulated more bytes than its
    /// per-stream cap. The helper attempted `Child::kill()` and
    /// reaped the zombie; `killed` records whether the kill syscall
    /// itself succeeded.
    OutputTooLarge {
        stream: Stream,
        limit: usize,
        killed: bool,
    },
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
            Self::OutputTooLarge {
                stream,
                limit,
                killed,
            } => {
                let suffix = if *killed { "killed" } else { "kill failed" };
                write!(
                    f,
                    "subprocess {stream} exceeded {limit}-byte cap ({suffix})",
                )
            }
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) | Self::Wait(e) | Self::Read(e) => Some(e),
            Self::Timeout { .. } | Self::OutputTooLarge { .. } => None,
        }
    }
}

/// Polling interval for `try_wait`. Tight enough that a sub-second
/// child still completes within one tick of its actual exit; loose
/// enough that a multi-second deadline doesn't pin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn `cmd` and wait at most `limits.deadline` for completion,
/// capping each output stream at `limits.max_stdout_bytes` /
/// `limits.max_stderr_bytes`.
///
/// On success, returns the `Output` (status + stdout + stderr)
/// identical to what [`Command::output`] would return. On
/// deadline-elapsed, sends `SIGKILL` to the child, reaps it, and
/// returns [`SpawnError::Timeout`]; on cap overflow, same kill +
/// reap and returns [`SpawnError::OutputTooLarge`]. Any partial
/// output is dropped in both failure paths.
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
/// - [`SpawnError::Timeout`] if the child did not exit within the
///   deadline.
/// - [`SpawnError::Read`] if draining either pipe failed.
/// - [`SpawnError::OutputTooLarge`] if either pipe accumulated more
///   than its per-stream cap.
///
/// # Panics
///
/// Panics only if the stdlib violates its own `Stdio::piped`
/// post-condition that `Child::stdout` / `Child::stderr` are
/// `Some` after `spawn`. The helper sets both fields immediately
/// above the spawn call, so the only paths that reach the
/// `expect`s are stdlib bugs.
pub fn run_with_limits(cmd: &mut Command, limits: SpawnLimits) -> Result<Output, SpawnError> {
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

    let stdout_overflow = Arc::new(AtomicBool::new(false));
    let stderr_overflow = Arc::new(AtomicBool::new(false));

    let (stdout_tx, stdout_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    {
        let overflow = Arc::clone(&stdout_overflow);
        let limit = limits.max_stdout_bytes;
        thread::spawn(move || {
            let mut buf = Vec::new();
            let result = drain_pipe(stdout, &mut buf, limit, &overflow).map(|()| buf);
            // Receiver may have already gone away on the
            // timeout / overflow path; ignore the send error.
            let _ = stdout_tx.send(result);
        });
    }
    {
        let overflow = Arc::clone(&stderr_overflow);
        let limit = limits.max_stderr_bytes;
        thread::spawn(move || {
            let mut buf = Vec::new();
            let result = drain_pipe(stderr, &mut buf, limit, &overflow).map(|()| buf);
            let _ = stderr_tx.send(result);
        });
    }

    let start = Instant::now();
    let status = loop {
        // Check overflow flags BEFORE try_wait so an over-limit
        // child gets reported as OutputTooLarge even if it raced
        // to exit between drain-thread overflow and try_wait.
        if stdout_overflow.load(Ordering::Acquire) {
            let killed = child.kill().is_ok();
            let _ = child.wait();
            return Err(SpawnError::OutputTooLarge {
                stream: Stream::Stdout,
                limit: limits.max_stdout_bytes,
                killed,
            });
        }
        if stderr_overflow.load(Ordering::Acquire) {
            let killed = child.kill().is_ok();
            let _ = child.wait();
            return Err(SpawnError::OutputTooLarge {
                stream: Stream::Stderr,
                limit: limits.max_stderr_bytes,
                killed,
            });
        }
        if let Some(status) = child.try_wait().map_err(SpawnError::Wait)? {
            break status;
        }
        if start.elapsed() >= limits.deadline {
            // SIGKILL the child; reap regardless so the OS
            // does not leak a zombie. The drain threads see
            // the pipe close and exit on their own.
            let killed = child.kill().is_ok();
            let _ = child.wait();
            return Err(SpawnError::Timeout {
                deadline: limits.deadline,
                killed,
            });
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

    // A child can exit naturally between the last overflow check
    // and `try_wait` returning Some — re-check after recv'ing the
    // drain results so over-limit output is reported even on the
    // natural-exit path. `killed = false` because we did not need
    // to kill (the child exited on its own).
    if stdout_overflow.load(Ordering::Acquire) {
        return Err(SpawnError::OutputTooLarge {
            stream: Stream::Stdout,
            limit: limits.max_stdout_bytes,
            killed: false,
        });
    }
    if stderr_overflow.load(Ordering::Acquire) {
        return Err(SpawnError::OutputTooLarge {
            stream: Stream::Stderr,
            limit: limits.max_stderr_bytes,
            killed: false,
        });
    }

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn drain_pipe<R: Read>(
    mut r: R,
    buf: &mut Vec<u8>,
    limit: usize,
    overflow: &AtomicBool,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > limit {
                    // Signal the parent and return; the parent
                    // sees the flag and kills the child. Dropping
                    // `r` here also closes the pipe so the child
                    // gets SIGPIPE on its next write, accelerating
                    // its exit on the rare path where the parent's
                    // kill hasn't landed yet.
                    overflow.store(true, Ordering::Release);
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default test limits: 10s deadline, 1 MiB caps. Loose enough
    /// that ordinary `fast_command` cases don't trip them.
    fn default_limits() -> SpawnLimits {
        SpawnLimits {
            deadline: Duration::from_secs(10),
            max_stdout_bytes: 1 << 20,
            max_stderr_bytes: 1 << 20,
        }
    }

    #[test]
    fn fast_command_returns_output_unchanged() {
        // /bin/true on every Unix; the helper must pass through
        // its zero exit and empty output without ceremony.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("printf hello && printf err 1>&2");
        let out = run_with_limits(&mut cmd, default_limits()).expect("fast cmd succeeds");
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
        let limits = SpawnLimits {
            deadline: Duration::from_millis(200),
            max_stdout_bytes: 1 << 20,
            max_stderr_bytes: 1 << 20,
        };
        let started = Instant::now();
        let err = run_with_limits(&mut cmd, limits).expect_err("must time out");
        let elapsed = started.elapsed();
        match err {
            SpawnError::Timeout { deadline, killed } => {
                assert_eq!(deadline, limits.deadline);
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
        let err = run_with_limits(&mut cmd, default_limits()).expect_err("missing binary errors");
        assert!(matches!(err, SpawnError::Spawn(_)), "got {err:?}");
    }

    #[test]
    fn nonzero_exit_propagates_via_output_status() {
        // `false` exits 1; helper returns Ok(Output) with that
        // exit code — non-zero is data, not error.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 7");
        let out = run_with_limits(&mut cmd, default_limits()).expect("nonzero is data");
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
        let out = run_with_limits(&mut cmd, default_limits()).expect("256 KiB drain succeeds");
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

    #[test]
    fn stdout_cap_fires_on_overflow_and_child_is_reaped() {
        // A child that loudly floods stdout must be killed when
        // its buffer crosses the cap, not allowed to grow the
        // parent's address space without bound. `yes` would loop
        // forever; we cap at 1 KiB and assert OutputTooLarge.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("while :; do printf x; done");
        let limits = SpawnLimits {
            deadline: Duration::from_secs(30),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        };
        let started = Instant::now();
        let err = run_with_limits(&mut cmd, limits).expect_err("must overflow");
        let elapsed = started.elapsed();
        match err {
            SpawnError::OutputTooLarge {
                stream,
                limit,
                killed,
            } => {
                assert_eq!(stream, Stream::Stdout, "must flag stdout overflow");
                assert_eq!(limit, 1024);
                assert!(killed, "kill syscall must succeed for a looping shell");
            }
            other => panic!("expected OutputTooLarge, got {other:?}"),
        }
        // The helper must return shortly after overflow, not
        // wait the full 30s deadline out.
        assert!(
            elapsed < Duration::from_secs(2),
            "helper hung past overflow: elapsed={elapsed:?}",
        );
    }

    #[test]
    fn under_limit_command_succeeds() {
        // Hello-world output must round-trip cleanly at a 1 MiB
        // cap. Regression guard: an off-by-one in the cap check
        // would clip "hello" at boundary 5.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("printf hello");
        let limits = SpawnLimits {
            deadline: Duration::from_secs(10),
            max_stdout_bytes: 1 << 20,
            max_stderr_bytes: 1 << 20,
        };
        let out = run_with_limits(&mut cmd, limits).expect("under-limit succeeds");
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn stderr_cap_fires_on_overflow() {
        // Same threat model as the stdout test, but the loud
        // child writes exclusively to stderr. The fan-out under
        // observe is symmetric — a wedged `gh` could flood either
        // pipe.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("while :; do printf x 1>&2; done");
        let limits = SpawnLimits {
            deadline: Duration::from_secs(30),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        };
        let err = run_with_limits(&mut cmd, limits).expect_err("must overflow");
        match err {
            SpawnError::OutputTooLarge {
                stream,
                limit,
                killed,
            } => {
                assert_eq!(stream, Stream::Stderr, "must flag stderr overflow");
                assert_eq!(limit, 1024);
                assert!(killed, "kill syscall must succeed for a looping shell");
            }
            other => panic!("expected OutputTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn both_streams_under_their_separate_caps_succeeds() {
        // 1 KiB on each pipe with 2 KiB caps must pass: the caps
        // are independent, so a tight cap on one does not starve
        // the other.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("head -c 1024 /dev/zero; head -c 1024 /dev/zero 1>&2");
        let limits = SpawnLimits {
            deadline: Duration::from_secs(10),
            max_stdout_bytes: 2048,
            max_stderr_bytes: 2048,
        };
        let out = run_with_limits(&mut cmd, limits).expect("under both caps");
        assert_eq!(out.stdout.len(), 1024);
        assert_eq!(out.stderr.len(), 1024);
    }

    #[test]
    fn output_too_large_error_display_names_stream_and_limit() {
        let err = SpawnError::OutputTooLarge {
            stream: Stream::Stderr,
            limit: 4096,
            killed: true,
        };
        let s = err.to_string();
        assert!(s.contains("stderr"), "display should name stream: {s}");
        assert!(s.contains("4096"), "display should include limit: {s}");
    }
}
