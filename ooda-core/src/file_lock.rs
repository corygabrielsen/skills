//! Cross-process advisory file lock as a typed RAII guard.
//!
//! Wraps stdlib `File::try_lock` / `File::lock` (stable since Rust
//! 1.89) on a sidecar `<path>.lock` file. The protected file remains
//! the caller's read/write target; the sidecar exists only to anchor
//! the kernel's advisory lock so the protected file's bytes are
//! never opened with the lock fd.
//!
//! # When to use
//!
//! Any cross-process read-modify-write window on a shared file. The
//! per-write atomicity supplied by [`crate::atomic_io::write_atomic`]
//! prevents torn reads but does NOT prevent two processes from both
//! reading a prior value, both deciding to write, and both writing
//! distinct new values. The whole read → decide → write window must
//! be flocked.
//!
//! # Semantics
//!
//! - **Advisory**: cooperating processes only. Code that opens the
//!   protected file without acquiring the lock bypasses the
//!   discipline. The lock is a convention enforced by every cross-
//!   process write site.
//! - **FD-tied**: the kernel releases the lock when the underlying
//!   file descriptor closes — including SIGKILL or any crash mode.
//!   A stale `.lock` file is harmless; the next `acquire` reopens it
//!   and succeeds.
//! - **Exclusive**: the only API supplied. Shared (read) locking is
//!   not exposed because every current call site needs exclusive
//!   access for the RMW window.
//!
//! # Sidecar naming
//!
//! `FileLock::for_path(p)` opens `{p}.lock` next to `p`. The sidecar
//! is created on demand with `create(true).write(true).truncate(false)`
//! so concurrent first-time openers do not race on `O_EXCL`. Truncate
//! is OFF because the file's content does not matter — only its
//! inode does.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// RAII guard around a held advisory lock on a sidecar file.
///
/// The lock releases when this value drops (the underlying `File`
/// closes its descriptor). Holding the guard across a read → decide
/// → write window serialises that window cross-process.
#[derive(Debug)]
pub struct FileLock {
    // The file descriptor is what holds the lock. Drop releases it
    // via `File`'s Drop closing the fd; the field is unread by name
    // outside of the cfg(test) accessor below.
    #[cfg_attr(not(test), allow(dead_code))]
    file: File,
    sidecar: PathBuf,
}

impl FileLock {
    /// Block until the exclusive advisory lock on `{path}.lock` is
    /// held by this process.
    ///
    /// Creates the sidecar on demand. Any parent directory must
    /// already exist; callers that need parent creation should
    /// `fs::create_dir_all(path.parent())` first.
    ///
    /// # Errors
    ///
    /// - [`io::Error`] — sidecar open failure (permission, no parent
    ///   dir, etc.) or `lock` syscall failure.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let sidecar = sidecar_path(path);
        let file = open_sidecar(&sidecar)?;
        file.lock()?;
        Ok(Self { file, sidecar })
    }

    /// Non-blocking variant. Returns `Ok(None)` when another process
    /// already holds the lock; returns `Ok(Some(_))` on acquisition.
    ///
    /// # Errors
    ///
    /// - [`io::Error`] — sidecar open failure or non-contention
    ///   `try_lock` failure.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let sidecar = sidecar_path(path);
        let file = open_sidecar(&sidecar)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file, sidecar })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// Sidecar path this guard holds. Diagnostic use only; the lock
    /// state is owned by the guard.
    #[must_use]
    pub fn sidecar(&self) -> &Path {
        &self.sidecar
    }

    /// Reference the underlying file descriptor. Test-only — the
    /// release contract is "drop the guard"; production code must
    /// not poke at the fd.
    #[cfg(test)]
    fn fd(&self) -> &File {
        &self.file
    }
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

fn open_sidecar(sidecar: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(sidecar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn acquire_creates_sidecar_next_to_path() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("state.json");
        let guard = FileLock::acquire(&target).unwrap();
        assert_eq!(guard.sidecar(), dir.path().join("state.json.lock"));
        assert!(guard.sidecar().exists());
    }

    #[test]
    fn try_acquire_returns_none_when_held() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("state.json");
        let held = FileLock::acquire(&target).unwrap();
        let second = FileLock::try_acquire(&target).unwrap();
        assert!(second.is_none(), "second try_acquire must see contention");
        drop(held);
        let third = FileLock::try_acquire(&target).unwrap();
        assert!(third.is_some(), "lock must release on drop");
    }

    #[test]
    fn drop_releases_lock_for_subsequent_acquire() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("state.json");
        {
            let _g = FileLock::acquire(&target).unwrap();
        }
        // No deadlock: prior drop released the lock.
        let _g2 = FileLock::acquire(&target).unwrap();
    }

    #[test]
    fn threads_serialise_through_acquire() {
        // Two threads racing through acquire() must observe a
        // strict ordering — never simultaneous interior execution.
        let dir = tempdir().unwrap();
        let target = dir.path().join("state.json");
        let inside = Arc::new(AtomicU32::new(0));
        let max_inside = Arc::new(AtomicU32::new(0));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let target = target.clone();
                let inside = Arc::clone(&inside);
                let max_inside = Arc::clone(&max_inside);
                thread::spawn(move || {
                    let _g = FileLock::acquire(&target).unwrap();
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_inside.fetch_max(n, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(10));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_inside.load(Ordering::SeqCst),
            1,
            "no two threads may hold the lock simultaneously",
        );
    }

    #[test]
    fn sidecar_is_distinct_from_protected_path() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("data.bin");
        let guard = FileLock::acquire(&target).unwrap();
        assert_ne!(guard.sidecar(), target);
        // We never opened `target`; only the sidecar.
        assert!(!target.exists());
    }

    #[test]
    fn fd_is_writable_and_persists() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("data.bin");
        let guard = FileLock::acquire(&target).unwrap();
        // Sanity: the underlying File is usable.
        assert!(guard.fd().metadata().is_ok());
    }
}
