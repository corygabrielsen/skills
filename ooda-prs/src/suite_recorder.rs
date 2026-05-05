//! Suite-level memory harness — the multi-PR sibling of `Recorder`.
//!
//! Where `Recorder` writes per-PR audit trails under
//! `<state-root>/github.com/<owner>/<repo>/prs/<pr>/`, `SuiteRecorder`
//! writes per-invocation suite trails under
//! `<state-root>/suites/<suite-id>/`. The two are independent and
//! coexist on the same state-root tree, which is keyed by forge +
//! repo + PR for per-PR records and by `suite-id` for suite records.
//!
//! Layout:
//!
//! ```text
//! <state-root>/suites/<suite-id>/
//!   manifest.json   -- argv, started_at, suite, mode, max_iter, ...
//!   pointers.json   -- per-PR (slug, pr, run_id) cross-references
//!   outcome.json    -- the final MultiOutcome + aggregate exit code
//!   trace.md        -- human-readable summary table
//! ```
//!
//! `<suite-id>` shares the same `<utc>-<nanos>-p<pid>` shape as
//! per-PR `<run-id>`. Two simultaneous suite invocations against
//! overlapping PRs each get a distinct `<suite-id>`; the per-PR
//! `runs/<run-id>/` namespacing prevents ledger-level collisions.
//!
//! Cross-PR thread-safety: each PR thread calls `register_pr` after
//! opening its own `Recorder`. The shared `Inner` is wrapped in
//! `Arc<Mutex<_>>` for serialized mutation. `record_outcome` and
//! `record_started` are called from the main thread, before/after
//! the spawn loop's `thread::scope` joins.
//!
//! Best-effort writes: I/O failures inside the recorder are
//! swallowed (same pattern as `Recorder`) — a write failure must
//! not change the binary's `Outcome`. Errors at `open` time DO
//! surface as `BinaryError` because manifest.json is the entry
//! point for any audit.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::multi_outcome::MultiOutcome;
use crate::recorder::{RecorderError, RunMode, make_run_id, resolve_state_root};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct SuiteRecorderConfig {
    pub suite: Vec<(RepoSlug, PullRequestNumber)>,
    pub mode: RunMode,
    pub max_iter: u32,
    pub status_comment: bool,
    pub state_root: Option<PathBuf>,
    pub concurrency: Option<u32>,
}

#[derive(Clone)]
pub struct SuiteRecorder {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    state_root: PathBuf,
    suite_root: PathBuf,
    suite_id: String,
    started_at: DateTime<Utc>,
    suite: Vec<(RepoSlug, PullRequestNumber)>,
    pointers: Vec<PrPointer>,
}

#[derive(Debug, Clone, Serialize)]
struct PrPointer {
    slug: String,
    pr: u64,
    run_id: String,
}

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    suite_id: &'a str,
    started_at: DateTime<Utc>,
    forge: &'a str,
    mode: RunMode,
    max_iter: u32,
    status_comment: bool,
    concurrency: Option<u32>,
    cwd: String,
    argv: Vec<String>,
    suite: Vec<SuiteMember>,
}

#[derive(Serialize)]
struct SuiteMember {
    slug: String,
    pr: u64,
}

#[derive(Serialize)]
struct PointersFile<'a> {
    schema_version: u32,
    suite_id: &'a str,
    prs: &'a [PrPointer],
}

#[derive(Serialize)]
struct OutcomeFile<'a> {
    schema_version: u32,
    suite_id: &'a str,
    finished_at: DateTime<Utc>,
    exit_code: u8,
    multi_outcome: &'a MultiOutcome,
}

impl SuiteRecorder {
    pub fn open(cfg: SuiteRecorderConfig) -> Result<Self, RecorderError> {
        let state_root = resolve_state_root(cfg.state_root.as_deref());
        let now = Utc::now();
        let suite_id = make_run_id(now);
        let suite_root = state_root.join("suites").join(&suite_id);

        fs::create_dir_all(&suite_root).map_err(RecorderError::Io)?;

        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            suite_id: &suite_id,
            started_at: now,
            forge: "github.com",
            mode: cfg.mode,
            max_iter: cfg.max_iter,
            status_comment: cfg.status_comment,
            concurrency: cfg.concurrency,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".into()),
            argv: std::env::args().collect(),
            suite: cfg
                .suite
                .iter()
                .map(|(slug, pr)| SuiteMember {
                    slug: slug.to_string(),
                    pr: pr.get(),
                })
                .collect(),
        };
        write_json(&suite_root.join("manifest.json"), &manifest)?;

