//! Semantic projection of [`ooda_state`] events into a typed
//! [`RunSnapshot`] for UI/CLI consumers.
//!
//! # Role
//!
//! `ooda-state` is the wire: append-only `events.jsonl` + content-
//! addressed blobs. Consumers (cockpit's REST/SSE surface, CLI
//! summaries, future TUIs) need an aggregated view — "what is this
//! run currently doing, per iteration, per axis" — without
//! re-deriving fold logic per consumer. This crate owns that fold.
//!
//! # Domain neutrality
//!
//! The core [`RunSnapshot`] is domain-agnostic. Per-domain semantics
//! land in [`DomainView`], a closed enum: [`DomainView::Pr`] for the
//! PR-side recorders, [`DomainView::Other`] as the forward-compatible
//! fallback. Adding a third domain is one variant + one match arm in
//! the folder.
//!
//! # Blob reading
//!
//! The projection extracts short prose summaries from per-axis
//! dashboard / oriented-state blobs. Callers pass a
//! [`BlobReader`] (e.g. a wrapped [`ooda_state::RunReader`]) so the
//! crate has no transitive dependency on a specific reader handle.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ooda_state::{BlobRef, Event, EventBody, RunReader};
use serde::{Deserialize, Serialize};

// ── Errors ───────────────────────────────────────────────────────────

/// Reasons projection cannot produce a [`RunSnapshot`].
#[derive(Debug)]
pub enum ProjectionError {
    /// Event list is empty or its first event is not
    /// [`EventBody::RunStarted`]. Every run begins with `RunStarted`;
    /// missing it means the input is structurally malformed.
    MissingRunStarted,
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRunStarted => f.write_str("event list does not begin with RunStarted"),
        }
    }
}

impl std::error::Error for ProjectionError {}

// ── BlobReader trait ─────────────────────────────────────────────────

/// Pluggable blob fetch for the projector. Implementations dereference
/// a [`BlobRef`] to bytes; the projector only needs read-access
/// because axis-summary extraction parses the blob as JSON and reads
/// a handful of top-level fields.
pub trait BlobReader {
    /// Fetch the bytes for `blob`. Implementations should verify the
    /// content hash matches the reference (the [`RunReader`] impl
    /// here does so transparently).
    ///
    /// # Errors
    ///
    /// Returns any I/O or hash-mismatch error the underlying store
    /// surfaces; the projector tolerates failures by skipping the
    /// summary extraction.
    fn read_blob(&self, blob: &BlobRef) -> std::io::Result<Vec<u8>>;
}

impl BlobReader for RunReader {
    fn read_blob(&self, blob: &BlobRef) -> std::io::Result<Vec<u8>> {
        RunReader::read_blob(self, blob).map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// No-op reader: every blob fetch returns [`std::io::ErrorKind::NotFound`].
/// Use when the caller wants the framing-only projection without
/// per-axis summary extraction (cheap, no disk reads).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullBlobReader;

impl BlobReader for NullBlobReader {
    fn read_blob(&self, _blob: &BlobRef) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

// ── Core snapshot ────────────────────────────────────────────────────

/// Aggregated view of one OODA run, derived by folding the run's
/// events.jsonl. Domain-neutral fields plus an optional typed
/// per-domain overlay in [`Self::domain_view`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub domain: String,
    pub target: serde_json::Value,
    pub started_at: DateTime<Utc>,
    pub latest_event_at: DateTime<Utc>,
    pub status: RunStatus,
    pub outcome: Option<OutcomeSnapshot>,
    pub iterations: Vec<IterationSnapshot>,
    pub domain_view: DomainView,
}

/// Run-level lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    Halted,
    Stalled,
    CapReached,
}

/// Terminal-event summary. Populated when the run reaches any of the
/// three terminal states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeSnapshot {
    /// Wire token from the terminal event (e.g. `"DoneMerged"` for
    /// PR-side success). For [`EventBody::RunStalled`] /
    /// [`EventBody::RunCapReached`] this is the variant name itself.
    pub kind: String,
    /// Exit code if available (always present for `RunHalted`).
    pub exit_code: i32,
    /// Headline string projected from the `domain_specific:outcome`
    /// event's payload, if the recorder emitted one.
    pub headline: Option<String>,
}

