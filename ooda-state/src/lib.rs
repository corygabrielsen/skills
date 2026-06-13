//! OODA state-tree — domain-agnostic on-disk model.
//!
//! # Layout
//!
//! ```text
//! <state-root>/
//! ├── runs/<run-id>/
//! │   ├── events.jsonl      ← source of truth (append-only)
//! │   └── blobs/<sha>.<ext> ← content-addressed payloads
//! └── live/<run-id>         ← empty marker; presence = "active"
//! ```
//!
//! # Domain neutrality
//!
//! The on-disk layout carries NO domain semantics. Run identifiers
//! are opaque (`<run-id>`); domain-specific identity (`pr` slug,
//! `codex-review` level, etc.) lives only inside event records
//! via the `target` payload field on `run.started` events.
//!
//! # Atomicity
//!
//! - `events.jsonl` appended via `O_APPEND`; single-line writes
//!   under `PIPE_BUF` (4096 bytes on POSIX) are atomic w.r.t.
//!   concurrent readers tailing the file.
//! - Blobs written via `tmp+rename` (rename is atomic on the same
//!   filesystem).
//! - Live markers created via `OpenOptions::create_new` (atomic
//!   `O_CREAT|O_EXCL`); deleted via `fs::remove_file` (atomic).
//!
//! Concurrent runs use disjoint paths (distinct `<run-id>`), so the
//! disk layout needs no inter-run locking. Each [`RunWriter`] is a
//! single-threaded handle: `&mut self` rules out in-process aliasing
//! by construction.
//!
//! # Liveness
//!
//! `live/<run-id>` markers can leak across SIGKILL / OOM / power
//! loss. [`RunId::generate`] embeds the writer's PID as the
//! `-p<pid>` suffix; readers filter out markers whose PID is no
//! longer alive (POSIX `kill(pid, 0)`). [`RunWriter`] also
//! implements [`Drop`] to release the marker on unwind, and
//! [`StateRoot::sweep_dead_markers`] reclaims disk space for
//! filtered-out markers.

#![doc(html_root_url = "https://docs.rs/ooda-state/0.1.0")]

pub mod tokens;
pub use tokens::{
    CodexReviewDomain, DecisionKind, Domain, DomainKind, OutcomeKind, PrDomain, blob_path,
    domain_specific, terminal_event,
};

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── Atomicity constants ──────────────────────────────────────────────

/// Cap for a single serialized event line (including the trailing
/// newline). POSIX `PIPE_BUF` is the kernel's guaranteed-atomic
/// `write(2)` size for `O_APPEND`; staying at or below it keeps
/// concurrent appenders from tearing each other's lines. On Linux
/// the documented value is 4096; the same bound is the working
/// floor on all supported platforms.
pub const MAX_EVENT_BYTES: usize = 4096;

// ── Errors ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum StateError {
    Io(io::Error),
    Json(serde_json::Error),
    /// Caller tried to start a run twice (live marker already
    /// exists). This signals either a collision in `<run-id>`
    /// generation (vanishingly unlikely with timestamp+entropy)
    /// or a writer-protocol bug (calling `start` twice on the
    /// same `RunWriter`).
    AlreadyStarted(RunId),
    /// Reader could not find a run with the given id.
    UnknownRun(RunId),
    /// A blob reference points at a missing file. Indicates
    /// corruption: events.jsonl claims a blob the filesystem
    /// doesn't have.
    MissingBlob {
        run_id: RunId,
        blob: BlobRef,
    },
    /// A blob's on-disk content does not hash to its filename.
    /// Indicates corruption or a hash-algorithm mismatch.
    BlobHashMismatch {
        run_id: RunId,
        expected: String,
        actual: String,
    },
    /// A serialized event line exceeded `MAX_EVENT_BYTES`
    /// (`PIPE_BUF`) even after the writer attempted blob
    /// substitution for the overflowing payload. Indicates a
    /// caller embedded oversized non-payload data (e.g. an
    /// unbounded `kind_suffix`); fix by trimming the offending
    /// field or routing it through `write_blob` explicitly.
    EventTooLarge {
        kind: &'static str,
        size: usize,
        cap: usize,
    },
    /// Caller attempted to `create_run` for a run id whose
    /// `events.jsonl` already exists. Signals reuse of a
    /// previously-used run id; create a fresh id via
    /// [`RunId::generate`].
    RunDirExists(RunId),
    /// Caller appended or halted a [`RunWriter`] after it had
    /// already been halted. Post-halt writes are a writer-protocol
    /// bug; the on-disk run is terminal.
    AlreadyHalted(RunId),
    /// A blob's recorded size exceeds the maximum the reader will
    /// allocate in one call. Use [`RunReader::read_blob_stream`]
    /// to consume blobs incrementally.
    BlobTooLarge {
        run_id: RunId,
        size: u64,
        limit: u64,
    },
}

/// Maximum blob size accepted by [`RunReader::read_blob`] (64 MiB).
/// Larger blobs must be read via [`RunReader::read_blob_stream`].
/// The cap is a defense against a corrupted [`BlobRef::size`]
/// triggering unbounded `Vec::with_capacity`.
pub const MAX_INLINE_BLOB_SIZE: u64 = 64 * 1024 * 1024;

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::AlreadyStarted(id) => write!(f, "run already started: {}", id.as_str()),
            Self::UnknownRun(id) => write!(f, "unknown run: {}", id.as_str()),
            Self::MissingBlob { run_id, blob } => write!(
                f,
                "missing blob {}.{} in run {}",
                blob.sha,
                blob.ext,
                run_id.as_str()
            ),
            Self::BlobHashMismatch {
                run_id,
                expected,
                actual,
            } => write!(
                f,
                "blob hash mismatch in run {}: expected {expected}, got {actual}",
                run_id.as_str()
            ),
            Self::EventTooLarge { kind, size, cap } => write!(
                f,
                "event line too large after overflow attempt: kind={kind} size={size} cap={cap}"
            ),
            Self::RunDirExists(id) => {
                write!(f, "run dir already populated: {}", id.as_str())
            }
            Self::AlreadyHalted(id) => write!(f, "run already halted: {}", id.as_str()),
            Self::BlobTooLarge {
                run_id,
                size,
                limit,
            } => write!(
                f,
                "blob in run {} reports size {size} bytes (limit {limit}); use read_blob_stream",
                run_id.as_str()
            ),
        }
    }
}

impl std::error::Error for StateError {}

impl From<io::Error> for StateError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for StateError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, StateError>;

// ── Identity ─────────────────────────────────────────────────────────

/// Opaque run identifier. The on-disk path uses this verbatim;
/// callers should generate via [`RunId::generate`] (timestamp +
/// entropy + pid) or supply their own globally-unique string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(String);

