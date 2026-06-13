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

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

/// Unix mode bits for sensitive files: owner read+write only.
///
/// Exposed for call sites whose `OpenOptions` shape is not covered by
/// the [`open_secure_append`] / [`open_secure_create_new`] /
/// [`open_secure_truncate`] helpers and must apply the mode directly.
#[cfg(unix)]
pub const SECURE_FILE_MODE: u32 = 0o600;

/// Unix mode bits for sensitive directories: owner read+write+execute only.
#[cfg(unix)]
const SECURE_DIR_MODE: u32 = 0o700;

/// Process-wide monotonic counter mixed into tmp filenames.
///
/// Kept for forensics — paired with `pid` and `nanos`, the suffix
/// `<pid>.<nanos>.<seq>` lets an operator reconstruct the ordering
/// of orphaned tmp files from a crashed process. Collision
/// avoidance against co-tenants is supplied by the urandom salt in
/// [`tmp_salt`]; `SEQ` alone is predictable and not load-bearing
/// for that property.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically and durably.
///
/// **Postcondition on success**: `path` contains exactly `bytes`;
/// content and dirent are both flushed past the page cache.
///
/// **Postcondition on failure**: `path` is unchanged. A tmp sibling
/// of `path` may persist as debris (name disambiguated by pid +
/// nanosecond + monotonic counter + 8-byte urandom salt — concurrent
/// writers do not collide, and a co-tenant local user cannot
/// predict the tmp name in advance to pre-create it and force
/// every `O_CREAT|O_EXCL` open to fail with `EEXIST`); readers of
/// `path` are unaffected. Cleanup of debris from crashed
/// invocations is the caller's responsibility.
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
    secure_create_dir_all(parent)?;

    let tmp = tmp_sibling(path);
    // Drop the tmp file's handle BEFORE the rename so Windows
    // (which won't rename an open file) works. The Linux/macOS
    // path doesn't strictly need this but it makes the contract
    // portable.
    {
        let mut f = open_secure_create_new(&tmp)?;
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

/// Create `path` (and any missing ancestors) with directory mode
/// `0o700` on unix — owner-only traversal.
///
/// **Security invariant**: every directory component created by this
/// call ends up `0o700`. Pre-existing components are left alone (the
/// caller is not the owner of state created by some prior process).
/// On non-unix targets this reduces to [`fs::create_dir_all`]; the
/// API contract is identical so call sites do not branch.
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the underlying
/// `DirBuilder::create` call.
pub fn secure_create_dir_all(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        if path.is_dir() {
            return Ok(());
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(SECURE_DIR_MODE);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Open `path` for append, creating with mode `0o600` on unix if it
/// does not already exist.
///
/// **Security invariant**: a freshly-created file is owner-readable
/// and owner-writable only. Existing files keep their current mode
/// (chmod would be a TOCTOU race with concurrent writers).
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the underlying open.
pub fn open_secure_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        options.mode(SECURE_FILE_MODE);
    }
    options.open(path)
}

/// Open `path` exclusively (`O_CREAT | O_EXCL`) with mode `0o600` on
/// unix. Fails if `path` already exists.
///
/// **Security invariant**: the file is created by this call and is
/// owner-read/write only. Used for tmp siblings, lock files, and
/// content-addressed blobs where the caller asserts uniqueness.
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the underlying open.
pub fn open_secure_create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(SECURE_FILE_MODE);
    }
    options.open(path)
}

/// Open `path` for write, truncating it to zero, creating with mode
/// `0o600` on unix if it does not already exist.
///
/// **Security invariant**: a freshly-created file is owner-read/write
/// only. Existing files keep their current mode.
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from the underlying open.
pub fn open_secure_truncate(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(SECURE_FILE_MODE);
    }
    options.open(path)
}

fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    let salt = tmp_salt();
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{pid}.{nanos}.{seq}.{salt:016x}"));
    std::path::PathBuf::from(tmp)
}

/// 64-bit unpredictable salt for tmp-sibling filenames.
///
/// **Threat model**: pid is observable via `/proc` and the wall
/// clock is approximable, so a co-tenant local user can predict the
/// `pid` + `nanos` + `seq` portion of the tmp name and pre-create
/// it. Every subsequent `O_CREAT|O_EXCL` open of the predicted name
/// then fails with `EEXIST`, bricking `write_atomic` (and through
/// it, dedup snapshots, sticky pointers, attestations, blob writes
/// — every stable read surface that goes through atomic rename).
/// Appending 64 bits of unpredictable entropy makes pre-create
/// computationally infeasible.
///
/// **Primary source**: 8 bytes from `/dev/urandom`. On Linux the
/// device is always present and reads from the kernel CSPRNG
/// without blocking.
///
/// **Fallback** (on read failure or non-unix targets): mix
/// `SystemTime::now()` with `process::id()`. Degraded entropy
/// — the result is predictable to an attacker with the same
/// information they already had — but strictly no worse than the
/// pre-salt format, so callers never lose ground if urandom is
/// unreachable.
fn tmp_salt() -> u64 {
    #[cfg(unix)]
    {
        if let Ok(bytes) = read_urandom_8() {
            return u64::from_ne_bytes(bytes);
        }
    }
    // Degraded fallback. The wrapping_mul folds the high bits of
    // pid into every bit of the result so the suffix at least
    // varies between sibling processes that happen to schedule on
    // the same nanosecond.
    #[allow(clippy::cast_possible_truncation)]
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let pid = u64::from(process::id());
    nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64) ^ pid
}

