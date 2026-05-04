//! Fitness skill invocation with bounded retry/backoff.
//!
//! Spawns the fitness command (an opaque argv), captures stdout/stderr,
//! parses the JSON report. Classifies stderr for retry decisions.
//! Zero domain knowledge — the argv is caller-provided.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::protocol::FitnessReport;

const MAX_ATTEMPTS: u32 = 3;
const BASE_BACKOFF_MS: u64 = 1_000;

/// Classified error from a fitness invocation.
#[derive(Debug)]
pub enum FitnessError {
    /// Transient — worth retrying (rate limit, network).
    Transient(String),
    /// Permanent — don't retry (auth, crash, parse failure).
    Permanent(String),
    /// Cancelled via signal.
    Cancelled,
}

impl std::fmt::Display for FitnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(s) | Self::Permanent(s) => write!(f, "{s}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

fn classify_stderr(stderr: &str) -> FitnessError {
    let is_transient = stderr.contains("rate limit")
        || stderr.contains("secondary rate limit")
        || stderr.contains("ECONNRESET")
        || stderr.contains("ETIMEDOUT")
        || stderr.contains("ENOTFOUND")
        || stderr.contains("EAI_AGAIN");

    if is_transient {
        FitnessError::Transient(stderr.to_string())
    } else {
        FitnessError::Permanent(stderr.to_string())
    }
}

/// Invoke the fitness skill with retry/backoff. Returns the parsed report
/// or a classified error. `cancelled` is checked between attempts.
pub fn invoke(argv: &[String], cancelled: &AtomicBool) -> Result<FitnessReport, FitnessError> {
    let (cmd, args) = argv
        .split_first()
        .ok_or_else(|| FitnessError::Permanent("empty fitness argv".to_string()))?;

    let mut last_stderr = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FitnessError::Cancelled);
        }

        let output = Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                last_stderr = e.to_string();
                if attempt == MAX_ATTEMPTS {
                    return Err(FitnessError::Permanent(last_stderr));
                }
                sleep_backoff(attempt, cancelled)?;
                continue;
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        last_stderr = stderr.clone();

        if output.status.success() {
            let report: FitnessReport = serde_json::from_slice(&output.stdout)
                .map_err(|e| FitnessError::Permanent(format!("JSON parse failed: {e}")))?;
            return Ok(report);
        }

        let err = classify_stderr(&stderr);
        match &err {
            FitnessError::Permanent(_) => return Err(err),
            FitnessError::Transient(_) => {
                if attempt == MAX_ATTEMPTS {
                    return Err(FitnessError::Permanent(format!(
                        "exhausted {MAX_ATTEMPTS} attempts: {last_stderr}"
                    )));
                }
                sleep_backoff(attempt, cancelled)?;
            }
            FitnessError::Cancelled => return Err(err),
        }
    }

    Err(FitnessError::Permanent(format!(
        "exhausted {MAX_ATTEMPTS} attempts: {last_stderr}"
    )))
}

fn sleep_backoff(attempt: u32, cancelled: &AtomicBool) -> Result<(), FitnessError> {
    let ms = BASE_BACKOFF_MS * 2u64.pow(attempt - 1);
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FitnessError::Cancelled);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