/// Per-iteration projection. Each OODA cycle (observe → orient →
/// decide → act/wait/handoff) lands in one slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationSnapshot {
    pub iteration: u32,
    pub observed_at: Option<DateTime<Utc>>,
    pub oriented_at: Option<DateTime<Utc>>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_kind: Option<String>,
    pub action_kind: Option<String>,
    pub executed: bool,
    pub success: Option<bool>,
    pub waited_ms: Option<u64>,
    pub handoff: Option<HandoffSnapshot>,
    pub blob_refs: Vec<BlobRefView>,
}

impl IterationSnapshot {
    fn new(iteration: u32) -> Self {
        Self {
            iteration,
            observed_at: None,
            oriented_at: None,
            decided_at: None,
            decision_kind: None,
            action_kind: None,
            executed: false,
            success: None,
            waited_ms: None,
            handoff: None,
            blob_refs: Vec::new(),
        }
    }
}

/// Handoff-event projection. `variant` is the
/// [`EventBody::IterationHandoff::variant`] wire token
/// (`"HandoffHuman"` / `"HandoffAgent"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffSnapshot {
    pub variant: String,
    pub action_kind: String,
    pub blob: BlobRefView,
}

/// Serializable view of a [`BlobRef`]. Carries the originating
/// event kind (e.g. `"dashboard"`, `"candidates"`, `"handoff_body"`)
/// so a UI can label download links without re-deriving context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRefView {
    pub sha: String,
    pub size: u64,
    pub ext: String,
    pub kind: String,
}

impl BlobRefView {
    fn from_ref(blob: &BlobRef, kind: &str) -> Self {
        Self {
            sha: blob.sha.clone(),
            size: blob.size,
            ext: blob.ext.clone(),
            kind: kind.to_string(),
        }
    }
}

// ── Domain overlay ───────────────────────────────────────────────────

/// Per-domain typed projection overlay. Closed-set: every domain that
/// renders distinctively in a UI gets its own variant. [`Self::Other`]
/// is the forward-compatible fallback for runs whose `domain` field
/// is unknown to this crate.
///
/// The PR view is boxed so the enum's stack footprint stays close to
/// the [`Self::Other`] variant; aggregated [`RunSnapshot`] values move
/// around freely without paying for the larger overlay on every clone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "domain", rename_all = "snake_case")]
pub enum DomainView {
    Pr(Box<PrDomainView>),
    Other,
}

/// PR-side domain overlay (`ooda-pr`, `ooda-prs`, `ooda-pr-codex-review`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrDomainView {
    /// `owner/repo` slug parsed from the run's target.
    pub slug: String,
    /// PR number parsed from the run's target.
    pub pr: u64,
    pub axes: PrAxisStatuses,
    pub branch_divergence: Option<BranchDivergenceView>,
}

/// PR-domain axis statuses. Each field is `Some` once the
/// corresponding axis emits at least one
/// [`ooda_state::tokens::DomainKind::IterationDashboard`] event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrAxisStatuses {
    pub ci: Option<AxisStatus>,
    pub reviews: Option<AxisStatus>,
    pub copilot: Option<AxisStatus>,
    pub cursor: Option<AxisStatus>,
    pub claude_review: Option<AxisStatus>,
    pub doc_review: Option<AxisStatus>,
    pub pull_request_metadata: Option<AxisStatus>,
    pub closeout: Option<AxisStatus>,
    pub state: Option<AxisStatus>,
    pub branch_sync: Option<AxisStatus>,
}