impl RunId {
    /// Wrap an arbitrary string as a run-id. Caller is responsible
    /// for global uniqueness; validation here rejects strings that
    /// would escape the runs/ directory, split framed text streams,
    /// or trip filesystem ergonomics.
    ///
    /// Rejected inputs:
    ///
    /// - empty or whitespace-only
    /// - any ASCII control byte (`< 0x20`, `0x7f`) — covers `\n`,
    ///   `\r`, `\t`, `\0`
    /// - path separators (`/`, `\\`)
    /// - leading `.` (hidden file) or exactly `.` / `..`
    /// - leading or trailing whitespace
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] with [`io::ErrorKind::InvalidInput`]
    /// if any rejection rule fires.
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.is_empty()
            || s.trim().is_empty()
            || s.len() != s.trim().len()
            || s.contains('/')
            || s.contains('\\')
            || s == "."
            || s == ".."
            || s.starts_with('.')
            || s.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            return Err(StateError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid run id: {s:?}"),
            )));
        }
        Ok(Self(s))
    }

    /// Generate a fresh run id from current UTC timestamp,
    /// subsecond-nanosecond entropy, and pid. The format is
    /// `<YYYYMMDDTHHMMSSZ>-<entropy>-p<pid>`.
    #[must_use]
    pub fn generate() -> Self {
        let now = Utc::now();
        let ts = now.format("%Y%m%dT%H%M%SZ");
        // Entropy via system clock subsecond nanos: enough for
        // run-id local uniqueness within one machine within one
        // second. A future revision could lift to a UUID v7 or
        // pull from /dev/urandom.
        let entropy = now.timestamp_subsec_nanos();
        let pid = std::process::id();
        Self(format!("{ts}-{entropy:09}-p{pid}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract the writer-PID suffix from a [`Self::generate`]-shaped
    /// run id. Returns `None` for ids that lack the `-p<digits>`
    /// suffix (e.g. caller-supplied ids via [`Self::new`]).
    ///
    /// Used by [`StateRoot::live_runs`] and writer-startup sweeps to
    /// classify whether a `live/<run-id>` marker belongs to a still-
    /// alive process or to a crashed writer.
    #[must_use]
    pub fn writer_pid(&self) -> Option<u32> {
        let suffix = self.0.rsplit('-').next()?;
        let digits = suffix.strip_prefix('p')?;
        if digits.is_empty() {
            return None;
        }
        digits.parse::<u32>().ok()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Blobs ────────────────────────────────────────────────────────────

/// Reference to a content-addressed blob inside a run's `blobs/`
/// directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lowercase hex of the SHA-256 of the blob's bytes.
    pub sha: String,
    /// Size in bytes of the blob's content.
    pub size: u64,
    /// Filename extension (no leading dot). `"md"`, `"json"`, etc.
    /// Carried for filesystem ergonomics, not for content sniffing.
    pub ext: String,
}

impl BlobRef {
    fn filename(&self) -> String {
        format!("{}.{}", self.sha, self.ext)
    }
}

// ── Observe outcomes ────────────────────────────────────────────────

/// Typed result of one observe cycle. The recorder projects this into
/// the `observe_finished` event payload's `kind` discriminant.
///
/// Variant set is closed: every observe end-state must map to one of
/// `Ok`, `Error`, or `RateLimited`. A throttled observe is a
/// structural non-success — projecting it as `Ok` collapses two
/// distinct iteration shapes ("healthy observe, intentional wait" vs
/// "throttled observe, all axes starved of input") into one
/// on-disk record, defeating downstream triage.
///
/// `RateLimited::scope` is the upstream-stable bucket token (e.g.
/// `"github/graphql/primary"`); `retry_after_secs` is the requested
/// back-off interval at the moment of detection. Domain-neutral: the
/// strings carry the wire identity, this crate does not parse them.
#[derive(Debug, Clone)]
pub enum ObserveOutcome {
    Ok,
    Error(String),
    RateLimited {
        scope: String,
        retry_after_secs: u64,
    },
}

impl ObserveOutcome {
    /// Single-token rendering for the `observe_finished` payload's
    /// `kind` field. Stable across Rust-side renames.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error(_) => "error",
            Self::RateLimited { .. } => "rate_limited",
        }
    }

    /// Boolean projection: only `Ok` is success. A rate-limit hit
    /// is a structural non-success even though it did not error.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Error-message projection. `RateLimited` materializes as
    /// `"rate_limited:<scope>"` so existing string-keyed consumers
    /// (postmortem, stuck-PR triage) keep matching without growing
    /// a new code path. `Ok` yields `None`.
    #[must_use]
    pub fn error_message(&self) -> Option<String> {
        match self {
            Self::Ok => None,
            Self::Error(e) => Some(e.clone()),
            Self::RateLimited { scope, .. } => Some(format!("rate_limited:{scope}")),
        }
    }
}

// ── Events ───────────────────────────────────────────────────────────

/// One typed event in a run's `events.jsonl`. Variant discriminator
/// is the `kind` field; remaining fields are variant-specific.
///
/// Domain semantics live INSIDE events (the `target` JSON value on
/// `RunStarted`, the `payload` on `DomainSpecific`), never in the
/// path or in the event variant set itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    /// Run started. Always the first event in `events.jsonl`.
    /// `domain` and `target` are opaque to this crate; the
    /// consuming binary defines their shapes.
    RunStarted {
        domain: String,
        target: serde_json::Value,
    },
    /// One observation cycle completed; observations snapshot
    /// stored as a blob.
    IterationObserved { iteration: u32, blob: BlobRef },
    /// One orient cycle completed; oriented snapshot stored as a
    /// blob.
    IterationOriented { iteration: u32, blob: BlobRef },
    /// Decide stage emitted a decision. `decision_kind` is the
    /// domain's stable token for the variant (e.g.
    /// `"Execute"`, `"Halt::HumanNeeded"`).
    IterationDecided {
        iteration: u32,
        decision_kind: String,
    },
    /// A handoff was selected. Prompt body stored as a blob;
    /// `variant` names which handoff flavor (`"HandoffHuman"`,
    /// `"HandoffAgent"`); `action_kind` is the domain's action
    /// name.
    IterationHandoff {
        iteration: u32,
        variant: String,
        action_kind: String,
        blob: BlobRef,
    },
    /// A non-handoff action was executed. Effect details live in
    /// the domain's action vocabulary; this event is the
    /// audit-trail marker. `success` reflects whether the action's
    /// effect completed without error — a failed Full action emits
    /// `success: false` so projection consumers can distinguish
    /// completed iterations from attempted-but-failed ones.
    IterationExecuted {
        iteration: u32,
        action_kind: String,
        success: bool,
    },
    /// A wait was performed. `interval_ms` is the elapsed wall
    /// time of the wait (not the requested duration).
    IterationWaited {
        iteration: u32,
        action_kind: String,
        interval_ms: u64,
    },
    /// Run reached a terminal state via the normal decision path.
    RunHalted { outcome: String, exit_code: i32 },
    /// Stall detector tripped: two consecutive identical actions.
    RunStalled { last_action: String },
    /// Iteration cap reached without halting.
    RunCapReached { last_action: String },
    /// Catch-all for domains that need an event the typed
    /// vocabulary doesn't model. `kind_suffix` is appended for human
    /// triage; `payload` is opaque JSON.
    DomainSpecific {
        kind_suffix: String,
        payload: serde_json::Value,
    },
}

/// One line in `events.jsonl`: timestamp + body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub body: EventBody,
}

impl Event {
    /// Wrap a body with the current UTC wall-clock as its
    /// timestamp.
    #[must_use]
    pub fn now(body: EventBody) -> Self {
        Self {
            ts: Utc::now(),
            body,
        }
    }
}

// ── State-root resolution ────────────────────────────────────────────