        // trace.md header — the per-PR pointers and final outcome
        // append after worker threads finish.
        let trace_path = suite_root.join("trace.md");
        let mut trace = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_path)
            .map_err(RecorderError::Io)?;
        let header = format!(
            "===== ooda-prs {} suite_id={} state_root={} mode={} max_iter={} status_comment={} concurrency={} =====\n\nSuite: {}\n",
            now.to_rfc3339(),
            suite_id,
            state_root.display(),
            cfg.mode,
            cfg.max_iter,
            cfg.status_comment,
            cfg.concurrency
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unbounded".into()),
            cfg.suite
                .iter()
                .map(|(s, p)| format!("{s}#{p}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let _ = writeln!(trace, "{header}");

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                state_root,
                suite_root,
                suite_id,
                started_at: now,
                suite: cfg.suite,
                pointers: Vec::new(),
            })),
        })
    }

    /// Record the per-PR `run_id` so the suite's `pointers.json`
    /// links `(slug, pr)` to the per-PR Recorder's run directory.
    /// Called from each worker thread after `Recorder::open`.
    /// Best-effort: file-write failures do not change the worker's
    /// behavior.
    pub fn register_pr(&self, slug: &RepoSlug, pr: PullRequestNumber, run_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.pointers.push(PrPointer {
                slug: slug.to_string(),
                pr: pr.get(),
                run_id: run_id.to_string(),
            });
            let _ = inner.write_pointers();
        }
    }

    /// Final write: outcome.json + close the trace.md summary table.
    /// Called from the main thread after `thread::scope` joins.
    pub fn record_outcome(&self, multi: &MultiOutcome, exit_code: u8) {
        if let Ok(inner) = self.inner.lock() {
            let _ = inner.write_outcome(multi, exit_code);
            let _ = inner.write_trace_summary(multi, exit_code);
        }
    }

    /// Path to `<state-root>/suites/<suite-id>/`. Useful for tests
    /// and for the `BinaryError` triage path that wants to point a
    /// human at the audit trail.
    pub fn suite_root(&self) -> PathBuf {
        self.inner
            .lock()
            .map(|inner| inner.suite_root.clone())
            .unwrap_or_default()
    }

    pub fn suite_id(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.suite_id.clone())
            .unwrap_or_default()
    }
}

impl Inner {
    fn write_pointers(&self) -> Result<(), RecorderError> {
        let path = self.suite_root.join("pointers.json");
        let payload = PointersFile {
            schema_version: SCHEMA_VERSION,
            suite_id: &self.suite_id,
            prs: &self.pointers,
        };
        write_json(&path, &payload)
    }

    fn write_outcome(&self, multi: &MultiOutcome, exit_code: u8) -> Result<(), RecorderError> {
        let path = self.suite_root.join("outcome.json");
        let payload = OutcomeFile {
            schema_version: SCHEMA_VERSION,
            suite_id: &self.suite_id,
            finished_at: Utc::now(),
            exit_code,
            multi_outcome: multi,
        };
        write_json(&path, &payload)
    }

    fn write_trace_summary(
        &self,
        multi: &MultiOutcome,
        exit_code: u8,
    ) -> Result<(), RecorderError> {
        let trace_path = self.suite_root.join("trace.md");
        let mut trace = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_path)
            .map_err(RecorderError::Io)?;

        // Map (slug, pr) → run_id (from pointers).
        let pointer_for = |slug: &str, pr: u64| -> &str {
            self.pointers
                .iter()
                .find(|p| p.slug == slug && p.pr == pr)
                .map(|p| p.run_id.as_str())
                .unwrap_or("(no run_id; recorder open failed)")
        };

        writeln!(trace, "## Per-PR results")?;
        writeln!(trace)?;
        writeln!(trace, "| slug | pr | run_id | outcome | exit |")?;
        writeln!(trace, "| --- | --- | --- | --- | --- |")?;
        match multi {
            MultiOutcome::UsageError(_) => {
                writeln!(trace, "| _none_ | — | — | UsageError | {exit_code} |")?;
            }
            MultiOutcome::Bundle(prs) => {
                for po in prs {
                    let slug = po.slug.to_string();
                    let pr = po.pr.get();
                    let run_id = pointer_for(&slug, pr);
                    writeln!(
                        trace,
                        "| {} | {} | {} | {} | {} |",
                        slug,
                        pr,
                        run_id,
                        outcome_short(&po.outcome),
                        po.outcome.exit_code(),
                    )?;
                }
            }
        }
        writeln!(trace)?;
        writeln!(
            trace,
            "Aggregate exit: **{exit_code}** (started_at={}, finished_at={})",
            self.started_at.to_rfc3339(),
            Utc::now().to_rfc3339(),
        )?;
        Ok(())
    }
}