impl PrAxisStatuses {
    fn slot_mut(&mut self, axis: &str) -> Option<&mut Option<AxisStatus>> {
        match axis {
            "ci" => Some(&mut self.ci),
            "reviews" => Some(&mut self.reviews),
            "copilot" => Some(&mut self.copilot),
            "cursor" => Some(&mut self.cursor),
            "claude_review" => Some(&mut self.claude_review),
            "doc_review" => Some(&mut self.doc_review),
            "pull_request_metadata" => Some(&mut self.pull_request_metadata),
            "closeout" => Some(&mut self.closeout),
            "state" => Some(&mut self.state),
            "branch_sync" => Some(&mut self.branch_sync),
            _ => None,
        }
    }
}

/// Per-axis status. `state_summary` is a freeform short prose snippet
/// projected from the axis's dashboard / oriented blob;
/// `current_candidate` carries the `action_kind` the axis nominated in
/// the latest iteration (if any).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisStatus {
    pub last_observed_at: DateTime<Utc>,
    pub state_summary: String,
    pub current_candidate: Option<String>,
}

/// Branch-divergence overlay for PR-side runs. Populated from the
/// recorder's `domain_specific:branch_divergence` event when present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchDivergenceView {
    pub from_sha: String,
    pub to_sha: String,
    pub graphite_tracked: bool,
}

// ── Projection function ──────────────────────────────────────────────

/// Fold a slice of events into a [`RunSnapshot`]. Idempotent;
/// callers may re-project after every appended event.
///
/// `reader` supplies bytes for content-addressed blobs the projector
/// dereferences when computing per-axis summaries. Pass
/// [`NullBlobReader`] to skip blob reads.
///
/// # Errors
///
/// Returns [`ProjectionError::MissingRunStarted`] if `events` is
/// empty or its first event is not [`EventBody::RunStarted`].
pub fn project_run(
    events: &[Event],
    reader: &dyn BlobReader,
    run_id: &str,
) -> Result<RunSnapshot, ProjectionError> {
    let mut iter = events.iter();
    let first = iter.next().ok_or(ProjectionError::MissingRunStarted)?;
    let EventBody::RunStarted { domain, target } = &first.body else {
        return Err(ProjectionError::MissingRunStarted);
    };

    let domain_view = initial_domain_view(domain, target);
    let mut snap = RunSnapshot {
        run_id: run_id.to_string(),
        domain: domain.clone(),
        target: target.clone(),
        started_at: first.ts,
        latest_event_at: first.ts,
        status: RunStatus::Active,
        outcome: None,
        iterations: Vec::new(),
        domain_view,
    };
    let mut iters: BTreeMap<u32, IterationSnapshot> = BTreeMap::new();

    for ev in events.iter().skip(1) {
        snap.latest_event_at = ev.ts;
        apply_event(&mut snap, &mut iters, ev, reader);
    }
    snap.iterations = iters.into_values().collect();
    Ok(snap)
}

fn initial_domain_view(domain: &str, target: &serde_json::Value) -> DomainView {
    if domain == "pr"
        && let Some(view) = pr_view_from_target(target)
    {
        return DomainView::Pr(Box::new(view));
    }
    DomainView::Other
}

fn pr_view_from_target(target: &serde_json::Value) -> Option<PrDomainView> {
    let slug = target.get("slug")?.as_str()?.to_string();
    let pr = target.get("pr")?.as_u64()?;
    Some(PrDomainView {
        slug,
        pr,
        axes: PrAxisStatuses::default(),
        branch_divergence: None,
    })
}