/// Resolve the OODA state root via the canonical env chain.
///
/// Precedence:
///
/// 1. `explicit` (e.g. CLI `--state-root PATH`), if `Some`.
/// 2. `$OODA_STATE_HOME`, if set and non-empty.
/// 3. `$XDG_STATE_HOME/ooda`, if `XDG_STATE_HOME` is set and
///    non-empty.
/// 4. `$HOME/.local/state/ooda`, if `HOME` is set and non-empty.
/// 5. `$TMPDIR/ooda` (via [`std::env::temp_dir`]) — the totality
///    fallback.
///
/// Domain-neutral: there is one state root per machine, shared by
/// every OODA agent regardless of domain. Domain identity lives
/// inside event records, not in the state root path.
#[must_use]
pub fn resolve_state_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = env_path("OODA_STATE_HOME") {
        return path;
    }
    if let Some(path) = env_path("XDG_STATE_HOME") {
        return path.join("ooda");
    }
    if let Some(home) = env_path("HOME") {
        return home.join(".local").join("state").join("ooda");
    }
    std::env::temp_dir().join("ooda")
}

/// Read an env-var as a path with normalization. Trims whitespace,
/// treats empty / whitespace-only values as unset, and expands a
/// leading `~/` (or bare `~`) against `$HOME` when present.
///
/// Returns `None` if the var is unset, empty after trim, or has a
/// `~`-expansion request that cannot resolve.
fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    Some(PathBuf::from(trimmed))
}

// ── State root ───────────────────────────────────────────────────────

/// Handle to a state root. Methods create the layout on demand;
/// callers can keep one handle for the lifetime of the process.
#[derive(Debug, Clone)]
pub struct StateRoot {
    root: PathBuf,
}

impl StateRoot {
    /// Open (or create) a state root at `path`. Creates `runs/`
    /// and `live/` if missing.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if the `runs/` or `live/`
    /// subdirectories cannot be created at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let root = path.into();
        fs::create_dir_all(root.join("runs"))?;
        fs::create_dir_all(root.join("live"))?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    fn run_dir(&self, id: &RunId) -> PathBuf {
        self.root.join("runs").join(id.as_str())
    }

    fn live_marker(&self, id: &RunId) -> PathBuf {
        self.root.join("live").join(id.as_str())
    }

    /// Open a writer for a new run. The run's directory and blobs/
    /// subdirectory are created; the live marker is **not** yet
    /// written — call [`RunWriter::start`] with the first
    /// `RunStarted` event to commit the run to the live index.
    ///
    /// Best-effort: orphan `*.tmp` files left over from prior writer
    /// crashes in `blobs/` are swept before returning.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::RunDirExists`] if `runs/<id>/events.jsonl`
    /// already has content (id reuse). Returns [`StateError::Io`] if
    /// `runs/<id>/blobs/` cannot be created.
    pub fn create_run(&self, id: RunId) -> Result<RunWriter> {
        let run_dir = self.run_dir(&id);
        let blobs_dir = run_dir.join("blobs");
        fs::create_dir_all(&blobs_dir)?;
        let events_path = run_dir.join("events.jsonl");
        if let Ok(meta) = fs::metadata(&events_path)
            && meta.len() > 0
        {
            return Err(StateError::RunDirExists(id));
        }
        sweep_blob_tmps(&blobs_dir);
        Ok(RunWriter {
            root: self.clone(),
            id,
            started: false,
            halted: false,
        })
    }

    /// Open a reader for an existing run.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::UnknownRun`] if `runs/<id>/` does
    /// not exist on disk.
    pub fn open_run(&self, id: RunId) -> Result<RunReader> {
        if !self.run_dir(&id).is_dir() {
            return Err(StateError::UnknownRun(id));
        }
        Ok(RunReader {
            root: self.clone(),
            id,
        })
    }

    /// List currently-active run IDs. PIDs encoded into ids by
    /// [`RunId::generate`] are probed via `kill(pid, 0)`; markers
    /// whose writer is no longer alive are filtered out. Use
    /// [`Self::live_runs_unfiltered`] for diagnostic enumeration
    /// that ignores liveness.
    ///
    /// Order is filesystem-dependent; callers should sort if they
    /// need determinism.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `live/` exists but cannot be
    /// enumerated.
    pub fn live_runs(&self) -> Result<Vec<RunId>> {
        let mut out = Vec::new();
        for id in self.live_runs_unfiltered()? {
            match id.writer_pid() {
                Some(pid) if !is_pid_alive(pid) => {}
                _ => out.push(id),
            }
        }
        Ok(out)
    }