fn outcome_short(o: &crate::outcome::Outcome) -> String {
    use crate::outcome::Outcome;
    match o {
        Outcome::DoneMerged => "DoneMerged".into(),
        Outcome::StuckRepeated(a) => format!("StuckRepeated:{}", a.kind.name()),
        Outcome::StuckCapReached(a) => format!("StuckCapReached:{}", a.kind.name()),
        Outcome::HandoffHuman(a) => format!("HandoffHuman:{}", a.kind.name()),
        Outcome::WouldAdvance(a) => format!("WouldAdvance:{}", a.kind.name()),
        Outcome::HandoffAgent(a) => format!("HandoffAgent:{}", a.kind.name()),
        Outcome::BinaryError(_) => "BinaryError".into(),
        Outcome::Paused => "Paused".into(),
        Outcome::DoneClosed => "DoneClosed".into(),
        Outcome::UsageError(_) => "UsageError".into(),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RecorderError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(RecorderError::Io)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(RecorderError::Json)?;
    fs::write(path, bytes).map_err(RecorderError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::Outcome;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::parse(s).unwrap()
    }

    fn pr(n: u64) -> PullRequestNumber {
        PullRequestNumber::new(n).unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ooda-prs-suite-recorder-test-{label}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn open_writes_manifest_and_trace_header() {
        let root = temp_root("open");
        let _ = fs::remove_dir_all(&root);
        let rec = SuiteRecorder::open(SuiteRecorderConfig {
            suite: vec![(slug("a/b"), pr(1)), (slug("a/b"), pr(2))],
            mode: RunMode::Loop,
            max_iter: 10,
            status_comment: false,
            state_root: Some(root.clone()),
            concurrency: Some(2),
        })
        .unwrap();

        let suite_root = rec.suite_root();
        assert!(suite_root.starts_with(root.join("suites")));
        assert!(suite_root.join("manifest.json").exists());
        assert!(suite_root.join("trace.md").exists());

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(suite_root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["mode"], "loop");
        assert_eq!(manifest["max_iter"], 10);
        assert_eq!(manifest["status_comment"], false);
        assert_eq!(manifest["concurrency"], 2);
        assert_eq!(manifest["suite"][0]["slug"], "a/b");
        assert_eq!(manifest["suite"][0]["pr"], 1);
        assert_eq!(manifest["suite"][1]["pr"], 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn register_pr_writes_pointers_json() {
        let root = temp_root("register");
        let _ = fs::remove_dir_all(&root);
        let rec = SuiteRecorder::open(SuiteRecorderConfig {
            suite: vec![(slug("a/b"), pr(1))],
            mode: RunMode::Inspect,
            max_iter: 1,
            status_comment: false,
            state_root: Some(root.clone()),
            concurrency: None,
        })
        .unwrap();

        rec.register_pr(&slug("a/b"), pr(1), "20260505T120000Z-000000000-p1234");
        let p: serde_json::Value =
            serde_json::from_slice(&fs::read(rec.suite_root().join("pointers.json")).unwrap())
                .unwrap();
        assert_eq!(p["prs"][0]["slug"], "a/b");
        assert_eq!(p["prs"][0]["pr"], 1);
        assert_eq!(p["prs"][0]["run_id"], "20260505T120000Z-000000000-p1234");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_outcome_writes_outcome_and_appends_trace() {
        use crate::multi_outcome::ProcessOutcome;
        let root = temp_root("record");
        let _ = fs::remove_dir_all(&root);
        let rec = SuiteRecorder::open(SuiteRecorderConfig {
            suite: vec![(slug("a/b"), pr(1)), (slug("a/b"), pr(2))],
            mode: RunMode::Loop,
            max_iter: 50,
            status_comment: false,
            state_root: Some(root.clone()),
            concurrency: None,
        })
        .unwrap();

        rec.register_pr(&slug("a/b"), pr(1), "RUN-1");
        rec.register_pr(&slug("a/b"), pr(2), "RUN-2");

        let multi = MultiOutcome::Bundle(vec![
            ProcessOutcome {
                slug: slug("a/b"),
                pr: pr(1),
                outcome: Outcome::DoneMerged,
            },
            ProcessOutcome {
                slug: slug("a/b"),
                pr: pr(2),
                outcome: Outcome::Paused,
            },
        ]);
        rec.record_outcome(&multi, 0);

        let out: serde_json::Value =
            serde_json::from_slice(&fs::read(rec.suite_root().join("outcome.json")).unwrap())
                .unwrap();
        assert_eq!(out["exit_code"], 0);
        // multi_outcome serializes as the enum-variant form
        // {"Bundle":[...]} per serde's default sum encoding.
        assert!(out["multi_outcome"]["Bundle"].is_array());

        let trace = fs::read_to_string(rec.suite_root().join("trace.md")).unwrap();
        assert!(trace.contains("a/b"));
        assert!(trace.contains("RUN-1"));
        assert!(trace.contains("DoneMerged"));
        assert!(trace.contains("Paused"));
        assert!(trace.contains("Aggregate exit: **0**"));

        let _ = fs::remove_dir_all(&root);
    }
}