fn apply_event(
    snap: &mut RunSnapshot,
    iters: &mut BTreeMap<u32, IterationSnapshot>,
    ev: &Event,
    reader: &dyn BlobReader,
) {
    match &ev.body {
        EventBody::RunStarted { .. } => {
            // Only the first event is RunStarted by construction;
            // a duplicate is structurally invalid but tolerated
            // (idempotent fold).
        }
        EventBody::IterationObserved { iteration, blob } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.observed_at = Some(ev.ts);
            slot.blob_refs.push(BlobRefView::from_ref(blob, "observed"));
        }
        EventBody::IterationOriented { iteration, blob } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.oriented_at = Some(ev.ts);
            slot.blob_refs.push(BlobRefView::from_ref(blob, "oriented"));
        }
        EventBody::IterationDecided {
            iteration,
            decision_kind,
        } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.decided_at = Some(ev.ts);
            slot.decision_kind = Some(decision_kind.clone());
        }
        EventBody::IterationHandoff {
            iteration,
            variant,
            action_kind,
            blob,
        } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.action_kind = Some(action_kind.clone());
            slot.handoff = Some(HandoffSnapshot {
                variant: variant.clone(),
                action_kind: action_kind.clone(),
                blob: BlobRefView::from_ref(blob, "handoff_body"),
            });
        }
        EventBody::IterationExecuted {
            iteration,
            action_kind,
            success,
        } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.action_kind = Some(action_kind.clone());
            slot.executed = true;
            slot.success = Some(*success);
        }
        EventBody::IterationWaited {
            iteration,
            action_kind,
            interval_ms,
        } => {
            let slot = iters
                .entry(*iteration)
                .or_insert_with(|| IterationSnapshot::new(*iteration));
            slot.action_kind = Some(action_kind.clone());
            slot.waited_ms = Some(*interval_ms);
        }
        EventBody::RunHalted { outcome, exit_code } => {
            snap.status = RunStatus::Halted;
            // Preserve any headline a prior `domain_specific:outcome`
            // event already projected.
            let headline = snap.outcome.as_ref().and_then(|o| o.headline.clone());
            snap.outcome = Some(OutcomeSnapshot {
                kind: outcome.clone(),
                exit_code: *exit_code,
                headline,
            });
        }
        EventBody::RunStalled { last_action } => {
            snap.status = RunStatus::Stalled;
            snap.outcome = Some(OutcomeSnapshot {
                kind: "RunStalled".to_string(),
                exit_code: 0,
                headline: Some(format!("stalled on action: {last_action}")),
            });
        }
        EventBody::RunCapReached { last_action } => {
            snap.status = RunStatus::CapReached;
            snap.outcome = Some(OutcomeSnapshot {
                kind: "RunCapReached".to_string(),
                exit_code: 0,
                headline: Some(format!("cap reached on action: {last_action}")),
            });
        }
        EventBody::DomainSpecific {
            kind_suffix,
            payload,
        } => apply_domain_specific(snap, iters, ev.ts, kind_suffix, payload, reader),
    }
}

fn apply_domain_specific(
    snap: &mut RunSnapshot,
    iters: &mut BTreeMap<u32, IterationSnapshot>,
    ts: DateTime<Utc>,
    kind_suffix: &str,
    payload: &serde_json::Value,
    reader: &dyn BlobReader,
) {
    match kind_suffix {
        "outcome" => {
            if let Some(headline) = payload.get("headline").and_then(|v| v.as_str()) {
                let existing = snap.outcome.clone();
                snap.outcome = Some(OutcomeSnapshot {
                    kind: existing
                        .as_ref()
                        .map_or_else(|| "Pending".to_string(), |o| o.kind.clone()),
                    exit_code: existing.as_ref().map_or(0, |o| o.exit_code),
                    headline: Some(headline.to_string()),
                });
            }
        }
        "branch_divergence" => {
            if let DomainView::Pr(view) = &mut snap.domain_view
                && let Some(divergence) = branch_divergence_from_payload(payload)
            {
                view.branch_divergence = Some(divergence);
            }
        }
        "iteration_dashboard" => {
            attach_axis_summary(snap, iters, ts, payload, reader, AxisField::Dashboard);
        }
        "iteration_candidates" => {
            attach_axis_summary(snap, iters, ts, payload, reader, AxisField::Candidates);
        }
        _ => {
            // Other domain_specific events (observe_started,
            // tool_call_*, trace_line, etc.) are tracked at the
            // raw-event layer; the projector does not surface them
            // individually. They reach UIs via /api/runs/<id>/events
            // raw SSE if needed.
        }
    }
}

