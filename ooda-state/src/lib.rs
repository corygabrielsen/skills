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
//! No locks; no shared mutable state between concurrent runs.

#![doc(html_root_url = "https://docs.rs/ooda-state/0.1.0")]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
}

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
    /// for global uniqueness; no validation here beyond rejecting
    /// strings that would escape the runs/ directory (`..`, `/`,
    /// leading `.`).
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] with [`io::ErrorKind::InvalidInput`]
    /// if `s` is empty, contains path separators, or would resolve
    /// to a hidden or parent-directory entry.
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.is_empty()
            || s.contains('/')
            || s.contains('\\')
            || s == "."
            || s == ".."
            || s.starts_with('.')
        {
            return Err(StateError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid run id: {s:?}"),
            )));
        }
        Ok(Self(s))
    }

    /// Generate a fresh run id from current UTC timestamp,
    /// subsecond-nanosecond entropy, and pid. Matches the
    /// `<YYYYMMDDTHHMMSSZ>-<entropy>-p<pid>` pattern v1 uses
    /// for run dirs.
    #[must_use]
    pub fn generate() -> Self {
        let now = Utc::now();
        let ts = now.format("%Y%m%dT%H%M%SZ");
        // Entropy via system clock subsecond nanos; for v2's purposes
        // (run-id local uniqueness within one machine within one
        // second) this is sufficient. A future revision could lift
        // to a UUID v7 or pull from /dev/urandom.
        let entropy = now.timestamp_subsec_nanos();
        let pid = std::process::id();
        Self(format!("{ts}-{entropy:09}-p{pid}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
    /// audit-trail marker.
    IterationExecuted { iteration: u32, action_kind: String },
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
    /// Catch-all for domains that need an event the v2 vocabulary
    /// doesn't yet model. `kind_suffix` is appended for human
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
    if let Some(path) = nonempty_env_path("OODA_STATE_HOME") {
        return path;
    }
    if let Some(path) = nonempty_env_path("XDG_STATE_HOME") {
        return path.join("ooda");
    }
    if let Some(home) = nonempty_env_path("HOME") {
        return home.join(".local").join("state").join("ooda");
    }
    std::env::temp_dir().join("ooda")
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

// ── State root ───────────────────────────────────────────────────────

/// Handle to a v2 state root. Methods create the layout on demand;
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
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `runs/<id>/blobs/` cannot be
    /// created.
    pub fn create_run(&self, id: RunId) -> Result<RunWriter> {
        let run_dir = self.run_dir(&id);
        fs::create_dir_all(run_dir.join("blobs"))?;
        Ok(RunWriter {
            root: self.clone(),
            id,
            started: false,
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

    /// List currently-active run IDs (presence of `live/<run-id>`
    /// marker). Order is filesystem-dependent; callers should sort
    /// if they need determinism.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `live/` exists but cannot be
    /// enumerated.
    pub fn live_runs(&self) -> Result<Vec<RunId>> {
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
}

// ── RunWriter ────────────────────────────────────────────────────────

/// Append-only writer for one run. Single-threaded by construction;
/// callers wanting concurrent writes should open distinct runs.
#[derive(Debug)]
pub struct RunWriter {
    root: StateRoot,
    id: RunId,
    started: bool,
}

impl RunWriter {
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.id
    }

    /// Commit the run to the live index and append the first
    /// event (must be [`EventBody::RunStarted`]).
    ///
    /// # Errors
    ///
    /// Returns [`StateError::AlreadyStarted`] if a live marker
    /// already exists for this run id (collision in id generation
    /// or double-`start`). Returns [`StateError::Io`] for other
    /// filesystem failures or [`StateError::Json`] if the event
    /// fails to serialize.
    pub fn start(&mut self, body: EventBody) -> Result<()> {
        debug_assert!(
            matches!(body, EventBody::RunStarted { .. }),
            "RunWriter::start expects EventBody::RunStarted"
        );
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
        self.append_event(&Event::now(body))
    }

    /// Append one event to `events.jsonl`. Must be preceded by
    /// [`Self::start`] for a `RunStarted` event; later events do
    /// not enforce ordering here (the consuming domain owns the
    /// semantic invariants).
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] for filesystem failures or
    /// [`StateError::Json`] if the event fails to serialize.
    pub fn append(&mut self, body: EventBody) -> Result<()> {
        self.append_event(&Event::now(body))
    }

    fn append_event(&mut self, event: &Event) -> Result<()> {
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
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
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if the temp file cannot be
    /// created/written or the rename into the final path fails.
    pub fn write_blob(&self, bytes: &[u8], ext: &str) -> Result<BlobRef> {
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
        // rename into place. Skipping fsync here for v1; can be
        // tightened if durability matters across power loss.
        let tmp = blob_path.with_extension(format!("{ext}.tmp"));
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
        }
        fs::rename(&tmp, &blob_path)?;
        Ok(blob)
    }

    /// Append a terminal event and remove the live marker. After
    /// this returns, the run is no longer in the live index.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] for filesystem failures other
    /// than a missing marker (which is silently tolerated for
    /// idempotency). Returns [`StateError::Json`] if the event
    /// fails to serialize.
    pub fn halt(&mut self, body: EventBody) -> Result<()> {
        debug_assert!(
            matches!(
                body,
                EventBody::RunHalted { .. }
                    | EventBody::RunStalled { .. }
                    | EventBody::RunCapReached { .. }
            ),
            "RunWriter::halt expects a terminal event variant"
        );
        self.append_event(&Event::now(body))?;
        // Best-effort: missing marker is fine (idempotent halt).
        let marker = self.root.live_marker(&self.id);
        match fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StateError::Io(e)),
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

    /// Parse the entire `events.jsonl` into a `Vec<Event>`. For
    /// small files (typical: 10s of KB to a few MB) this is fine;
    /// callers wanting a streaming reader should use
    /// [`Self::events_stream`].
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if the file exists but cannot
    /// be read, or [`StateError::Json`] if any line fails to
    /// parse.
    pub fn events(&self) -> Result<Vec<Event>> {
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)?;
        let mut out = Vec::new();
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        Ok(out)
    }

    /// Iterator over events as they're parsed. Stops at the first
    /// malformed line, returning the parse error.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Io`] if `events.jsonl` exists but
    /// cannot be opened. A missing file is treated as an empty
    /// iterator (no error).
    pub fn events_stream(&self) -> Result<impl Iterator<Item = Result<Event>>> {
        use std::io::BufRead;
        let path = self.root.run_dir(&self.id).join("events.jsonl");
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Empty iterator (no events yet).
                return Ok(EventsIter {
                    inner: Box::new(std::iter::empty()),
                });
            }
            Err(e) => return Err(StateError::Io(e)),
        };
        let reader = io::BufReader::new(file);
        let inner = reader.lines().filter_map(|l| match l {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(serde_json::from_str::<Event>(&s).map_err(StateError::from)),
            Err(e) => Some(Err(StateError::Io(e))),
        });
        Ok(EventsIter {
            inner: Box::new(inner),
        })
    }

    /// Read a blob's bytes. Verifies the on-disk hash matches the
    /// reference; mismatch is a corruption signal.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::MissingBlob`] if the referenced
    /// blob is not on disk; [`StateError::Io`] if the file
    /// exists but cannot be read; [`StateError::BlobHashMismatch`]
    /// if the on-disk content does not hash to the expected sha.
    pub fn read_blob(&self, blob: &BlobRef) -> Result<Vec<u8>> {
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
        let mut buf = Vec::with_capacity(usize::try_from(blob.size).unwrap_or(usize::MAX));
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
    fn double_start_is_rejected() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let mut a = root.create_run(id.clone()).unwrap();
        a.start(EventBody::RunStarted {
            domain: "test".into(),
            target: serde_json::Value::Null,
        })
        .unwrap();

        // Second writer on the same id tries to start: collision.
        let mut b = root.create_run(id.clone()).unwrap();
        let err = b
            .start(EventBody::RunStarted {
                domain: "test".into(),
                target: serde_json::Value::Null,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::AlreadyStarted(_)));
    }

    #[test]
    fn blob_dedup_skips_second_write() {
        let (_tmp, root) = fresh();
        let id = RunId::generate();
        let writer = root.create_run(id.clone()).unwrap();

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
        let writer = root.create_run(id.clone()).unwrap();
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
        let writer = root.create_run(id.clone()).unwrap();
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