    /// List every `live/` marker without PID-liveness filtering.
    /// Use for diagnostics; production readers should prefer
    /// [`Self::live_runs`].
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `live/` exists but cannot be
    /// enumerated.
    pub fn live_runs_unfiltered(&self) -> Result<Vec<RunId>> {
        let mut out = Vec::new();
        let live_dir = self.root.join("live");
        if !live_dir.is_dir() {
            return Ok(out);
        }
        for entry in fs::read_dir(&live_dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(id) = RunId::new(name)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Unlink every `live/<run-id>` marker whose embedded PID is no
    /// longer alive. Markers without a parseable PID suffix are
    /// left in place (conservative — caller may be using a non-
    /// generated id scheme).
    ///
    /// Returns the ids of swept markers for logging.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `live/` cannot be enumerated.
    /// Per-file unlink failures are best-effort (logged-implicit via
    /// the return list excluding them).
    pub fn sweep_dead_markers(&self) -> Result<Vec<RunId>> {
        let mut swept = Vec::new();
        for id in self.live_runs_unfiltered()? {
            let Some(pid) = id.writer_pid() else { continue };
            if is_pid_alive(pid) {
                continue;
            }
            let marker = self.live_marker(&id);
            if fs::remove_file(&marker).is_ok() {
                swept.push(id);
            }
        }
        Ok(swept)
    }
}

/// POSIX `kill(pid, 0)` liveness probe. `Ok` => alive (or alive but
/// not owned by the caller — `EPERM`); `ESRCH` => dead.
///
/// On non-Unix targets this returns `true` (conservative — we can
/// neither probe nor garbage-collect markers; readers see the marker
/// regardless of writer state).
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        // kill(0, 0) addresses every process in the caller's group;
        // never a valid writer pid. Treat as dead.
        return false;
    }
    // SAFETY: `libc::kill` with signal 0 performs an existence check
    // only; no side effects on the target process.
    let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
    let rc = unsafe { libc_kill(pid_i32, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM (process exists, not owned by us) == alive.
    matches!(io::Error::last_os_error().raw_os_error(), Some(1))
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

/// Best-effort sweep of orphan `<sha>.<ext>.tmp` siblings in a
/// blobs directory. Called from [`StateRoot::create_run`].
fn sweep_blob_tmps(blobs_dir: &Path) {
    let Ok(entries) = fs::read_dir(blobs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

// ── RunWriter ────────────────────────────────────────────────────────

/// Append-only writer for one run. Single-threaded by construction:
/// every mutating method takes `&mut self`, ruling out in-process
/// aliasing. Concurrent writes belong on distinct runs.
#[derive(Debug)]
pub struct RunWriter {
    root: StateRoot,
    id: RunId,
    started: bool,
    halted: bool,
}

impl RunWriter {
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.id
    }

    /// True once a terminal event has been appended via
    /// [`Self::halt`]. Subsequent [`Self::append`] / [`Self::halt`]
    /// calls return [`StateError::AlreadyHalted`].
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Append `RunStarted` then commit the run to the live index.
    /// Order matters: the event lands on disk *before* the marker
    /// so a crash mid-`start` leaves a marker-less run dir that
    /// readers ignore (rather than a stuck-live empty marker).
    ///
    /// # Panics
    ///
    /// Panics if `body` is not [`EventBody::RunStarted`]. The
    /// terminal-event contract is a writer-protocol invariant and
    /// must hold in release.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::AlreadyStarted`] if a live marker
    /// already exists for this run id (collision in id generation
    /// or double-`start`). Returns [`StateError::Io`] for other
    /// filesystem failures or [`StateError::Json`] if the event
    /// fails to serialize.
    pub fn start(&mut self, body: EventBody) -> Result<()> {
        assert!(
            matches!(body, EventBody::RunStarted { .. }),
            "RunWriter::start expects EventBody::RunStarted"
        );
        assert!(!self.started, "RunWriter::start called twice");
        assert!(!self.halted, "RunWriter::start after halt");
        // Append the RunStarted event first; on success, claim the
        // marker. If the append fails we never created the marker,
        // so there is nothing to roll back. If the marker creation
        // fails (id collision), the events.jsonl already carries
        // the line — that's tolerable: readers key liveness off the
        // marker, not the file.
        self.append_event(Event::now(body))?;
        let marker = self.root.live_marker(&self.id);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(StateError::AlreadyStarted(self.id.clone()));
            }
            Err(e) => return Err(StateError::Io(e)),
        }
        self.started = true;
        Ok(())
    }

    /// Append one event to `events.jsonl`. Must be preceded by
    /// [`Self::start`] for a `RunStarted` event; later events do
    /// not enforce ordering here (the consuming domain owns the
    /// semantic invariants).
    ///
    /// Each serialized line is capped at [`MAX_EVENT_BYTES`]
    /// (POSIX `PIPE_BUF`). Over-budget events trigger automatic
    /// blob substitution for the variants that carry an
    /// unbounded JSON field (`DomainSpecific.payload`,
    /// `RunStarted.target`); if substitution still leaves the
    /// line over budget, the call returns
    /// [`StateError::EventTooLarge`].
    ///
    /// # Errors
    ///
    /// Returns [`StateError::AlreadyHalted`] if [`Self::halt`] has
    /// already run. Returns [`StateError::Io`] for filesystem
    /// failures, [`StateError::Json`] if the event fails to
    /// serialize, or [`StateError::EventTooLarge`] if the
    /// post-substitution line still exceeds the cap.
    pub fn append(&mut self, body: EventBody) -> Result<()> {
        if self.halted {
            return Err(StateError::AlreadyHalted(self.id.clone()));
        }
        self.append_event(Event::now(body))
    }

    fn append_event(&mut self, mut event: Event) -> Result<()> {
        let mut line = serde_json::to_vec(&event)?;
        if line.len() + 1 > MAX_EVENT_BYTES {
            substitute_overflow_field(self, &mut event)?;
            line = serde_json::to_vec(&event)?;
        }
        line.push(b'\n');
        if line.len() > MAX_EVENT_BYTES {
            return Err(StateError::EventTooLarge {
                kind: event_kind(&event.body),
                size: line.len(),
                cap: MAX_EVENT_BYTES,
            });
        }
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(&line)?;
        Ok(())
    }

    /// Hash + content-address-write a blob. Returns a [`BlobRef`]
    /// suitable for embedding in an event payload. Idempotent: if
    /// the blob already exists (same sha + ext), the existing
    /// file is reused.
    ///
    /// Takes `&mut self` to rule out concurrent in-process callers
    /// racing on the `exists()` → `create_new(tmp)` → `rename`
    /// sequence.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if the temp file cannot be
    /// created/written or the rename into the final path fails.
    pub fn write_blob(&mut self, bytes: &[u8], ext: &str) -> Result<BlobRef> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha = hex::encode(hasher.finalize());
        let blob = BlobRef {
            sha,
            size: bytes.len() as u64,
            ext: ext.to_string(),
        };
        let blob_path = self
            .root
            .run_dir(&self.id)
            .join("blobs")
            .join(blob.filename());
        if blob_path.exists() {
            return Ok(blob);
        }
        // tmp+rename: write to a sibling temp, then atomically
        // rename into place. No fsync — can be tightened if
        // durability across power loss becomes a requirement.
        let tmp = blob_path.with_extension(format!("{ext}.tmp"));
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
        }
        fs::rename(&tmp, &blob_path)?;
        Ok(blob)
    }

    /// Append a terminal event and remove the live marker. After
    /// this returns, the run is no longer in the live index and
    /// the writer is halted: subsequent [`Self::append`] /
    /// [`Self::halt`] return [`StateError::AlreadyHalted`].
    ///
    /// # Panics
    ///
    /// Panics if `body` is not one of the terminal variants
    /// ([`EventBody::RunHalted`], [`EventBody::RunStalled`],
    /// [`EventBody::RunCapReached`]).
    ///
    /// Append-first ordering: the terminal event must land on disk
    /// before the live marker is cleared. The reader-side invariant
    /// is "absent live marker ⇒ terminal event in the log"; clearing
    /// the marker after a failed append would break it and readers
    /// could no longer distinguish a deliberate halt from SIGKILL.
    ///
    /// On append failure: the writer is NOT marked halted and the
    /// marker is NOT touched. The error is returned to the caller
    /// and [`Drop`] later observes `halted == false` and emits the
    /// `DroppedWithoutHalt` fallback (which clears the marker).
    ///
    /// # Errors
    ///
    /// Returns [`StateError::AlreadyHalted`] if `halt` has already
    /// run. Returns [`StateError::Io`] for filesystem failures
    /// other than a missing marker (which is silently tolerated
    /// for idempotency). Returns [`StateError::Json`] if the
    /// event fails to serialize, or [`StateError::EventTooLarge`]
    /// if the terminal event exceeds the per-line cap.
    pub fn halt(&mut self, body: EventBody) -> Result<()> {
        assert!(
            matches!(
                body,
                EventBody::RunHalted { .. }
                    | EventBody::RunStalled { .. }
                    | EventBody::RunCapReached { .. }
            ),
            "RunWriter::halt expects a terminal event variant"
        );
        if self.halted {
            return Err(StateError::AlreadyHalted(self.id.clone()));
        }
        // Append first. If this fails, leave `halted` false and the
        // marker untouched so Drop's `DroppedWithoutHalt` fallback
        // still fires and the reader-side invariant
        // "absent live marker ⇒ terminal event in the log" holds.
        self.append_event(Event::now(body))?;
        self.halted = true;
        let marker = self.root.live_marker(&self.id);
        match fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StateError::Io(e)),
        }
    }
}

// ── Event overflow helpers ───────────────────────────────────────────

/// Stable token for the event variant; used in error reporting and
/// in the overflow-payload sidecar so the reader can pivot from a
/// shrunk event back to its original kind.
fn event_kind(body: &EventBody) -> &'static str {
    match body {
        EventBody::RunStarted { .. } => "run_started",
        EventBody::IterationObserved { .. } => "iteration_observed",
        EventBody::IterationOriented { .. } => "iteration_oriented",
        EventBody::IterationDecided { .. } => "iteration_decided",
        EventBody::IterationHandoff { .. } => "iteration_handoff",
        EventBody::IterationExecuted { .. } => "iteration_executed",
        EventBody::IterationWaited { .. } => "iteration_waited",
        EventBody::RunHalted { .. } => "run_halted",
        EventBody::RunStalled { .. } => "run_stalled",
        EventBody::RunCapReached { .. } => "run_cap_reached",
        EventBody::DomainSpecific { .. } => "domain_specific",
    }
}

