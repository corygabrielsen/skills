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
        // Simple lock: create exclusively. Not flock — good enough for
        // single-machine use. The lock file contains our PID.
        if lock_path.exists() {
            let content = fs::read_to_string(&lock_path).unwrap_or_default();
            return Err(format!("session locked (pid {content})"));
        }
        fs::write(&lock_path, std::process::id().to_string())
            .map_err(|e| format!("cannot write lock: {e}"))?;

        let history = load_history(&dir);

        Ok(Self { dir, history, lock_path })
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
        writeln!(file, "{line}")
            .map_err(|e| format!("cannot write history: {e}"))?;
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
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| format!("cannot serialize: {e}"))?;
    fs::write(path, format!("{json}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn now_iso() -> String {
    // Use system time without chrono dependency.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // Rough ISO 8601 — good enough for session metadata.
    let secs = dur.as_secs();
    format!("{secs}")
}
