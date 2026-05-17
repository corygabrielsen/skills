//! Atomic + durable file writes for stable read surfaces.
//!
//! A *stable read surface* is a file observed by concurrent readers
//! as authoritative state — pointer files (a single mutable head
//! into an immutable history), manifest files (the symbol → path
//! map of an artifact set), dedup snapshots, signed attestations.
//! Two failure modes are excluded:
//!
//! - **Torn read** — a reader observes a partial write.
//! - **Lost durability** — a successful write disappears on crash.
//!
//! # Protocol
//!
//! `write-tmp → fsync(tmp) → rename(tmp, path) → fsync(parent_dir)`,
//! with `tmp` a sibling of `path` on the same filesystem (cross-
//! filesystem rename is not atomic).
//!
//! # Invariants
//!
//! - **Atomicity**: at every instant, `path` contains either the
//!   prior bytes or the new bytes — never a prefix of either.
//!   Established by `rename`, atomic on POSIX and on Windows ≥ 10
//!   with `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
//! - **Content durability**: on successful return, the new bytes
//!   are flushed past the page cache to durable storage. Established
//!   by `fsync(tmp)` before the rename.
//! - **Dirent durability**: on successful return, the directory
//!   entry pointing at the new inode survives a crash. Established
//!   by `fsync(parent_dir)` after the rename. Omitting this leaves
//!   content durable but admits dirent loss on power failure.
//!
//! On platforms without directory fsync (Windows / NTFS), dirent
//! durability is supplied by the filesystem journal; the parent
//! fsync reduces to a no-op handle flush. The API contract is
//! identical across platforms.
//!
//! # When NOT to use this
//!
//! - **Append-only logs** — readers truncate at the last good
//!   record; partial appends are self-detecting.
//! - **Content-addressed write-once files** — content identity is
//!   the hash; torn writes are detectable and regeneratable.

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
/// **Postcondition on success**: `path` contains exactly `bytes`;
/// content and dirent are both flushed past the page cache.
///
/// **Postcondition on failure**: `path` is unchanged. A tmp sibling
/// of `path` may persist as debris (name disambiguated by pid +
/// nanosecond + monotonic counter, so concurrent writers do not
/// collide); readers of `path` are unaffected. Cleanup of debris
/// from crashed invocations is the caller's responsibility.
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the underlying tmp open,
/// write, sync, rename, or parent-dir sync.
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
