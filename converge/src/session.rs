//! Session management: lock, history, exit.json.
//!
//! Sessions live at `/tmp/converge/{session-id}/`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::halt::{HaltReport, IterLog};

const BASE_DIR: &str = "/tmp/converge";

pub(crate) struct Session {
    pub dir: PathBuf,
    pub history: Vec<IterLog>,
    lock_path: PathBuf,
}

impl Session {
    /// Open (or create) a session directory. Acquires a lock file.
    ///
    /// On `AlreadyExists`, the lockfile body (PID of the prior
    /// owner) is consulted: if that PID is no longer alive, the
    /// stale lock is unlinked and the acquisition retries once.
    /// This covers SIGINT / SIGKILL / panic-unwind paths that
    /// bypass [`Self::release`] (and that [`Drop`] cannot catch).
    pub(crate) fn open(session_id: &str) -> Result<Self, String> {
        let dir = PathBuf::from(BASE_DIR).join(session_id);
        ooda_core::atomic_io::secure_create_dir_all(&dir)
            .map_err(|e| format!("cannot create session dir {}: {e}", dir.display()))?;

        let lock_path = dir.join("lock");
        // Atomic lock via O_CREAT|O_EXCL — no TOCTOU race.
        // Retry once on AlreadyExists if the prior owner PID is dead
        // (covers SIGKILL / panic paths that bypass Drop).
        for attempt in 0..2 {
            match ooda_core::atomic_io::open_secure_create_new(&lock_path) {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    let history = load_history(&dir);
                    return Ok(Self {
                        dir,
                        history,
                        lock_path,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let content = fs::read_to_string(&lock_path).unwrap_or_default();
                    let prior_pid = content.trim().parse::<u32>().ok();
                    if attempt == 0 && prior_pid.is_some_and(|p| !is_pid_alive(p)) {
                        // Stale lock — owner is dead. Unlink and retry.
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    return Err(format!("session locked (pid {content})"));
                }
                Err(e) => return Err(format!("cannot create lock: {e}")),
            }
        }
        Err("session locked (pid unknown)".to_string())
    }

    pub(crate) fn append_history(&mut self, entry: IterLog) -> Result<(), String> {
        let path = self.dir.join("history.jsonl");
        let mut file = ooda_core::atomic_io::open_secure_append(&path)
            .map_err(|e| format!("cannot open history: {e}"))?;
        let line = serde_json::to_string(&entry)
            .map_err(|e| format!("cannot serialize history entry: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("cannot write history: {e}"))?;
        self.history.push(entry);
        Ok(())
    }

    pub(crate) fn write_in_progress(
        &self,
        session_id: &str,
        resume_cmd: &[String],
    ) -> Result<(), String> {
        let stub = serde_json::json!({
            "stage": "in_progress",
            "timestamp": now_iso(),
            "session_id": session_id,
            "resume_cmd": resume_cmd,
        });
        write_json(&self.dir.join("exit.json"), &stub)
    }

    pub(crate) fn write_halt(&self, report: &HaltReport) -> Result<(), String> {
        write_json(&self.dir.join("exit.json"), report)
    }

    pub(crate) fn release(&self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl Drop for Session {
    /// Release the lockfile on best-effort exit (panic unwind, early
    /// return between [`Self::open`] and [`Self::release`]).
    /// SIGKILL / SIGTERM-without-trap still bypass Drop; readers
    /// reconcile those via the PID-liveness re-acquire path in
    /// [`Self::open`].
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// POSIX `kill(pid, 0)` liveness probe (mirrors the helper in
/// `ooda-state` and `cockpit`). Used by [`Session::open`] to
/// reclaim leaked lockfiles whose owner is no longer running.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
    let rc = unsafe { libc_kill(pid_i32, 0) };
    if rc == 0 {
        return true;
    }
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(1))
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn load_history(dir: &Path) -> Vec<IterLog> {
    let path = dir.join("history.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialize: {e}"))?;
    ooda_core::atomic_io::write_atomic(path, format!("{json}\n").as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// ISO 8601 UTC timestamp without external dependencies.
pub(crate) fn now_iso() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    epoch_to_iso(dur.as_secs())
}

/// Convert epoch seconds to ISO 8601 UTC string.
/// Extracted from `now_iso` to enable deterministic testing.
fn epoch_to_iso(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let mut days = secs / 86400;
    // Compute year/month/day from days since epoch (1970-01-01).
    let mut y = 1970i64;
    loop {
        let dy = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0usize;
    while mo < 11 && days >= mdays[mo] {
        days -= mdays[mo];
        mo += 1;
    }
    format!("{y:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z", mo + 1, days + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_format_matches_iso8601() {
        let ts = now_iso();
        // Pattern: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
        assert_eq!(&ts[19..20], "Z");
        // Year, month, day, hour, minute, second should all parse as numbers.
        ts[0..4].parse::<u32>().expect("year");
        ts[5..7].parse::<u32>().expect("month");
        ts[8..10].parse::<u32>().expect("day");
        ts[11..13].parse::<u32>().expect("hour");
        ts[14..16].parse::<u32>().expect("minute");
        ts[17..19].parse::<u32>().expect("second");
    }

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_epoch_2026_04_17_noon() {
        // 2026-04-17T12:00:00Z = 1776427200 epoch seconds.
        assert_eq!(epoch_to_iso(1_776_427_200), "2026-04-17T12:00:00Z");
    }

    #[test]
    fn leap_year_feb_29() {
        // 2024-02-29T00:00:00Z = 1709164800 epoch seconds.
        assert_eq!(epoch_to_iso(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn end_of_day() {
        // 2026-04-17T23:59:59Z = 1776470399 epoch seconds.
        assert_eq!(epoch_to_iso(1_776_470_399), "2026-04-17T23:59:59Z");
    }

    #[test]
    fn year_2000_leap_century() {
        // 2000-03-01T00:00:00Z = 951868800 epoch seconds.
        // 2000 is a leap year (divisible by 400).
        assert_eq!(epoch_to_iso(951_868_800), "2000-03-01T00:00:00Z");
    }
}