#[cfg(unix)]
fn read_urandom_8() -> std::io::Result<[u8; 8]> {
    let mut f = File::open("/dev/urandom")?;
    let mut buf = [0u8; 8];
    f.read_exact(&mut buf)?;
    Ok(buf)
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

    #[cfg(unix)]
    #[test]
    fn write_atomic_sets_secure_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "write_atomic must create files at 0o600");
    }

    #[cfg(unix)]
    #[test]
    fn secure_create_dir_all_sets_dir_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        secure_create_dir_all(&nested).unwrap();
        for p in [
            nested.as_path(),
            nested.parent().unwrap(),
            nested.parent().unwrap().parent().unwrap(),
        ] {
            let mode = fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be 0o700", p.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_secure_append_creates_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let mut f = open_secure_append(&path).unwrap();
        f.write_all(b"line\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn open_secure_create_new_creates_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("blob");
        let _ = open_secure_create_new(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn open_secure_truncate_creates_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("scratch.log");
        let _ = open_secure_truncate(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
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

    #[test]
    fn tmp_sibling_carries_urandom_salt_suffix() {
        // The tmp name MUST end with `.tmp.<pid>.<nanos>.<seq>.<16
        // hex chars>`. Without the trailing 16-hex-char salt, a
        // co-tenant who can observe `pid` and the wall clock can
        // pre-create the predicted file and brick every subsequent
        // `write_atomic`.
        let name = tmp_sibling(Path::new("/var/lib/x/data"));
        let s = name.to_str().unwrap();
        let suffix = s.strip_prefix("/var/lib/x/data").unwrap();
        // Suffix starts with '.', so the first split element is "".
        let parts: Vec<&str> = suffix.split('.').collect();
        // Expected: ["", "tmp", "<pid>", "<nanos>", "<seq>", "<salt>"]
        assert_eq!(parts.len(), 6, "unexpected suffix shape: {suffix}");
        assert_eq!(parts[0], "");
        assert_eq!(parts[1], "tmp");
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[3].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[4].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[5].len(), 16, "salt must be 16 hex chars");
        assert!(
            parts[5].chars().all(|c| c.is_ascii_hexdigit()),
            "salt must be hex: {}",
            parts[5]
        );
    }

    #[test]
    fn tmp_sibling_salt_varies_between_calls() {
        // Back-to-back calls MUST produce different salts. Without
        // unpredictable entropy in the suffix the threat from M6
        // (co-tenant pre-create) is not closed.
        let p = Path::new("/tmp/example.json");
        let a = tmp_sibling(p);
        let b = tmp_sibling(p);
        let a_salt = a.to_str().unwrap().rsplit('.').next().unwrap();
        let b_salt = b.to_str().unwrap().rsplit('.').next().unwrap();
        assert_eq!(a_salt.len(), 16);
        assert_eq!(b_salt.len(), 16);
        assert_ne!(a_salt, b_salt, "consecutive salts must differ");
    }

    #[test]
    fn tmp_sibling_names_unique_across_10000_calls() {
        // 10000 calls in a tight loop, all names must be unique.
        // `SEQ` alone guarantees uniqueness; the stronger assertion
        // — that the urandom salt component is also non-constant —
        // is covered by [`tmp_sibling_salt_varies_between_calls`].
        use std::collections::HashSet;
        let p = Path::new("/tmp/x");
        let names: HashSet<_> = (0..10_000_u32).map(|_| tmp_sibling(p)).collect();
        assert_eq!(names.len(), 10_000, "tmp names collided");
        // And: 10000 distinct salts (i.e., not stuck on a single
        // fallback value).
        let salts: HashSet<String> = (0..10_000_u32)
            .map(|_| {
                tmp_sibling(p)
                    .to_str()
                    .unwrap()
                    .rsplit('.')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        // Allow a few duplicates from the birthday paradox at 64 bits
        // (P(collision in 10k draws) ≈ 2.7e-12 — vanishingly small),
        // but anything less than 9999 indicates the salt is stuck.
        assert!(
            salts.len() >= 9_999,
            "salt is not non-deterministic: only {} unique values in 10000 draws",
            salts.len()
        );
    }

    #[test]
    fn write_atomic_tmp_file_name_carries_salt() {
        // `write_atomic` picks its tmp name via `tmp_sibling`, then
        // opens it with `O_CREAT|O_EXCL`. The salt suffix on the
        // tmp name is what defeats the pre-create race; verify it
        // is present on the exact path `write_atomic` would use for
        // a real destination.
        let dir = tempdir().unwrap();
        let dest = dir.path().join("state.json");
        let tmp = tmp_sibling(&dest);
        let salt = tmp
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap();
        assert_eq!(salt.len(), 16);
        assert!(salt.chars().all(|c| c.is_ascii_hexdigit()));
        // And the end-to-end write still succeeds with the salted
        // tmp path.
        write_atomic(&dest, b"payload").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
    }
}