/// Which payload kind we are currently bucketing into the axis.
/// Both dashboard and candidates events carry an axis-tagged blob
/// reference the projector dereferences for the prose summary.
#[derive(Debug, Clone, Copy)]
enum AxisField {
    Dashboard,
    Candidates,
}

fn attach_axis_summary(
    snap: &mut RunSnapshot,
    iters: &mut BTreeMap<u32, IterationSnapshot>,
    ts: DateTime<Utc>,
    payload: &serde_json::Value,
    reader: &dyn BlobReader,
    field: AxisField,
) {
    let Some(blob) = payload.get("blob").and_then(blob_ref_from_value) else {
        return;
    };
    let iteration = payload.get("iteration").and_then(serde_json::Value::as_u64);
    if let Some(iteration) = iteration {
        let iter_slot = iters
            .entry(u32::try_from(iteration).unwrap_or(u32::MAX))
            .or_insert_with(|| {
                IterationSnapshot::new(u32::try_from(iteration).unwrap_or(u32::MAX))
            });
        let kind = match field {
            AxisField::Dashboard => "dashboard",
            AxisField::Candidates => "candidates",
        };
        iter_slot.blob_refs.push(BlobRefView::from_ref(&blob, kind));
    }
    // Per-axis summary extraction is only meaningful for the PR
    // domain right now; other domains route through DomainView::Other
    // and skip axis extraction.
    let DomainView::Pr(pr_view) = &mut snap.domain_view else {
        return;
    };
    let Ok(bytes) = reader.read_blob(&blob) else {
        return;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    match field {
        AxisField::Dashboard => merge_dashboard_into_axes(&mut pr_view.axes, &parsed, ts),
        AxisField::Candidates => merge_candidates_into_axes(&mut pr_view.axes, &parsed, ts),
    }
}

/// Walk a dashboard blob (typically a map of `<axis> -> <axis-report>`
/// or a flat object of per-axis fields) and project each known axis
/// into [`PrAxisStatuses`]. Unknown axes are ignored; the schema is
/// forward-compatible at the projection layer (new axes simply do
/// not surface in the typed view until the enum is extended).
fn merge_dashboard_into_axes(
    axes: &mut PrAxisStatuses,
    parsed: &serde_json::Value,
    ts: DateTime<Utc>,
) {
    let Some(map) = parsed.as_object() else {
        return;
    };
    for (axis_name, axis_value) in map {
        let Some(slot) = axes.slot_mut(axis_name.as_str()) else {
            continue;
        };
        let summary = summarize_axis_value(axis_value);
        let candidate = slot.as_ref().and_then(|a| a.current_candidate.clone());
        *slot = Some(AxisStatus {
            last_observed_at: ts,
            state_summary: summary,
            current_candidate: candidate,
        });
    }
}

/// Walk a candidates blob — a list of `{ axis, action_kind, ... }`
/// records the orient layer emits — and update each known axis's
/// `current_candidate` field.
fn merge_candidates_into_axes(
    axes: &mut PrAxisStatuses,
    parsed: &serde_json::Value,
    ts: DateTime<Utc>,
) {
    let Some(list) = parsed.as_array() else {
        return;
    };
    for entry in list {
        let Some(axis_name) = entry.get("axis").and_then(|v| v.as_str()) else {
            continue;
        };
        let action_kind = entry
            .get("action_kind")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(slot) = axes.slot_mut(axis_name) else {
            continue;
        };
        if let Some(existing) = slot.as_mut() {
            existing.current_candidate = action_kind;
            existing.last_observed_at = ts;
        } else {
            *slot = Some(AxisStatus {
                last_observed_at: ts,
                state_summary: String::new(),
                current_candidate: action_kind,
            });
        }
    }
}

/// Render a per-axis report value as a short prose summary. Tries a
/// handful of well-known fields the PR-side axis reports populate
/// (`headline`, `status`, `summary`) before falling back to the
/// JSON-rendered first line.
fn summarize_axis_value(value: &serde_json::Value) -> String {
    for key in ["headline", "summary", "status", "state"] {
        if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
            return truncate(s, 200);
        }
    }
    let rendered = match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    truncate(rendered.lines().next().unwrap_or(""), 200)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn blob_ref_from_value(value: &serde_json::Value) -> Option<BlobRef> {
    serde_json::from_value(value.clone()).ok()
}

fn branch_divergence_from_payload(payload: &serde_json::Value) -> Option<BranchDivergenceView> {
    let from_sha = payload.get("from_sha")?.as_str()?.to_string();
    let to_sha = payload.get("to_sha")?.as_str()?.to_string();
    let graphite_tracked = payload
        .get("graphite_tracked")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(BranchDivergenceView {
        from_sha,
        to_sha,
        graphite_tracked,
    })
}

// ── Projected wire events ────────────────────────────────────────────

/// Delta event the cockpit projection-SSE endpoint emits over the
/// wire. [`Self::Snapshot`] is the per-connection backfill frame;
/// subsequent variants apply incrementally to the client's local
/// state.
///
/// [`Self::Snapshot`] is boxed so the enum's stack footprint matches
/// the smaller delta variants; the backfill frame is emitted at most
/// once per SSE connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectedEvent {
    Snapshot(Box<RunSnapshot>),
    IterationUpdate(IterationSnapshot),
    StatusChange {
        status: RunStatus,
        outcome: Option<OutcomeSnapshot>,
    },
    AxisUpdate {
        axis: String,
        status: AxisStatus,
    },
    BranchDivergence(BranchDivergenceView),
    HandoffEmitted(HandoffSnapshot),
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ooda_state::{BlobRef, EventBody};

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn ev(secs: i64, body: EventBody) -> Event {
        Event { ts: ts(secs), body }
    }

    fn blob(sha: &str, ext: &str) -> BlobRef {
        BlobRef {
            sha: sha.to_string(),
            size: 0,
            ext: ext.to_string(),
        }
    }

    #[test]
    fn empty_events_returns_missing_run_started() {
        let err = project_run(&[], &NullBlobReader, "x").unwrap_err();
        assert!(matches!(err, ProjectionError::MissingRunStarted));
    }

    #[test]
    fn first_event_must_be_run_started() {
        let evs = vec![ev(
            1,
            EventBody::RunHalted {
                outcome: "DoneMerged".into(),
                exit_code: 0,
            },
        )];
        let err = project_run(&evs, &NullBlobReader, "x").unwrap_err();
        assert!(matches!(err, ProjectionError::MissingRunStarted));
    }

    #[test]
    fn run_started_only_yields_active_snapshot() {
        let evs = vec![ev(
            10,
            EventBody::RunStarted {
                domain: "pr".into(),
                target: serde_json::json!({"slug": "owner/repo", "pr": 42}),
            },
        )];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.run_id, "r1");
        assert_eq!(snap.domain, "pr");
        assert_eq!(snap.status, RunStatus::Active);
        assert!(snap.iterations.is_empty());
        assert!(snap.outcome.is_none());
        match snap.domain_view {
            DomainView::Pr(v) => {
                assert_eq!(v.slug, "owner/repo");
                assert_eq!(v.pr, 42);
            }
            DomainView::Other => panic!("expected Pr view"),
        }
    }

    #[test]
    fn unknown_domain_yields_other_view() {
        let evs = vec![ev(
            1,
            EventBody::RunStarted {
                domain: "weather".into(),
                target: serde_json::json!({"city": "Berlin"}),
            },
        )];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert!(matches!(snap.domain_view, DomainView::Other));
    }

    #[test]
    fn full_lifecycle_populates_iteration_fields() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({"slug": "x/y", "pr": 1}),
                },
            ),
            ev(
                2,
                EventBody::IterationObserved {
                    iteration: 1,
                    blob: blob("aa", "json"),
                },
            ),
            ev(
                3,
                EventBody::IterationOriented {
                    iteration: 1,
                    blob: blob("bb", "json"),
                },
            ),
            ev(
                4,
                EventBody::IterationDecided {
                    iteration: 1,
                    decision_kind: "Execute".into(),
                },
            ),
            ev(
                5,
                EventBody::IterationExecuted {
                    iteration: 1,
                    action_kind: "ReRunCi".into(),
                    success: true,
                },
            ),
            ev(
                6,
                EventBody::RunHalted {
                    outcome: "DoneMerged".into(),
                    exit_code: 0,
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.status, RunStatus::Halted);
        assert_eq!(snap.iterations.len(), 1);
        let i = &snap.iterations[0];
        assert_eq!(i.iteration, 1);
        assert!(i.observed_at.is_some());
        assert!(i.oriented_at.is_some());
        assert_eq!(i.decision_kind.as_deref(), Some("Execute"));
        assert_eq!(i.action_kind.as_deref(), Some("ReRunCi"));
        assert!(i.executed);
        assert_eq!(i.success, Some(true));
        let outcome = snap.outcome.unwrap();
        assert_eq!(outcome.kind, "DoneMerged");
        assert_eq!(outcome.exit_code, 0);
    }

    #[test]
    fn handoff_event_populates_handoff_field() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({"slug": "x/y", "pr": 1}),
                },
            ),
            ev(
                2,
                EventBody::IterationHandoff {
                    iteration: 1,
                    variant: "HandoffHuman".into(),
                    action_kind: "AskHuman".into(),
                    blob: blob("cc", "md"),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        let h = snap.iterations[0].handoff.as_ref().unwrap();
        assert_eq!(h.variant, "HandoffHuman");
        assert_eq!(h.action_kind, "AskHuman");
        assert_eq!(h.blob.kind, "handoff_body");
    }

    #[test]
    fn stalled_terminal_sets_status_and_outcome() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                2,
                EventBody::RunStalled {
                    last_action: "Wait".into(),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.status, RunStatus::Stalled);
        let o = snap.outcome.unwrap();
        assert_eq!(o.kind, "RunStalled");
        assert!(o.headline.unwrap().contains("Wait"));
    }

    #[test]
    fn cap_reached_terminal_sets_status_and_outcome() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                2,
                EventBody::RunCapReached {
                    last_action: "ReRunCi".into(),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.status, RunStatus::CapReached);
        let o = snap.outcome.unwrap();
        assert_eq!(o.kind, "RunCapReached");
    }

    #[test]
    fn waited_event_populates_waited_ms() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                2,
                EventBody::IterationWaited {
                    iteration: 3,
                    action_kind: "Wait".into(),
                    interval_ms: 1500,
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.iterations[0].waited_ms, Some(1500));
        assert_eq!(snap.iterations[0].action_kind.as_deref(), Some("Wait"));
    }

    #[test]
    fn outcome_headline_from_domain_specific_then_halt_keeps_headline() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                2,
                EventBody::DomainSpecific {
                    kind_suffix: "outcome".into(),
                    payload: serde_json::json!({"headline": "all green"}),
                },
            ),
            ev(
                3,
                EventBody::RunHalted {
                    outcome: "DoneMerged".into(),
                    exit_code: 0,
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        let o = snap.outcome.unwrap();
        assert_eq!(o.kind, "DoneMerged");
        assert_eq!(o.headline.as_deref(), Some("all green"));
    }

    #[test]
    fn branch_divergence_populates_pr_view() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({"slug": "x/y", "pr": 1}),
                },
            ),
            ev(
                2,
                EventBody::DomainSpecific {
                    kind_suffix: "branch_divergence".into(),
                    payload: serde_json::json!({
                        "from_sha": "deadbeef",
                        "to_sha": "feedface",
                        "graphite_tracked": true,
                    }),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        let DomainView::Pr(view) = snap.domain_view else {
            panic!("expected Pr view");
        };
        let div = view.branch_divergence.unwrap();
        assert_eq!(div.from_sha, "deadbeef");
        assert_eq!(div.to_sha, "feedface");
        assert!(div.graphite_tracked);
    }

    /// In-memory blob reader for tests that need axis-summary
    /// extraction without touching disk.
    struct MapReader {
        blobs: std::collections::HashMap<String, Vec<u8>>,
    }

    impl BlobReader for MapReader {
        fn read_blob(&self, blob: &BlobRef) -> std::io::Result<Vec<u8>> {
            self.blobs
                .get(&blob.sha)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    #[test]
    fn dashboard_blob_populates_axis_status() {
        let dashboard = serde_json::json!({
            "ci": {"headline": "checks pending"},
            "reviews": {"headline": "1 reviewer requested"}
        });
        let bytes = serde_json::to_vec(&dashboard).unwrap();
        let mut blobs = std::collections::HashMap::new();
        blobs.insert("sha1".to_string(), bytes);
        let reader = MapReader { blobs };
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({"slug": "x/y", "pr": 1}),
                },
            ),
            ev(
                2,
                EventBody::DomainSpecific {
                    kind_suffix: "iteration_dashboard".into(),
                    payload: serde_json::json!({
                        "iteration": 1,
                        "blob": {"sha": "sha1", "size": 99, "ext": "json"},
                    }),
                },
            ),
        ];
        let snap = project_run(&evs, &reader, "r1").unwrap();
        let DomainView::Pr(view) = snap.domain_view else {
            panic!("expected Pr view");
        };
        assert_eq!(
            view.axes.ci.as_ref().unwrap().state_summary,
            "checks pending"
        );
        assert!(view.axes.reviews.is_some());
    }

    #[test]
    fn candidates_blob_sets_current_candidate_per_axis() {
        let candidates = serde_json::json!([
            {"axis": "ci", "action_kind": "ReRunCi"},
            {"axis": "reviews", "action_kind": "RequestReviews"},
        ]);
        let bytes = serde_json::to_vec(&candidates).unwrap();
        let mut blobs = std::collections::HashMap::new();
        blobs.insert("sha2".to_string(), bytes);
        let reader = MapReader { blobs };
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({"slug": "x/y", "pr": 1}),
                },
            ),
            ev(
                2,
                EventBody::DomainSpecific {
                    kind_suffix: "iteration_candidates".into(),
                    payload: serde_json::json!({
                        "iteration": 1,
                        "count": 2,
                        "blob": {"sha": "sha2", "size": 99, "ext": "json"},
                    }),
                },
            ),
        ];
        let snap = project_run(&evs, &reader, "r1").unwrap();
        let DomainView::Pr(view) = snap.domain_view else {
            panic!("expected Pr view");
        };
        assert_eq!(
            view.axes.ci.as_ref().unwrap().current_candidate.as_deref(),
            Some("ReRunCi")
        );
        assert_eq!(
            view.axes
                .reviews
                .as_ref()
                .unwrap()
                .current_candidate
                .as_deref(),
            Some("RequestReviews")
        );
    }

    #[test]
    fn iteration_blob_refs_accumulate_observed_and_oriented() {
        let evs = vec![
            ev(
                1,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                2,
                EventBody::IterationObserved {
                    iteration: 1,
                    blob: blob("aa", "json"),
                },
            ),
            ev(
                3,
                EventBody::IterationOriented {
                    iteration: 1,
                    blob: blob("bb", "json"),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        let kinds: Vec<&str> = snap.iterations[0]
            .blob_refs
            .iter()
            .map(|b| b.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["observed", "oriented"]);
    }

    #[test]
    fn latest_event_at_advances_with_each_event() {
        let evs = vec![
            ev(
                10,
                EventBody::RunStarted {
                    domain: "pr".into(),
                    target: serde_json::json!({}),
                },
            ),
            ev(
                20,
                EventBody::IterationObserved {
                    iteration: 1,
                    blob: blob("aa", "json"),
                },
            ),
        ];
        let snap = project_run(&evs, &NullBlobReader, "r1").unwrap();
        assert_eq!(snap.started_at, ts(10));
        assert_eq!(snap.latest_event_at, ts(20));
    }
}
