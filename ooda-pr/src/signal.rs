//! Trapped-signal shutdown plumbing.
//!
//! `SIGINT` / `SIGTERM` set an atomic that the OODA loop polls at
//! each iteration boundary. The loop owns the halt path — the
//! handler never touches the recorder. This avoids racing the
//! writer's lock from a signal context and keeps the terminal
//! event + live-marker cleanup on the same code path as every
//! other halt.
//!
//! ```text
//! signal-hook → SHUTDOWN_SIGNAL.store(N)
//!            ← Recorder.halt(RunHalted{outcome:"SignalInterrupted", ...})
//!     loop body: if let Some(code) = check_shutdown() { return ... }
//! ```
//!
//! Exit-code values (130 = `SIGINT`, 143 = `SIGTERM`) match the
//! POSIX `128 + N` convention so a wrapper script cannot
//! distinguish the trapped path from an uncaught signal on `$?`.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

use ooda_core::ExitCode;

/// Last observed signal as a process-exit code. `0` means no signal
/// has fired; `130` / `143` mean `SIGINT` / `SIGTERM` respectively.
/// Read by the loop driver via [`check_shutdown`]; written by the
/// signal handler installed by [`install_signal_handlers`].
static SHUTDOWN_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Install `SIGINT` / `SIGTERM` handlers that store the matching
/// POSIX exit code into [`SHUTDOWN_SIGNAL`]. The handlers do no
/// other work — every side effect lives on the loop's halt path.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] from `signal-hook` if
/// registration fails (e.g. the process is sandboxed without
/// permission to install handlers). Callers should treat this as
/// fatal — without handlers, the binary cannot honour the
/// graceful-shutdown contract.
pub(crate) fn install_signal_handlers() -> Result<(), io::Error> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::low_level;
    // SAFETY: handlers do only an atomic store — no allocation,
    // no syscalls, no locking. This is exactly the operation
    // `signal-hook` documents as safe inside `low_level::register`.
    unsafe {
        low_level::register(SIGINT, || {
            SHUTDOWN_SIGNAL.store(i32::from(ExitCode::SignalSigint.as_u8()), Ordering::SeqCst);
        })?;
        low_level::register(SIGTERM, || {
            SHUTDOWN_SIGNAL.store(i32::from(ExitCode::SignalSigterm.as_u8()), Ordering::SeqCst);
        })?;
    }
    Ok(())
}

/// Poll the shutdown atomic. Returns `Some(code)` if a signal has
/// fired since the last call (the loop should halt with
/// `Outcome::SignalInterrupted { exit_code: code }`); `None`
/// otherwise.
#[must_use]
pub(crate) fn check_shutdown() -> Option<u8> {
    let sig = SHUTDOWN_SIGNAL.load(Ordering::SeqCst);
    if sig == 0 {
        None
    } else {
        // The stored value is always one of the two POSIX exit
        // codes (130 / 143); the `try_into` is a structural
        // backstop, falling back to the SIGTERM token if the
        // atomic somehow holds an out-of-range value.
        Some(u8::try_from(sig).unwrap_or(ExitCode::SignalSigterm.as_u8()))
    }
}

/// Test-only: arm the shutdown atomic without installing handlers
/// so unit tests can drive the loop's signal-poll path
/// deterministically. Paired with [`reset_for_test`].
#[cfg(test)]
pub(crate) fn set_for_test(code: u8) {
    SHUTDOWN_SIGNAL.store(i32::from(code), Ordering::SeqCst);
}

/// Test-only: clear the shutdown atomic so sibling tests in the
/// same process observe a clean slate.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    SHUTDOWN_SIGNAL.store(0, Ordering::SeqCst);
}