/// Try to fit an oversized event under [`MAX_EVENT_BYTES`] by
/// spilling the unbounded JSON field (`DomainSpecific.payload`,
/// `RunStarted.target`) into a content-addressed blob and
/// replacing the inline value with a small overflow stub
/// `{ "blob": <BlobRef>, "overflow": true }`.
///
/// Variants without an unbounded JSON field are left untouched;
/// the caller's subsequent size check will surface
/// [`StateError::EventTooLarge`] for those.
fn substitute_overflow_field(writer: &mut RunWriter, event: &mut Event) -> Result<()> {
    match &mut event.body {
        EventBody::DomainSpecific { payload, .. } => {
            let blob = spill_value_to_blob(writer, payload)?;
            *payload = serde_json::json!({ "blob": blob, "overflow": true });
        }
        EventBody::RunStarted { target, .. } => {
            let blob = spill_value_to_blob(writer, target)?;
            *target = serde_json::json!({ "blob": blob, "overflow": true });
        }
        _ => {}
    }
    Ok(())
}

fn spill_value_to_blob(writer: &mut RunWriter, value: &serde_json::Value) -> Result<BlobRef> {
    let bytes = serde_json::to_vec(value)?;
    writer.write_blob(&bytes, "json")
}

impl Drop for RunWriter {
    /// Best-effort release of the live marker on unwind / scope-exit
    /// without an explicit [`Self::halt`]. Covers SIGINT trapped to
    /// `process::exit` paths and panic unwinds. Hard kills (SIGKILL,
    /// OOM) skip Drop; readers reconcile those via PID-liveness in
    /// [`StateRoot::live_runs`].
    ///
    /// Append-first ordering: the synthetic `DroppedWithoutHalt`
    /// terminal event must land on disk before the marker is
    /// cleared. The reader-side invariant is "absent live marker ⇒
    /// terminal event in the log"; an append that failed before the
    /// marker was cleared would break it.
    ///
    /// # Worst case: catastrophic append failure at drop-time
    ///
    /// If the synthetic-event append fails (disk full, fs read-only,
    /// run dir unlinked), Drop cannot return an error. The fallback
    /// discipline is:
    ///
    /// 1. Emit a clearly-tagged warning to stderr so a human running
    ///    interactively sees the corruption.
    /// 2. Leave the live marker in place so readers observe "marker
    ///    present + no terminal event" — the same shape a SIGKILL'd
    ///    writer produces. PID-liveness sweep then reclaims the
    ///    marker once the writer process exits.
    ///
    /// Net result: the on-disk run is indistinguishable from a hard
    /// kill, which is the strongest signal we can give without a
    /// return channel.
    fn drop(&mut self) {
        if !self.started || self.halted {
            return;
        }
        // Best-effort terminal event so readers can distinguish
        // "writer crashed cleanly" from "writer SIGKILLed mid-run".
        // Append first; clear the marker only if the append landed.
        match self.append_event(Event::now(EventBody::RunHalted {
            outcome: "DroppedWithoutHalt".to_string(),
            exit_code: -1,
        })) {
            Ok(()) => {
                let marker = self.root.live_marker(&self.id);
                let _ = fs::remove_file(&marker);
            }
            Err(e) => {
                // Append failed at drop-time (disk full, fs
                // read-only, run dir unlinked). Surface to stderr
                // and leave the marker in place so readers see the
                // same shape as a SIGKILL'd writer.
                eprintln!(
                    "ooda-state: run {} dropped without halt and synthetic terminal event failed: {e}",
                    self.id.as_str()
                );
            }
        }
    }
}

// ── RunReader ────────────────────────────────────────────────────────

/// Read-only handle to one run's on-disk state.
#[derive(Debug)]
pub struct RunReader {
    root: StateRoot,
    id: RunId,
}

impl RunReader {
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.id
    }

    /// Parse the entire `events.jsonl` into a `Vec<Event>`. Lenient:
    /// malformed lines (forward-compat unknown variants, mid-write
    /// partial lines) are skipped and the parse error is dropped.
    /// Callers needing strict semantics should use
    /// [`Self::events_strict`].
    ///
    /// A trailing line not terminated by `\n` is treated as
    /// "writer mid-flight" and skipped — the writer is racing the
    /// reader and the next call will see the completed line.
    /// Corruption is `complete-line-but-bad-JSON` only.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if the file exists but cannot
    /// be read.
    pub fn events(&self) -> Result<Vec<Event>> {
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path)?;
        let mut out = Vec::new();
        for line in complete_lines(&bytes) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(trimmed) {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// Strict variant of [`Self::events`]: returns the first parse
    /// error encountered. Use only when partial lines and forward-
    /// compat skew are known to be impossible.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] for file-read failures or
    /// [`StateError::Json`] on the first malformed line.
    pub fn events_strict(&self) -> Result<Vec<Event>> {
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path)?;
        let mut out = Vec::new();
        for line in complete_lines(&bytes) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.push(serde_json::from_str(trimmed)?);
        }
        Ok(out)
    }

    /// Iterator over events as they're parsed. Lenient: per-line
    /// parse errors are skipped (yielding only the valid events).
    /// A trailing line not terminated by `\n` is treated as
    /// "writer mid-flight" and skipped silently. Use
    /// [`Self::events_stream_strict`] when strict per-line
    /// reporting is required.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `events.jsonl` exists but
    /// cannot be opened. A missing file is treated as an empty
    /// iterator (no error).
    pub fn events_stream(&self) -> Result<EventsIter> {
        use std::io::BufRead;
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(EventsIter {
                    inner: Box::new(std::iter::empty()),
                });
            }
            Err(e) => return Err(StateError::Io(e)),
        };
        let reader = io::BufReader::new(file);
        let inner = reader.lines().filter_map(|l| match l {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => serde_json::from_str::<Event>(&s).ok().map(Ok),
            Err(_) => None,
        });
        Ok(EventsIter {
            inner: Box::new(inner),
        })
    }

    /// Strict streaming variant of [`Self::events_stream`]. Yields
    /// `Err(_)` on per-line parse failure, letting a strict caller
    /// distinguish malformed lines from forward-compat unknowns.
    ///
    /// # Errors
    ///
    /// See [`Self::events_stream`] for opening errors. Per-line
    /// parse failures surface as `Some(Err(_))` items in the
    /// returned iterator.
    pub fn events_stream_strict(&self) -> Result<EventsIter> {
        use std::io::BufRead;
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(EventsIter {
                    inner: Box::new(std::iter::empty()),
                });
            }
            Err(e) => return Err(StateError::Io(e)),
        };
        let mut reader = io::BufReader::new(file);
        // Consume the file via `read_until(b'\n', ..)` so the
        // trailing partial line (no final `\n`) can be detected
        // and dropped rather than mis-parsed.
        let mut completed: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut buf = Vec::new();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                        completed.push(buf);
                    } else {
                        // Writer mid-flight: tail bytes without a
                        // terminating newline are dropped on this
                        // pass; the next reader will see them
                        // completed.
                        break;
                    }
                }
                Err(e) => return Err(StateError::Io(e)),
            }
        }
        let inner = completed
            .into_iter()
            .filter_map(|raw| match std::str::from_utf8(&raw) {
                Ok(s) if s.trim().is_empty() => None,
                Ok(s) => Some(serde_json::from_str::<Event>(s).map_err(StateError::from)),
                Err(e) => Some(Err(StateError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    e,
                )))),
            });
        Ok(EventsIter {
            inner: Box::new(inner),
        })
    }

    /// Read a blob's bytes into memory. Verifies the on-disk hash
    /// matches the reference; mismatch is a corruption signal.
    ///
    /// The allocation is bounded by [`MAX_INLINE_BLOB_SIZE`] (64
    /// MiB) cross-checked against the file's actual length on
    /// disk. Larger blobs surface as [`StateError::BlobTooLarge`];
    /// the caller must use [`Self::read_blob_stream`] to consume
    /// them incrementally.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::MissingBlob`] if the referenced
    /// blob is not on disk; [`StateError::BlobTooLarge`] if its
    /// on-disk size exceeds the inline cap;
    /// [`StateError::Io`] if the file exists but cannot be read;
    /// [`StateError::BlobHashMismatch`] if the on-disk content
    /// does not hash to the expected sha.
    pub fn read_blob(&self, blob: &BlobRef) -> Result<Vec<u8>> {
        let blob_path = self
            .root
            .run_dir(&self.id)
            .join("blobs")
            .join(blob.filename());
        let meta = match fs::metadata(&blob_path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StateError::MissingBlob {
                    run_id: self.id.clone(),
                    blob: blob.clone(),
                });
            }
            Err(e) => return Err(StateError::Io(e)),
        };
        let on_disk = meta.len();
        if on_disk > MAX_INLINE_BLOB_SIZE {
            return Err(StateError::BlobTooLarge {
                run_id: self.id.clone(),
                size: on_disk,
                limit: MAX_INLINE_BLOB_SIZE,
            });
        }
        // Trust the file length over the recorded size — a
        // corrupted BlobRef::size could otherwise drive an OOM
        // allocation. Cap defensively at MAX_INLINE_BLOB_SIZE.
        let cap = usize::try_from(on_disk).unwrap_or(0);
        let mut buf = Vec::with_capacity(cap);
        File::open(&blob_path)?.read_to_end(&mut buf)?;
        let mut hasher = Sha256::new();
        hasher.update(&buf);
        let actual = hex::encode(hasher.finalize());
        if actual != blob.sha {
            return Err(StateError::BlobHashMismatch {
                run_id: self.id.clone(),
                expected: blob.sha.clone(),
                actual,
            });
        }
        Ok(buf)
    }

    /// Open a hash-verifying streaming reader for a blob. Reads
    /// bytes incrementally and accumulates the SHA-256; the caller
    /// must invoke [`HashVerifyingReader::verify`] after consuming
    /// the stream to confirm content integrity.
    ///
    /// Use for blobs that may exceed [`MAX_INLINE_BLOB_SIZE`]
    /// (large LLM transcripts, captured stdout).
    ///
    /// # Errors
    ///
    /// Returns [`StateError::MissingBlob`] if the referenced blob
    /// is not on disk; [`StateError::Io`] if the file cannot be
    /// opened.
    pub fn read_blob_stream(&self, blob: &BlobRef) -> Result<HashVerifyingReader> {
        let blob_path = self
            .root
            .run_dir(&self.id)
            .join("blobs")
            .join(blob.filename());
        if !blob_path.exists() {
            return Err(StateError::MissingBlob {
                run_id: self.id.clone(),
                blob: blob.clone(),
            });
        }
        let file = File::open(&blob_path)?;
        Ok(HashVerifyingReader {
            inner: io::BufReader::new(file),
            hasher: Sha256::new(),
            expected: blob.sha.clone(),
            run_id: self.id.clone(),
        })
    }
}

