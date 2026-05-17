//! Atomic + durable file writes for stable read surfaces.
//!
//! Stable read surfaces are files that another process / iteration
//! / user reads as authoritative state: `CURRENT.json`,
//! `dedup.json`, attestation files, manifest files. They must
//! never be observed in a partial state by a concurrent reader,
//! and must survive a crash without truncation.
//!
//! The pattern is "write to tmp, fsync tmp, rename, fsync parent":
//!
//! 1. Open `<path>.tmp.<pid>.<nanos>` in the destination's directory
//!    (must be the same filesystem for `rename` atomicity).
//! 2. Write the bytes.
//! 3. `sync_all()` the tmp file — flushes the data to disk before
//!    we rename it into place.
//! 4. `rename(tmp, path)` — atomic on POSIX (`man 2 rename`) and on
//!    Windows ≥ 10 with `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
//! 5. Open the parent directory and `sync_all()` it — flushes the
//!    directory entry update so the rename survives crash.
//!
//! Without step 5, the rename is durable for the file *contents*
//! but the directory entry pointing at it may be lost on power
//! failure (the inode is committed, the dirent isn't). The Linux
//! manpage for `rename` says: "If newpath already exists, it will
//! be atomically replaced ... callers ... must ensure that the
//! directory entries are durably written by calling `fsync` on the
//! containing directory."
//!
//! Step 5 is a no-op on Windows (NTFS has no concept of fsync-ing
//! a directory handle); the `sync_all` call there reduces to a
//! handle-flush that returns `Ok(())` without doing anything
//! observable. Documented for portability.
//!
//! # When NOT to use this
//!
//! Append-only logs (events.jsonl, ledger.md, trace.md) don't need
//! the tmp+rename pattern — partial appends are truncated by the
//! reader at the last good record. Content-addressed blobs
//! (blobs/sha256/.../<hash>.zst) are write-once and named by their
//! own hash; a torn write is detectable and regeneratable.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide monotonic disambiguator for tmp filenames. Avoids
/// collisions when multiple `write_atomic` calls race within the
/// same nanosecond (or on systems where the clock has low
/// resolution).
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically and durably.
///
/// On success the destination contains exactly `bytes` and the
/// rename + directory update are flushed to disk.
///
/// On failure the tmp file is left in place under
/// `<path>.tmp.<pid>.<nanos>.<seq>`; callers may treat the tmp as
/// debris and ignore it (a subsequent successful write removes it
/// only if it was the one this call created; tmp files from
/// crashed prior invocations stay until a janitor removes them).
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the tmp file open, write,
/// sync, rename, or parent-dir sync steps.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "write_atomic: path has no parent directory",
        )
    })?;
    if parent.as_os_str().is_empty() {
        // `path.parent()` returns `Some("")` for a bare filename;
        // canonicalize to current dir so OpenOptions / rename work.
        return write_atomic(&Path::new(".").join(path), bytes);
    }
    fs::create_dir_all(parent)?;

    let tmp = tmp_sibling(path);
    // Drop the tmp file's handle BEFORE the rename so Windows
    // (which won't rename an open file) works. The Linux/macOS
    // path doesn't strictly need this but it makes the contract
    // portable.
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // Tmp file is debris; best-effort cleanup before returning.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_parent_dir(parent)?;
    Ok(())
}

fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{pid}.{nanos}.{seq}"));
    std::path::PathBuf::from(tmp)
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> std::io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> std::io::Result<()> {
    // Windows / non-POSIX have no equivalent of fsync(dir_fd);
    // directory metadata updates are journaled by the filesystem.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_writes_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"{\"hello\":\"world\"}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"hello\":\"world\"}");
    }

    #[test]
    fn overwrite_replaces_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"old content").unwrap();
        write_atomic(&path, b"new content").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new content");
    }

    #[test]
    fn parent_dirs_are_created() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deeply/state.json");
        write_atomic(&path, b"x").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn empty_bytes_writes_zero_length_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.json");
        write_atomic(&path, b"").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn no_tmp_debris_on_success() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"x").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn many_writes_disambiguate_tmp_names() {
        // 100 rapid writes within the same nanosecond MUST NOT
        // collide on tmp filename — `SEQ` is the disambiguator.
        let dir = tempdir().unwrap();
        for i in 0..100 {
            let path = dir.path().join(format!("f{i}.json"));
            write_atomic(&path, b"x").unwrap();
        }
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 100);
    }
}
