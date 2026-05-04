//! Session management: lock, history, exit.json.
//!
//! Sessions live at `/tmp/converge/{session-id}/`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::halt::{HaltReport, IterLog};

const BASE_DIR: &str = "/tmp/converge";

pub struct Session {
    pub dir: PathBuf,
    pub history: Vec<IterLog>,
    lock_path: PathBuf,
}

impl Session {
    /// Open (or create) a session directory. Acquires a lock file.
    pub fn open(session_id: &str) -> Result<Self, String> {
        let dir = PathBuf::from(BASE_DIR).join(session_id);
        fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create session dir {}: {e}", dir.display()))?;

        let lock_path = dir.join("lock");
        // Atomic lock via O_CREAT|O_EXCL — no TOCTOU race.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let content = fs::read_to_string(&lock_path).unwrap_or_default();
                return Err(format!("session locked (pid {content})"));
            }
            Err(e) => return Err(format!("cannot create lock: {e}")),
        }

        let history = load_history(&dir);

        Ok(Self {
            dir,
            history,
            lock_path,
        })
    }

    pub fn append_history(&mut self, entry: IterLog) -> Result<(), String> {
        let path = self.dir.join("history.jsonl");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open history: {e}"))?;
        let line = serde_json::to_string(&entry)
            .map_err(|e| format!("cannot serialize history entry: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("cannot write history: {e}"))?;
        self.history.push(entry);
        Ok(())
    }

    pub fn write_in_progress(&self, session_id: &str, resume_cmd: &[String]) -> Result<(), String> {
        let stub = serde_json::json!({
            "stage": "in_progress",
            "timestamp": now_iso(),
            "session_id": session_id,
            "resume_cmd": resume_cmd,
        });
        write_json(&self.dir.join("exit.json"), &stub)
    }

    pub fn write_halt(&self, report: &HaltReport) -> Result<(), String> {
        write_json(&self.dir.join("exit.json"), report)
    }

    pub fn release(&self) {
        let _ = fs::remove_file(&self.lock_path);
    }
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
    fs::write(path, format!("{json}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// ISO 8601 UTC timestamp without external dependencies.
pub fn now_iso() -> String {
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
        assert_eq!(epoch_to_iso(1776427200), "2026-04-17T12:00:00Z");
    }

    #[test]
    fn leap_year_feb_29() {
        // 2024-02-29T00:00:00Z = 1709164800 epoch seconds.
        assert_eq!(epoch_to_iso(1709164800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn end_of_day() {
        // 2026-04-17T23:59:59Z = 1776470399 epoch seconds.
        assert_eq!(epoch_to_iso(1776470399), "2026-04-17T23:59:59Z");
    }

    #[test]
    fn year_2000_leap_century() {
        // 2000-03-01T00:00:00Z = 951868800 epoch seconds.
        // 2000 is a leap year (divisible by 400).
        assert_eq!(epoch_to_iso(951868800), "2000-03-01T00:00:00Z");
    }
}