/// Streaming reader that updates a SHA-256 hasher over every byte
/// consumed via its `Read` impl. Callers must invoke
/// [`Self::verify`] after the stream is fully drained to confirm
/// the on-disk content matches the expected hash.
pub struct HashVerifyingReader {
    inner: io::BufReader<File>,
    hasher: Sha256,
    expected: String,
    run_id: RunId,
}

impl HashVerifyingReader {
    /// Finalize the hash and compare to the expected SHA-256.
    /// Call after the stream has been fully consumed.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::BlobHashMismatch`] if the accumulated
    /// hash differs from the expected sha.
    pub fn verify(self) -> Result<()> {
        let actual = hex::encode(self.hasher.finalize());
        if actual == self.expected {
            Ok(())
        } else {
            Err(StateError::BlobHashMismatch {
                run_id: self.run_id,
                expected: self.expected,
                actual,
            })
        }
    }
}

impl Read for HashVerifyingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Split a byte buffer into the slices preceding each `\n`. A
/// trailing run of bytes without a terminator is dropped (the
/// writer is mid-flight; the next read will see the completed
/// line). Empty lines are kept as zero-length slices and filtered
/// at the parse step.
fn complete_lines(bytes: &[u8]) -> impl Iterator<Item = &str> {
    // Trim the trailing partial line (no final `\n`) so every
    // yielded slice corresponds to a kernel-atomic complete write.
    let terminated = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(idx) => &bytes[..=idx],
        None => &[],
    };
    terminated
        .split(|b| *b == b'\n')
        // The final element of `split` on a terminated buffer is
        // an empty slice after the last `\n`; skip it.
        .filter(|s| !s.is_empty())
        .filter_map(|slice| std::str::from_utf8(slice).ok())
}

/// Boxed iterator wrapper so the concrete inner type doesn't leak
/// into the public API.
pub struct EventsIter {
    inner: Box<dyn Iterator<Item = Result<Event>>>,
}

impl Iterator for EventsIter {
    type Item = Result<Event>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, StateRoot) {
        let tmp = TempDir::new().unwrap();
        let root = StateRoot::new(tmp.path()).unwrap();
        (tmp, root)
    }

    #[test]
    fn run_lifecycle_writes_events_and_toggles_live_marker() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();

        // Live marker is absent before start
        assert!(!root.live_runs().unwrap().contains(&id));

        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::json!({ "k": "v" }),
            })
            .unwrap();

        // Live marker is present after start
        assert!(root.live_runs().unwrap().contains(&id));

        writer
            .halt(EventBody::RunHalted {
                outcome: "DoneMerged".into(),
                exit_code: 0,
            })
            .unwrap();

        // Live marker is gone after halt
        assert!(!root.live_runs().unwrap().contains(&id));

        // events.jsonl has 2 lines
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].body, EventBody::RunStarted { .. }));
        assert!(matches!(events[1].body, EventBody::RunHalted { .. }));
    }

    #[test]
    fn double_start_on_same_writer_panics() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut a = root.create_run(id).unwrap();
        a.start(EventBody::RunStarted {
            domain: "test".into(),
            target: serde_json::Value::Null,
        })
        .unwrap();

        // Second start on the same writer is a writer-protocol bug.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            a.start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
        }))
        .is_err();
        assert!(panicked);
    }

    #[test]
    fn create_run_on_started_id_returns_run_dir_exists() {
        // Second create_run on a populated dir is rejected by
        // RunDirExists; this is the cross-process collision path.
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut a = root.create_run(id.clone()).unwrap();
        a.start(EventBody::RunStarted {
            domain: "test".into(),
            target: serde_json::Value::Null,
        })
        .unwrap();
        let err = root.create_run(id).unwrap_err();
        assert!(matches!(err, StateError::RunDirExists(_)));
    }

    #[test]
    fn blob_dedup_skips_second_write() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();

        let body = b"identical content";
        let a = writer.write_blob(body, "md").unwrap();
        let b = writer.write_blob(body, "md").unwrap();
        assert_eq!(a, b);

        // Only one blob file on disk
        let blobs_dir = root.path().join("runs").join(id.as_str()).join("blobs");
        let count = fs::read_dir(&blobs_dir).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn blob_read_back_verifies_hash() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let body = b"hello, world";
        let blob = writer.write_blob(body, "txt").unwrap();
        let reader = root.open_run(id).unwrap();
        let back = reader.read_blob(&blob).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn blob_read_detects_tampering() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let blob = writer.write_blob(b"original", "txt").unwrap();
        // Overwrite the file with different bytes.
        let path = root
            .path()
            .join("runs")
            .join(id.as_str())
            .join("blobs")
            .join(blob.filename());
        fs::write(&path, b"tampered").unwrap();

        let reader = root.open_run(id.clone()).unwrap();
        let err = reader.read_blob(&blob).unwrap_err();
        assert!(matches!(err, StateError::BlobHashMismatch { .. }));
    }

    #[test]
    fn concurrent_runs_do_not_interfere() {
        // Two writers on distinct run-ids should both succeed
        // without coordination.
        let (_tmp, root) = fresh();
        let id_a = RunId::generate();
        // Small sleep so generate() yields a distinct id (it's
        // timestamp + subsec nanos + pid; same-process same-nanos
        // is possible).
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_b = RunId::generate();
        assert_ne!(id_a, id_b);

        let mut a = root.create_run(id_a.clone()).unwrap();
        let mut b = root.create_run(id_b.clone()).unwrap();
        a.start(EventBody::RunStarted {
            domain: "test".into(),
            target: serde_json::Value::Null,
        })
        .unwrap();
        b.start(EventBody::RunStarted {
            domain: "test".into(),
            target: serde_json::Value::Null,
        })
        .unwrap();

        let live: Vec<_> = root.live_runs().unwrap();
        assert!(live.contains(&id_a));
        assert!(live.contains(&id_b));
    }

    #[test]
    fn run_id_rejects_path_traversal() {
        assert!(RunId::new("..").is_err());
        assert!(RunId::new("a/b").is_err());
        assert!(RunId::new(".hidden").is_err());
        assert!(RunId::new("").is_err());
        assert!(RunId::new("normal-id").is_ok());
    }

    #[test]
    fn oversize_domain_specific_payload_is_spilled_to_blob() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        // 8 KiB inline string blows past PIPE_BUF (4 KiB cap).
        let big = "x".repeat(8 * 1024);
        writer
            .append(EventBody::DomainSpecific {
                kind_suffix: "stress".into(),
                payload: serde_json::json!({ "data": big }),
            })
            .unwrap();
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 2);
        let EventBody::DomainSpecific { payload, .. } = &events[1].body else {
            panic!("expected DomainSpecific, got {:?}", events[1].body);
        };
        assert_eq!(payload.get("overflow"), Some(&serde_json::json!(true)));
        let blob_ref: BlobRef =
            serde_json::from_value(payload.get("blob").cloned().unwrap()).unwrap();
        let bytes = reader.read_blob(&blob_ref).unwrap();
        let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            original.get("data").and_then(|v| v.as_str()),
            Some(big.as_str())
        );
    }

    #[test]
    fn oversize_run_started_target_is_spilled_to_blob() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let big = "y".repeat(8 * 1024);
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::json!({ "blob_of_text": big }),
            })
            .unwrap();
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 1);
        let EventBody::RunStarted { target, .. } = &events[0].body else {
            panic!("expected RunStarted");
        };
        assert_eq!(target.get("overflow"), Some(&serde_json::json!(true)));
        assert!(target.get("blob").is_some());
    }

    #[test]
    fn oversize_event_without_spillable_field_returns_event_too_large() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        // IterationDecided.decision_kind is structurally bounded
        // by domain vocabulary; stuffing an oversize value here is
        // a caller bug. The writer surfaces it as EventTooLarge.
        let big = "Z".repeat(8 * 1024);
        let err = writer
            .append(EventBody::IterationDecided {
                iteration: 1,
                decision_kind: big,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::EventTooLarge { .. }), "{err:?}");
    }

    #[test]
    fn run_id_rejects_control_bytes_and_whitespace() {
        assert!(RunId::new("a\nb").is_err());
        assert!(RunId::new("a\rb").is_err());
        assert!(RunId::new("a\tb").is_err());
        assert!(RunId::new("a\0b").is_err());
        assert!(RunId::new(" leading").is_err());
        assert!(RunId::new("trailing ").is_err());
        assert!(RunId::new("   ").is_err());
    }

    #[test]
    fn run_id_writer_pid_parses_generated_ids() {
        let id = RunId::generate();
        let pid = id.writer_pid().expect("generated id has pid suffix");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn run_id_writer_pid_returns_none_for_caller_supplied() {
        let id = RunId::new("custom-id").unwrap();
        assert!(id.writer_pid().is_none());
    }

    #[test]
    fn post_halt_append_returns_already_halted() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        writer
            .halt(EventBody::RunHalted {
                outcome: "Done".into(),
                exit_code: 0,
            })
            .unwrap();
        let err = writer
            .append(EventBody::DomainSpecific {
                kind_suffix: "after_halt".into(),
                payload: serde_json::Value::Null,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::AlreadyHalted(_)));
    }

    #[test]
    fn double_halt_returns_already_halted() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        writer
            .halt(EventBody::RunHalted {
                outcome: "Done".into(),
                exit_code: 0,
            })
            .unwrap();
        let err = writer
            .halt(EventBody::RunHalted {
                outcome: "Done".into(),
                exit_code: 0,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::AlreadyHalted(_)));
    }

    #[test]
    fn create_run_rejects_populated_events_file() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        // Drop the writer (Drop releases marker + appends synthetic
        // halt). Now reopen the same id: events.jsonl has content.
        drop(writer);
        let err = root.create_run(id).unwrap_err();
        assert!(matches!(err, StateError::RunDirExists(_)));
    }

    #[test]
    fn events_tolerates_trailing_partial_line() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        // Append a partial fragment by hand to simulate a
        // mid-flight writer (one event terminated, one not).
        let events_path = root
            .path()
            .join("runs")
            .join(id.as_str())
            .join("events.jsonl");
        let mut f = OpenOptions::new().append(true).open(&events_path).unwrap();
        f.write_all(b"{\"ts\":\"2026-05-17T00:00:00Z\",\"kind\":\"iteration_decided\",\"iteration\":2,\"decision_kind\":\"Exe").unwrap();
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 1, "partial trailing line must be dropped");
        let streamed: Vec<Event> = reader
            .events_stream()
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(
            streamed.len(),
            1,
            "events_stream must drop trailing partial too",
        );
    }

    #[test]
    fn halt_preserves_marker_when_append_fails_so_drop_can_fallback() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        assert!(root.live_runs().unwrap().contains(&id));
        // Force an oversized terminal event so the append step
        // fails. Reader-side invariant: absent live marker ⇒
        // terminal event in the log. So a failed append must leave
        // the marker present; Drop's fallback later emits the
        // synthetic `DroppedWithoutHalt` terminal event and clears
        // the marker, preserving the invariant end-to-end.
        let big_outcome = "B".repeat(8 * 1024);
        let err = writer
            .halt(EventBody::RunHalted {
                outcome: big_outcome,
                exit_code: 1,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::EventTooLarge { .. }), "{err:?}");
        assert!(
            root.live_runs().unwrap().contains(&id),
            "marker must be preserved when append fails so Drop's \
             DroppedWithoutHalt fallback can reconcile the log",
        );
        // After Drop runs, marker is gone and a synthetic
        // RunHalted terminal event is on disk.
        drop(writer);
        assert!(!root.live_runs_unfiltered().unwrap().contains(&id));
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        let last = events.last().expect("at least one event");
        match &last.body {
            EventBody::RunHalted { outcome, .. } => {
                assert_eq!(outcome, "DroppedWithoutHalt");
            }
            other => panic!("expected synthetic RunHalted, got {other:?}"),
        }
    }

    #[test]
    fn drop_with_failed_append_leaves_marker_present() {
        // Catastrophic case: at drop-time the synthetic
        // `DroppedWithoutHalt` event cannot be written (disk full,
        // fs read-only, run dir unlinked). Drop cannot return an
        // error, so the discipline is: surface to stderr and leave
        // the marker present so readers see "marker + no terminal
        // event" — indistinguishable from a SIGKILL'd writer.
        // PID-liveness sweep reclaims the marker once the writer
        // process exits.
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        assert!(root.live_runs_unfiltered().unwrap().contains(&id));
        // Force the synthetic append to fail by unlinking the run
        // directory between start and drop. The next
        // `OpenOptions::new().create(true).append(true).open(&path)`
        // cannot create events.jsonl because its parent is gone.
        let run_dir = root.path().join("runs").join(id.as_str());
        fs::remove_dir_all(&run_dir).unwrap();
        drop(writer);
        // Marker is still in place because Drop's append failed.
        assert!(
            root.live_runs_unfiltered().unwrap().contains(&id),
            "marker must remain when Drop's synthetic-event append \
             fails so readers cannot mistake the run for a clean halt",
        );
    }

    #[test]
    fn drop_without_halt_releases_marker() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        assert!(root.live_runs_unfiltered().unwrap().contains(&id));
        drop(writer);
        assert!(!root.live_runs_unfiltered().unwrap().contains(&id));
    }

    #[test]
    fn live_runs_filters_dead_pid_markers() {
        let (_tmp, root) = fresh();
        // Hand-craft a marker for a never-existing PID.
        let id = RunId::new("20260101T000000Z-000000000-p1").unwrap();
        std::fs::write(root.path().join("live").join(id.as_str()), b"").unwrap();
        assert!(root.live_runs_unfiltered().unwrap().contains(&id));
        // PID 1 is init — alive on every running system. Use a
        // sentinel "unlikely to exist" PID instead.
        let dead_id = RunId::new("20260101T000000Z-000000001-p4294967294").unwrap();
        std::fs::write(root.path().join("live").join(dead_id.as_str()), b"").unwrap();
        let live = root.live_runs().unwrap();
        assert!(!live.contains(&dead_id), "dead pid should be filtered");
    }

    #[test]
    fn sweep_dead_markers_unlinks_filtered_entries() {
        let (_tmp, root) = fresh();
        let dead_id = RunId::new("20260101T000000Z-000000002-p4294967294").unwrap();
        std::fs::write(root.path().join("live").join(dead_id.as_str()), b"").unwrap();
        let swept = root.sweep_dead_markers().unwrap();
        assert!(swept.contains(&dead_id));
        assert!(!root.path().join("live").join(dead_id.as_str()).exists());
    }

    #[test]
    fn read_blob_rejects_oversize() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let blob = writer.write_blob(b"x", "txt").unwrap();
        // Overwrite the file with content over the inline cap.
        let path = root
            .path()
            .join("runs")
            .join(id.as_str())
            .join("blobs")
            .join(blob.filename());
        let big = vec![0u8; usize::try_from(MAX_INLINE_BLOB_SIZE + 1).unwrap()];
        std::fs::write(&path, &big).unwrap();
        let reader = root.open_run(id).unwrap();
        let err = reader.read_blob(&blob).unwrap_err();
        assert!(matches!(err, StateError::BlobTooLarge { .. }));
    }

    #[test]
    fn read_blob_stream_verifies_hash() {
        use std::io::Read as _;
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let blob = writer.write_blob(b"streaming bytes", "txt").unwrap();
        let reader = root.open_run(id).unwrap();
        let mut stream = reader.read_blob_stream(&blob).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"streaming bytes");
        stream.verify().unwrap();
    }

    #[test]
    fn read_blob_stream_detects_tampering() {
        use std::io::Read as _;
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        let blob = writer.write_blob(b"original", "txt").unwrap();
        let path = root
            .path()
            .join("runs")
            .join(id.as_str())
            .join("blobs")
            .join(blob.filename());
        std::fs::write(&path, b"tampered").unwrap();
        let reader = root.open_run(id).unwrap();
        let mut stream = reader.read_blob_stream(&blob).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let err = stream.verify().unwrap_err();
        assert!(matches!(err, StateError::BlobHashMismatch { .. }));
    }

    #[test]
    fn events_skips_malformed_lines() {
        use std::io::Write as _;

        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        // Inject a malformed line directly into events.jsonl.
        let path = root
            .path()
            .join("runs")
            .join(id.as_str())
            .join("events.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"not json\n").unwrap();
        f.write_all(
            br#"{"ts":"2026-05-17T00:00:00Z","kind":"iteration_decided","iteration":1,"decision_kind":"Execute"}
"#,
        )
        .unwrap();
        drop(f);
        let reader = root.open_run(id).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 2, "skipped malformed, kept 2 valid");
        let err = reader.events_strict().unwrap_err();
        assert!(matches!(err, StateError::Json(_)));
    }

    #[test]
    fn env_path_trims_and_expands_tilde() {
        // Use scoped env vars to avoid cross-test pollution.
        // SAFETY: tests run single-threaded under default `cargo test`?
        // No — they run in parallel by default. The env mutations
        // here race with other tests reading the same vars. Mitigate
        // by using a process-unique var name so this test is the
        // only reader.
        let var = format!("OODA_TEST_PATH_{}", std::process::id());
        // SAFETY: env mutation in test; var name uniquified per pid.
        unsafe { std::env::set_var(&var, "   /tmp/foo   ") };
        assert_eq!(env_path(&var), Some(PathBuf::from("/tmp/foo")));
        // SAFETY: see above.
        unsafe { std::env::set_var(&var, "   ") };
        assert_eq!(env_path(&var), None);
        // SAFETY: see above.
        unsafe { std::env::set_var(&var, "~/scratch") };
        if let Some(home) = std::env::var_os("HOME") {
            let expected = PathBuf::from(home).join("scratch");
            assert_eq!(env_path(&var), Some(expected));
        }
        // SAFETY: see above.
        unsafe { std::env::remove_var(&var) };
    }

    #[test]
    fn create_run_sweeps_blob_tmps() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        // Pre-populate a stale tmp.
        let blobs_dir = root.path().join("runs").join(id.as_str()).join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join("abc.md.tmp"), b"stale").unwrap();
        let _writer = root.create_run(id).unwrap();
        assert!(!blobs_dir.join("abc.md.tmp").exists());
    }

    #[test]
    fn events_stream_yields_parsed_events() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        writer
            .append(EventBody::IterationDecided {
                iteration: 1,
                decision_kind: "Execute".into(),
            })
            .unwrap();
        let reader = root.open_run(id).unwrap();
        let events: Vec<Event> = reader
            .events_stream()
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(events.len(), 2);
    }
}
