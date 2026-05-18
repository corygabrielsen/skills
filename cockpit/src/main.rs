//! Cockpit daemon — HTTP + SSE surface over the OODA state tree.
//!
//! # Role
//!
//! Background companion to the OODA agent family. Serves a small
//! axum HTTP surface on localhost, including `/api/events`: a
//! Server-Sent-Events stream that fans out parsed `ooda_state::Event`
//! records as they're appended to any active run's `events.jsonl`.
//!
//! The daemon is observational. The control plane (POST endpoints,
//! WebSocket) is future work. See
//! `~/.claude/projects/-home-cory-code-skills/memory/project-cockpit-design.md`.
//!
//! # Watcher scope
//!
//! Cockpit runs an `inotify` watch on `<state-root>/live/` only.
//! That directory has small cardinality (a handful of active runs at
//! a time); the per-run `events.jsonl` files are tailed by a small
//! polling task spawned on `Create` and cancelled on `Remove`.
//! Watching the entire state tree (`runs/` includes one directory
//! per historical run) would cost ~300k inotify handles on a mature
//! machine — that's why scope is `live/` only.
//!
//! # Invariants
//!
//! - **Read-only**: Cockpit never writes to the state tree.
//! - **Bind defaults are platform-aware**: WSL2 forwards `127.0.0.1`
//!   to the Windows host inconsistently, so the default bind is
//!   `0.0.0.0` under WSL2 and `127.0.0.1` everywhere else.
//!   `--bind ADDR` overrides.
//! - **Fail open on disconnect**: SSE clients can lag, disconnect,
//!   and reconnect; the daemon never blocks on a slow client. The
//!   broadcast channel drops the oldest event for laggards.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, SeekFrom};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use futures_util::stream::{self, Stream, StreamExt as FuturesStreamExt};
use notify::{EventKind, RecursiveMode, Watcher};
use ooda_core::ExitCode;
use ooda_projection::{BlobReader, ProjectedEvent, RunSnapshot, project_run};
use ooda_state::{BlobRef, Event as OodaEvent, RunId, StateRoot};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

const DEFAULT_PORT: u16 = 7777;

/// Broadcast channel capacity. Slow clients lose the oldest events
/// when the buffer fills; the SSE handler logs the lag and
/// reconnect-from-disk is not implemented (the broadcast feed is
/// "live tail", not durable history — durable history is on disk).
const BROADCAST_CAPACITY: usize = 1024;

/// Bounded capacity for the notify→async bridge. Notify is a sync
/// callback; the receiver is an async task. Disk is authoritative —
/// a dropped notify event delays tail-start by one poll at worst.
const NOTIFY_CHANNEL_CAPACITY: usize = 4096;

/// Per-run projection-broadcast capacity. SSE clients tailing a
/// specific run subscribe here; lagged clients drop deltas (full
/// snapshot is on disk via `GET /api/runs/<id>`).
const PER_RUN_BROADCAST_CAPACITY: usize = 256;

/// Cap on the per-tail `partial` buffer (1 MiB). If a writer never
/// emits a newline (writer-protocol bug), the partial buffer would
/// grow unbounded. On overflow the buffer is drained to the next
/// newline and a warning is logged.
const PARTIAL_BUFFER_CAP: usize = 1024 * 1024;

/// Initial backoff for restart loops; doubles per failure to a
/// per-loop ceiling.
const RESTART_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const RESTART_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often the per-run tail task wakes to read new bytes. The
/// `live/` watcher is event-driven, but each `events.jsonl` is
/// polled because adding a second inotify watch per active run is
/// not worth the cost given typical agent cadence (~1 event/sec).
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    state_root: PathBuf,
    tx: broadcast::Sender<StreamedEvent>,
    metrics: Arc<Metrics>,
    /// Per-run projection registry. Snapshots are computed lazily on
    /// first request and updated by the live tail; the broadcast
    /// channel fans projected deltas out to SSE subscribers tailing
    /// that specific run.
    runs: Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
    /// HTTP port the daemon is bound to. Used by the Origin check on
    /// write endpoints to allowlist `http://localhost:<port>` and
    /// `http://127.0.0.1:<port>` and reject every other Origin header.
    port: u16,
}

/// One per-run projection slot. `snapshot` is the latest aggregated
/// view; `tx` broadcasts [`ProjectedEvent`] deltas to subscribers of
/// `GET /api/runs/<id>/events`.
#[derive(Debug)]
struct ProjectionEntry {
    snapshot: Mutex<RunSnapshot>,
    tx: broadcast::Sender<ProjectedEvent>,
}

/// Process-lifetime observability counters surfaced via `/api/health`.
#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
struct Metrics {
    /// Total broadcast events dropped due to subscriber `Lagged(n)`.
    lagged_total: AtomicU64,
    /// Total notify events dropped because the bounded bridge was
    /// full (operator should see this if the watcher is overrun).
    notify_dropped_total: AtomicU64,
    /// Total times a tail's `partial` buffer hit `PARTIAL_BUFFER_CAP`
    /// and was force-drained to the next newline.
    partial_overflow_total: AtomicU64,
    /// Total times a tail detected `events.jsonl` shrinking (writer-
    /// contract violation) and reset its offset.
    truncations_total: AtomicU64,
}

#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
    state_root: String,
    version: &'static str,
    lagged_total: u64,
    notify_dropped_total: u64,
    partial_overflow_total: u64,
    truncations_total: u64,
}

/// SSE wire shape: the parsed `ooda_state::Event` plus the run id
/// the event belongs to. The frontend routes events per-run via
/// `run_id`; `event` carries the typed body and timestamp verbatim.
#[derive(Debug, Clone, Serialize)]
struct StreamedEvent {
    run_id: String,
    #[serde(flatten)]
    event: OodaEvent,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cockpit=info,tower_http=info".into()),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(CliError::Usage(msg)) => {
            eprintln!("cockpit: {msg}");
            return ExitCode::UsageError.into();
        }
        Err(CliError::Other(e)) => {
            eprintln!("cockpit: {e}");
            return ExitCode::BinaryError.into();
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cockpit: tokio runtime: {e}");
            return ExitCode::BinaryError.into();
        }
    };

    match runtime.block_on(serve(args)) {
        Ok(()) => ExitCode::DoneSucceeded.into(),
        Err(e) => {
            eprintln!("cockpit: {e}");
            ExitCode::BinaryError.into()
        }
    }
}

async fn serve(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let state_root_path = ooda_state::resolve_state_root(args.state_root.as_deref());
    let state_root = StateRoot::new(&state_root_path)?;
    let bind_ip = args.bind.unwrap_or_else(default_bind_ip);
    let addr = SocketAddr::new(bind_ip, args.port);

    if is_wsl() {
        tracing::info!(
            ?addr,
            ?state_root_path,
            "cockpit starting (WSL2 detected — default bind is 0.0.0.0 so Windows browsers can reach the daemon)",
        );
    } else {
        tracing::info!(?addr, ?state_root_path, "cockpit starting");
    }

    let metrics = Arc::new(Metrics::default());
    // Best-effort: reclaim disk for live markers whose writer is
    // dead. PID-liveness is parsed from the run-id suffix; non-
    // generated ids are left alone (conservative).
    match state_root.sweep_dead_markers() {
        Ok(swept) if !swept.is_empty() => {
            tracing::info!(count = swept.len(), "swept dead live markers");
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(%err, "sweep_dead_markers failed"),
    }

    let (tx, _rx) = broadcast::channel::<StreamedEvent>(BROADCAST_CAPACITY);
    let runs: Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    spawn_live_watcher(
        state_root.clone(),
        tx.clone(),
        Arc::clone(&metrics),
        Arc::clone(&runs),
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/assets/style.css", get(style_css))
        .route("/assets/app.js", get(app_js))
        .route("/api/health", get(health))
        .route("/api/events", get(events_sse))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/:run_id", get(get_run))
        .route("/api/runs/:run_id/events", get(run_events_sse))
        .route("/api/runs/:run_id/blobs/:sha", get(get_blob))
        .route("/api/runs/:run_id/halt", post(halt_run))
        .with_state(AppState {
            state_root: state_root_path.clone(),
            tx,
            metrics,
            runs,
            port: args.port,
        });

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{}", addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("cockpit shutdown complete");
    Ok(())
}

// ── HTTP handlers ────────────────────────────────────────────────────

async fn index() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/app.js"),
    )
}

async fn health(State(app): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        state_root: app.state_root.display().to_string(),
        version: env!("CARGO_PKG_VERSION"),
        lagged_total: app.metrics.lagged_total.load(Ordering::Relaxed),
        notify_dropped_total: app.metrics.notify_dropped_total.load(Ordering::Relaxed),
        partial_overflow_total: app.metrics.partial_overflow_total.load(Ordering::Relaxed),
        truncations_total: app.metrics.truncations_total.load(Ordering::Relaxed),
    })
}

async fn events_sse(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // BroadcastStream surfaces a `Lagged(n)` error when a slow
    // subscriber falls behind the channel capacity. Cockpit's
    // contract is "live tail, best effort" — log the lag, count
    // the dropped batch, and keep the SSE connection alive.
    let rx = app.tx.subscribe();
    let metrics = Arc::clone(&app.metrics);
    let stream = StreamExt::filter_map(BroadcastStream::new(rx), move |res| match res {
        Ok(ev) => match serde_json::to_string(&ev) {
            Ok(json) => Some(Ok(SseEvent::default().event("mutation").data(json))),
            Err(err) => {
                tracing::warn!(%err, "dropping event: serialize failed");
                None
            }
        },
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            metrics.lagged_total.fetch_add(n, Ordering::Relaxed);
            tracing::warn!(dropped = n, "broadcast subscriber lagged; dropping batch");
            None
        }
    });
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// ── Projection HTTP handlers ─────────────────────────────────────────

/// Compact summary returned by `GET /api/runs`. The full snapshot
/// (iterations, axes, blob refs) ships only via `GET /api/runs/<id>`.
#[derive(Debug, Serialize)]
struct RunSummary {
    run_id: String,
    domain: String,
    target: serde_json::Value,
    status: ooda_projection::RunStatus,
    started_at: DateTime<Utc>,
    latest_event_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ListRunsParams {
    /// `"active"` (default) → only runs with no terminal event.
    /// `"all"` → every run on disk.
    #[serde(default)]
    status: Option<String>,
    /// Cap on results (default 50). Sorted by `latest_event_at` desc
    /// before truncation.
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_LIST_LIMIT: usize = 50;

async fn list_runs(State(app): State<AppState>, Query(params): Query<ListRunsParams>) -> Response {
    let status_filter = params.status.as_deref().unwrap_or("active");
    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let state_root = match StateRoot::new(&app.state_root) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let ids = match status_filter {
        "active" => match state_root.live_runs() {
            Ok(ids) => ids,
            Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        },
        "all" => match all_run_ids(&app.state_root).await {
            Ok(ids) => ids,
            Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        },
        other => {
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("unknown status filter: {other}"),
            );
        }
    };

    let mut summaries: Vec<RunSummary> = Vec::new();
    for id in ids {
        match load_or_get_snapshot(&app, &state_root, &id).await {
            Ok(snap) => {
                let snap = snap.snapshot.lock().await;
                summaries.push(RunSummary {
                    run_id: snap.run_id.clone(),
                    domain: snap.domain.clone(),
                    target: snap.target.clone(),
                    status: snap.status,
                    started_at: snap.started_at,
                    latest_event_at: snap.latest_event_at,
                    outcome_kind: snap.outcome.as_ref().map(|o| o.kind.clone()),
                    exit_code: snap.outcome.as_ref().map(|o| o.exit_code),
                });
            }
            Err(err) => {
                tracing::debug!(run_id = %id, %err, "skipping run: project failed");
            }
        }
    }
    summaries.sort_by_key(|s| std::cmp::Reverse(s.latest_event_at));
    summaries.truncate(limit);
    Json(summaries).into_response()
}

async fn get_run(State(app): State<AppState>, AxumPath(run_id): AxumPath<String>) -> Response {
    let state_root = match StateRoot::new(&app.state_root) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let id = match RunId::new(run_id) {
        Ok(id) => id,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    match load_or_get_snapshot(&app, &state_root, &id).await {
        Ok(entry) => {
            let snap = entry.snapshot.lock().await.clone();
            Json(snap).into_response()
        }
        Err(SnapshotError::UnknownRun) => error_json(StatusCode::NOT_FOUND, "unknown run"),
        Err(SnapshotError::Projection(err)) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string())
        }
        Err(SnapshotError::Io(err)) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string())
        }
    }
}

async fn run_events_sse(
    State(app): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    let state_root = match StateRoot::new(&app.state_root) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let id = match RunId::new(run_id) {
        Ok(id) => id,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let entry = match load_or_get_snapshot(&app, &state_root, &id).await {
        Ok(entry) => entry,
        Err(SnapshotError::UnknownRun) => {
            return error_json(StatusCode::NOT_FOUND, "unknown run");
        }
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let snapshot = entry.snapshot.lock().await.clone();
    let metrics = Arc::clone(&app.metrics);
    let rx = entry.tx.subscribe();
    // Snapshot (backfill) first; live deltas after — single stream
    // so the client's SSE state machine never sees an out-of-order
    // surprise.
    let backfill = stream::once(async move {
        let snap_event = ProjectedEvent::Snapshot(Box::new(snapshot));
        serialize_projected(&snap_event)
    });
    let live = FuturesStreamExt::filter_map(BroadcastStream::new(rx), move |res| {
        let metrics = Arc::clone(&metrics);
        async move {
            match res {
                Ok(ev) => Some(serialize_projected(&ev)),
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    metrics.lagged_total.fetch_add(n, Ordering::Relaxed);
                    tracing::warn!(dropped = n, "projected subscriber lagged");
                    None
                }
            }
        }
    });
    let stream = FuturesStreamExt::filter_map(
        FuturesStreamExt::chain(backfill, live),
        |opt: Option<SseEvent>| async move { opt.map(Ok::<_, Infallible>) },
    );
    let sse = Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    );
    sse.into_response()
}

fn serialize_projected(event: &ProjectedEvent) -> Option<SseEvent> {
    match serde_json::to_string(event) {
        Ok(json) => Some(SseEvent::default().event("projected").data(json)),
        Err(err) => {
            tracing::warn!(%err, "dropping projected event: serialize failed");
            None
        }
    }
}

async fn get_blob(
    State(app): State<AppState>,
    AxumPath((run_id, sha)): AxumPath<(String, String)>,
) -> Response {
    let state_root = match StateRoot::new(&app.state_root) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let id = match RunId::new(run_id) {
        Ok(id) => id,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    // Locate the on-disk blob path. The blobs/ directory stores files
    // as `<sha>.<ext>`; cockpit accepts a bare sha and resolves the
    // single matching file. Multiple ext matches return 500
    // (corruption — blobs are content-addressed and unique).
    let blobs_dir = app.state_root.join("runs").join(id.as_str()).join("blobs");
    let resolved = match resolve_blob_path(&blobs_dir, &sha).await {
        Ok(p) => p,
        Err(BlobLookupError::NotFound) => {
            return error_json(StatusCode::NOT_FOUND, "blob not found");
        }
        Err(BlobLookupError::Ambiguous) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "multiple blobs match sha (corruption)",
            );
        }
        Err(BlobLookupError::Io(err)) => {
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
    };
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    let meta = match tokio::fs::metadata(&resolved).await {
        Ok(m) => m,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // Route through RunReader::read_blob so the content-addressed
    // hash check is performed once, by the layer that owns the
    // invariant. A mismatch surfaces as 500 (corruption: the on-disk
    // file no longer hashes to its filename).
    let reader = match state_root.open_run(id.clone()) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let blob_ref = BlobRef {
        sha: sha.clone(),
        size: meta.len(),
        ext: ext.clone(),
    };
    let bytes = match reader.read_blob(&blob_ref) {
        Ok(b) => b,
        Err(ooda_state::StateError::BlobHashMismatch { .. }) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "blob hash mismatch (corruption)",
            );
        }
        Err(ooda_state::StateError::BlobTooLarge { .. }) => {
            return error_json(StatusCode::PAYLOAD_TOO_LARGE, "blob exceeds inline cap");
        }
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // Hand back the raw bytes with a content-type derived from the
    // file extension. The set covers what the PR-side recorders emit
    // today (md, json, txt). Unknown extensions fall back to
    // application/octet-stream so the browser downloads rather than
    // tries to render.
    let content_type = content_type_for_ext(&ext);
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    if let Ok(value) = HeaderValue::from_str(content_type) {
        resp.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    resp
}

// ── Halt endpoint ────────────────────────────────────────────────────

/// Request body for `POST /api/runs/<id>/halt`. The `reason` field is
/// surfaced in cockpit logs only; it does not (today) reach the on-disk
/// audit trail. Body is optional — a missing or empty body yields the
/// default reason.
#[derive(Debug, Default, Deserialize)]
struct HaltRequest {
    #[serde(default)]
    reason: Option<String>,
}

/// Response body for a successful halt. Carries the resolved PID + run
/// id so the caller has the same key it would observe via `/api/runs`.
#[derive(Debug, Serialize)]
struct HaltAccepted {
    status: &'static str,
    pid: u32,
    run_id: String,
    reason: String,
}

const DEFAULT_HALT_REASON: &str = "user requested via cockpit";

/// `POST /api/runs/<run-id>/halt` — send `SIGTERM` to the writer PID
/// embedded in the run id.
///
/// The endpoint deliberately performs no on-disk mutation. The
/// writer's `Drop` impl (and any future signal handler in ooda-state)
/// is responsible for releasing the live marker + appending the
/// terminal event; on a hard kill, `StateRoot::sweep_dead_markers`
/// reclaims the marker at the next reader/writer startup.
///
/// Response codes follow the V2.0 spec:
///
/// - 200 OK — signal queued; `{"status":"signalled", "pid":..., ...}`
/// - 404 Not Found — no live marker, or the recorded PID is already dead
/// - 409 Conflict — last event on disk is terminal
/// - 412 Precondition Failed — run id lacks the `-p<pid>` suffix
/// - 403 Forbidden — PID is alive but is not an OODA writer
/// - 400 Bad Request — run id rejected by `RunId::new`
///
/// PID-ownership check reads `/proc/<pid>/comm` on Linux and requires
/// the comm value to start with `ooda-`. On non-Linux Unix targets
/// (BSD, macOS) the check is skipped with a warning — `/proc/<pid>/comm`
/// is Linux-specific and the run-id collision risk between an OODA
/// run and an unrelated PID under the same numeric range is real but
/// low. On non-Unix targets the endpoint returns 412 (the syscall is
/// unsupported).
async fn halt_run(
    State(app): State<AppState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
    body: Option<Json<HaltRequest>>,
) -> Response {
    if let Err(resp) = check_origin(&headers, app.port) {
        return *resp;
    }
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let reason = req
        .reason
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HALT_REASON.to_string());

    let id = match RunId::new(run_id) {
        Ok(id) => id,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let Some(pid) = id.writer_pid() else {
        return error_json(
            StatusCode::PRECONDITION_FAILED,
            "run_id missing -p<pid> suffix; cannot derive target pid",
        );
    };

    let state_root = match StateRoot::new(&app.state_root) {
        Ok(r) => r,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let live_marker = app.state_root.join("live").join(id.as_str());
    if !live_marker.exists() {
        return error_json(StatusCode::NOT_FOUND, "no live marker for run_id");
    }

    match last_event_kind(&state_root, &id) {
        Ok(Some(kind)) if is_terminal_kind(&kind) => {
            return error_json(
                StatusCode::CONFLICT,
                &format!("run is terminal (last event: {kind})"),
            );
        }
        Ok(_) => {}
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }

    if !is_pid_alive(pid) {
        // Dead PID with a leftover marker — sweep best-effort so the
        // next reader/lister sees a clean state.
        if let Err(err) = state_root.sweep_dead_markers() {
            tracing::warn!(%err, "sweep_dead_markers after stale marker failed");
        }
        return error_json(StatusCode::NOT_FOUND, "writer pid is no longer alive");
    }

    match check_pid_ownership(pid) {
        PidOwnership::Owned => {}
        PidOwnership::Foreign => {
            return error_json(
                StatusCode::FORBIDDEN,
                &format!("pid {pid} exists but does not appear to be an OODA writer"),
            );
        }
        PidOwnership::Unverifiable => {
            tracing::warn!(
                pid,
                "skipping pid ownership check (unsupported platform); proceeding with SIGTERM"
            );
        }
    }

    match send_sigterm(pid) {
        Ok(()) => {
            tracing::info!(run_id = %id, pid, %reason, "halt: signalled");
            (
                StatusCode::OK,
                Json(HaltAccepted {
                    status: "signalled",
                    pid,
                    run_id: id.as_str().to_string(),
                    reason,
                }),
            )
                .into_response()
        }
        Err(SigError::Esrch) => {
            // Race: pid died between our liveness probe and kill(2).
            // Surface as 404 and let the caller observe via SSE.
            if let Err(err) = state_root.sweep_dead_markers() {
                tracing::warn!(%err, "sweep_dead_markers after ESRCH failed");
            }
            error_json(StatusCode::NOT_FOUND, "writer pid is no longer alive")
        }
        Err(SigError::Eperm) => error_json(
            StatusCode::FORBIDDEN,
            &format!("not permitted to signal pid {pid}"),
        ),
        Err(SigError::Other(rc)) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("kill(pid={pid}, SIGTERM) failed: errno {rc}"),
        ),
        Err(SigError::Unsupported) => error_json(
            StatusCode::PRECONDITION_FAILED,
            "halt unsupported on this platform",
        ),
    }
}

/// Enforce the origin allowlist on write endpoints. Curl + server-side
/// callers omit `Origin` entirely; browsers always set it. The check
/// exists to block cross-site browser POSTs from a rogue tab on a
/// different origin (CSRF defense-in-depth, NOT auth).
///
/// Returns the rejection [`Response`] boxed so the result is small —
/// `axum::Response` itself carries a heap-allocated body but is
/// stack-large enough to trip `clippy::result_large_err` on a bare
/// `Result<(), Response>`.
fn check_origin(headers: &HeaderMap, port: u16) -> Result<(), Box<Response>> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let Ok(value) = origin.to_str() else {
        return Err(Box::new(error_json(
            StatusCode::FORBIDDEN,
            "origin header not utf-8",
        )));
    };
    let allowed = [
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ];
    if allowed.iter().any(|a| a == value) {
        Ok(())
    } else {
        Err(Box::new(error_json(
            StatusCode::FORBIDDEN,
            "origin not allowed",
        )))
    }
}

/// Return the `kind` discriminant of the last event in
/// `runs/<id>/events.jsonl`, or `None` if the file is empty / missing.
/// Tolerates malformed trailing lines (writer mid-flight) by walking
/// backwards to the most recent well-formed line.
fn last_event_kind(state_root: &StateRoot, id: &RunId) -> Result<Option<String>, std::io::Error> {
    let reader = state_root
        .open_run(id.clone())
        .map_err(std::io::Error::other)?;
    let events = reader.events().map_err(std::io::Error::other)?;
    Ok(events
        .last()
        .map(|ev| event_body_kind(&ev.body).to_string()))
}

/// Map an [`ooda_state::EventBody`] variant to its on-disk
/// `kind` token. Mirrors the `#[serde(tag = "kind", rename_all =
/// "snake_case")]` projection — kept hand-rolled rather than
/// re-serializing so this stays O(1) per call and free of allocation
/// surprises.
fn event_body_kind(body: &ooda_state::EventBody) -> &'static str {
    use ooda_state::EventBody as B;
    match body {
        B::RunStarted { .. } => "run_started",
        B::IterationObserved { .. } => "iteration_observed",
        B::IterationOriented { .. } => "iteration_oriented",
        B::IterationDecided { .. } => "iteration_decided",
        B::IterationHandoff { .. } => "iteration_handoff",
        B::IterationExecuted { .. } => "iteration_executed",
        B::IterationWaited { .. } => "iteration_waited",
        B::RunHalted { .. } => "run_halted",
        B::RunStalled { .. } => "run_stalled",
        B::RunCapReached { .. } => "run_cap_reached",
        B::DomainSpecific { .. } => "domain_specific",
    }
}

/// Closed set of `kind` discriminants that terminate a run. Any of
/// these as the last event means the run is on-disk-final and a halt
/// POST is a 409.
fn is_terminal_kind(kind: &str) -> bool {
    matches!(kind, "run_halted" | "run_stalled" | "run_cap_reached")
}

/// PID-ownership check outcome.
#[derive(Debug)]
enum PidOwnership {
    /// `/proc/<pid>/comm` starts with `ooda-`.
    Owned,
    /// `/proc/<pid>/comm` was readable but did not start with `ooda-`.
    Foreign,
    /// Platform lacks a supported `/proc/<pid>/comm` (BSD, macOS,
    /// non-Unix). Caller logs and proceeds without the check.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Unverifiable,
}

#[cfg(target_os = "linux")]
fn check_pid_ownership(pid: u32) -> PidOwnership {
    let path = format!("/proc/{pid}/comm");
    match std::fs::read_to_string(&path) {
        Ok(comm) => {
            let trimmed = comm.trim();
            if trimmed.starts_with("ooda-") {
                PidOwnership::Owned
            } else {
                PidOwnership::Foreign
            }
        }
        Err(err) => {
            tracing::warn!(pid, %err, "proc comm read failed; treating as foreign");
            PidOwnership::Foreign
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn check_pid_ownership(_pid: u32) -> PidOwnership {
    PidOwnership::Unverifiable
}

#[derive(Debug)]
enum SigError {
    /// `ESRCH` — pid does not exist (race vs. our liveness probe).
    Esrch,
    /// `EPERM` — pid exists but we lack permission to signal it.
    Eperm,
    /// Any other syscall error; `i32` is `raw_os_error`.
    Other(i32),
    /// Non-Unix build target.
    #[cfg_attr(unix, allow(dead_code))]
    Unsupported,
}

/// POSIX `kill(pid, SIGTERM)`. SIGTERM is the polite stop signal; the
/// target process is expected (today or eventually) to install a
/// handler that releases its live marker + appends a halt event.
#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), SigError> {
    if pid == 0 {
        // kill(0, …) addresses every process in the caller's group.
        // Refuse: never a valid writer pid.
        return Err(SigError::Esrch);
    }
    let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
    // SAFETY: `libc::kill(pid, SIGTERM)` is a synchronous syscall
    // with no aliasing or memory contract; the only side effect is
    // signal delivery to `pid`.
    let rc = unsafe { libc_kill_signal(pid_i32, SIGTERM) };
    if rc == 0 {
        return Ok(());
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(3) => Err(SigError::Esrch),
        Some(1) => Err(SigError::Eperm),
        Some(other) => Err(SigError::Other(other)),
        None => Err(SigError::Other(-1)),
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) -> Result<(), SigError> {
    Err(SigError::Unsupported)
}

#[cfg(unix)]
const SIGTERM: i32 = 15;

#[cfg(unix)]
unsafe extern "C" {
    // Distinct symbol from the `libc_kill` signal-0 probe earlier in
    // this file so each call site reads independently; both bind to
    // the same C `kill(2)` regardless.
    #[link_name = "kill"]
    fn libc_kill_signal(pid: i32, sig: i32) -> i32;
}

fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "md" => "text/markdown; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

enum BlobLookupError {
    NotFound,
    Ambiguous,
    Io(io::Error),
}

impl std::fmt::Display for BlobLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("blob not found"),
            Self::Ambiguous => f.write_str("multiple blobs match sha"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

async fn resolve_blob_path(blobs_dir: &Path, sha: &str) -> Result<PathBuf, BlobLookupError> {
    // sha hardening: the on-disk filenames use lowercase hex; reject
    // anything else early so we never iterate the dir for invalid
    // input. Length 64 = SHA-256.
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(BlobLookupError::NotFound);
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut entries = match tokio::fs::read_dir(blobs_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(BlobLookupError::NotFound),
        Err(e) => return Err(BlobLookupError::Io(e)),
    };
    while let Some(entry) = entries.next_entry().await.map_err(BlobLookupError::Io)? {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if let Some((stem, _ext)) = name_str.rsplit_once('.')
            && stem == sha
        {
            matches.push(entry.path());
        }
    }
    match matches.len() {
        0 => Err(BlobLookupError::NotFound),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => Err(BlobLookupError::Ambiguous),
    }
}

#[derive(Debug)]
enum SnapshotError {
    UnknownRun,
    Projection(ooda_projection::ProjectionError),
    Io(ooda_state::StateError),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun => f.write_str("unknown run"),
            Self::Projection(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<ooda_state::StateError> for SnapshotError {
    fn from(err: ooda_state::StateError) -> Self {
        match err {
            ooda_state::StateError::UnknownRun(_) => Self::UnknownRun,
            other => Self::Io(other),
        }
    }
}

/// Fetch the registry entry for `id`, projecting on first touch. The
/// registry is the source of truth for the live projection; the disk
/// is the source of truth for events.
async fn load_or_get_snapshot(
    app: &AppState,
    state_root: &StateRoot,
    id: &RunId,
) -> Result<Arc<ProjectionEntry>, SnapshotError> {
    {
        let guard = app.runs.lock().await;
        if let Some(entry) = guard.get(id) {
            return Ok(Arc::clone(entry));
        }
    }
    let reader = state_root.open_run(id.clone())?;
    let events = reader.events()?;
    let snapshot =
        project_run(&events, &reader as &dyn BlobReader, id.as_str()).map_err(|err| {
            // An empty events.jsonl is treated as "run not found yet"
            // — the writer claimed the dir but hasn't appended yet.
            match err {
                ooda_projection::ProjectionError::MissingRunStarted if events.is_empty() => {
                    SnapshotError::UnknownRun
                }
                err @ ooda_projection::ProjectionError::MissingRunStarted => {
                    SnapshotError::Projection(err)
                }
            }
        })?;
    let (tx, _rx) = broadcast::channel::<ProjectedEvent>(PER_RUN_BROADCAST_CAPACITY);
    let entry = Arc::new(ProjectionEntry {
        snapshot: Mutex::new(snapshot),
        tx,
    });
    let mut guard = app.runs.lock().await;
    // Lost-race recovery: another task may have populated the entry
    // while we were projecting. Prefer the existing entry to keep
    // subscribers' broadcast channels stable.
    Ok(Arc::clone(guard.entry(id.clone()).or_insert_with(|| entry)))
}

/// Recompute and broadcast a fresh snapshot for `run_id`. Called by
/// the tail loop after at least one new event was forwarded; cheap
/// at typical run sizes (sub-ms project, sub-KB broadcast).
async fn update_projection(
    state_root: &StateRoot,
    runs: &Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
    run_id: &RunId,
) {
    let entry = {
        let guard = runs.lock().await;
        guard.get(run_id).map(Arc::clone)
    };
    let Some(entry) = entry else {
        // No subscriber has touched this run yet; nothing to update.
        // The snapshot will be projected lazily on first request.
        return;
    };
    let reader = match state_root.open_run(run_id.clone()) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(run_id = %run_id, %err, "re-project: open_run failed");
            return;
        }
    };
    let events = match reader.events() {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(run_id = %run_id, %err, "re-project: events read failed");
            return;
        }
    };
    let snapshot = match project_run(&events, &reader as &dyn BlobReader, run_id.as_str()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(run_id = %run_id, %err, "re-project: projection failed");
            return;
        }
    };
    {
        let mut guard = entry.snapshot.lock().await;
        *guard = snapshot.clone();
    }
    // Send Snapshot as the delta for now. A future revision can emit
    // narrower deltas (IterationUpdate, StatusChange) by diffing the
    // previous snapshot against the new one. For V1.2, a full
    // snapshot per delta is well within wire/CPU budgets at typical
    // run sizes.
    let _ = entry.tx.send(ProjectedEvent::Snapshot(Box::new(snapshot)));
}

/// Enumerate every run id under `runs/` regardless of liveness.
/// Hidden / malformed entries are silently skipped.
async fn all_run_ids(state_root: &Path) -> io::Result<Vec<RunId>> {
    let runs_dir = state_root.join("runs");
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(&runs_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Ok(id) = RunId::new(name) {
            out.push(id);
        }
    }
    Ok(out)
}

fn error_json(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message });
    (status, Json(body)).into_response()
}

// ── Live watcher ─────────────────────────────────────────────────────

/// Generation-tagged entry in the `tails` map. The generation is
/// assigned at spawn time and consulted at self-cleanup so a stale
/// task cannot delete a fresh entry that replaced it (Remove →
/// Create coalescence race).
#[derive(Debug)]
struct TailEntry {
    generation: u64,
    cancel: oneshot::Sender<()>,
}

type Tails = Arc<Mutex<HashMap<RunId, TailEntry>>>;

/// Spawn the `live/` watcher in a restart loop. The watcher runs
/// for the lifetime of the daemon; a notify-backend death or IO
/// error logs and re-creates the watcher with exponential backoff
/// rather than leaving cockpit deaf.
fn spawn_live_watcher(
    state_root: StateRoot,
    tx: broadcast::Sender<StreamedEvent>,
    metrics: Arc<Metrics>,
    runs: Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
) {
    tokio::spawn(async move {
        let tails: Tails = Arc::new(Mutex::new(HashMap::new()));
        let generation = Arc::new(AtomicU64::new(0));
        run_with_restart("live_watcher", move || {
            let state_root = state_root.clone();
            let tx = tx.clone();
            let tails = Arc::clone(&tails);
            let metrics = Arc::clone(&metrics);
            let generation = Arc::clone(&generation);
            let runs = Arc::clone(&runs);
            async move {
                run_live_watcher_once(state_root, tx, tails, metrics, generation, runs).await
            }
        })
        .await;
    });
}

/// Restart-with-backoff helper. Calls `make_fut` in a loop; on
/// `Err` logs and sleeps with exponentially-increasing backoff
/// (capped at [`RESTART_MAX_BACKOFF`]); on `Ok` resets the backoff.
async fn run_with_restart<F, Fut>(name: &'static str, mut make_fut: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>,
{
    let mut backoff = RESTART_INITIAL_BACKOFF;
    loop {
        match make_fut().await {
            Ok(()) => {
                backoff = RESTART_INITIAL_BACKOFF;
            }
            Err(err) => {
                tracing::error!(task = name, %err, ?backoff, "restarting after error");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RESTART_MAX_BACKOFF);
            }
        }
    }
}

async fn run_live_watcher_once(
    state_root: StateRoot,
    tx: broadcast::Sender<StreamedEvent>,
    tails: Tails,
    metrics: Arc<Metrics>,
    generation: Arc<AtomicU64>,
    runs: Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let live_dir = state_root.path().join("live");
    // StateRoot::new created `live/` if missing; tolerate the race
    // where it was removed between construction and watcher startup.
    tokio::fs::create_dir_all(&live_dir).await?;

    // Pre-tail every already-live run. `live_runs()` filters out
    // markers whose writer PID is dead; the tail begins at the
    // file's current EOF so a fresh subscriber doesn't get flooded
    // with replayed history (broadcast subscribers connect after
    // this point and only see appends from now on).
    for id in state_root.live_runs()? {
        start_tail(
            &state_root,
            &tx,
            &tails,
            &metrics,
            &generation,
            &runs,
            id,
            StartOffset::Eof,
        )
        .await;
    }

    let (notify_tx, mut notify_rx) = mpsc::channel::<notify::Event>(NOTIFY_CHANNEL_CAPACITY);
    let metrics_for_cb = Arc::clone(&metrics);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) => {
                // Bounded channel; on full, drop and count. Disk is
                // authoritative — a missed Create just delays
                // tail-start by one poll on the next event.
                if notify_tx.try_send(ev).is_err() {
                    metrics_for_cb
                        .notify_dropped_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(err) => tracing::warn!(%err, "live watcher event error"),
        }
    })?;
    watcher.watch(&live_dir, RecursiveMode::NonRecursive)?;

    while let Some(ev) = notify_rx.recv().await {
        match ev.kind {
            EventKind::Create(_) => {
                for path in ev.paths {
                    if let Some(id) = run_id_from_marker(&path) {
                        // Skip start_tail if the marker's PID is
                        // already dead (rare: marker created by a
                        // process that crashed milliseconds later).
                        if let Some(pid) = id.writer_pid()
                            && !is_pid_alive(pid)
                        {
                            tracing::debug!(run_id = %id, pid, "skipping tail: pid dead");
                            continue;
                        }
                        start_tail(
                            &state_root,
                            &tx,
                            &tails,
                            &metrics,
                            &generation,
                            &runs,
                            id,
                            StartOffset::Beginning,
                        )
                        .await;
                    }
                }
            }
            EventKind::Remove(_) => {
                for path in ev.paths {
                    if let Some(id) = run_id_from_marker(&path) {
                        stop_tail(&tails, &id).await;
                    }
                }
            }
            _ => {}
        }
    }

    // Channel closed: watcher dropped. Returning Ok lets the
    // restart loop re-establish the watcher.
    drop(watcher);
    Ok(())
}

/// POSIX `kill(pid, 0)` liveness probe (mirrors the helper in
/// `ooda-state`). Used to skip tailing markers whose writer is
/// already gone at the moment we observe their Create.
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

/// Initial offset for a freshly-spawned tail task.
#[derive(Copy, Clone, Debug)]
enum StartOffset {
    /// Read from offset 0 — used when responding to a Create event
    /// (likely a brand-new run with little history).
    Beginning,
    /// Seek to EOF — used for pre-startup pre-tail to avoid
    /// flooding the broadcast channel with replayed historical
    /// events that no live subscriber asked for.
    Eof,
}

/// Extract a `RunId` from the basename of a `live/<run-id>` marker
/// path. Returns `None` if the basename is missing or rejected by
/// `RunId::new` (e.g. hidden files, path traversal).
fn run_id_from_marker(path: &Path) -> Option<RunId> {
    let name = path.file_name()?.to_str()?;
    RunId::new(name).ok()
}

#[allow(clippy::too_many_arguments)]
async fn start_tail(
    state_root: &StateRoot,
    tx: &broadcast::Sender<StreamedEvent>,
    tails: &Tails,
    metrics: &Arc<Metrics>,
    generation: &Arc<AtomicU64>,
    runs: &Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
    id: RunId,
    start: StartOffset,
) {
    let mut guard = tails.lock().await;
    if guard.contains_key(&id) {
        return;
    }
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let gen_id = generation.fetch_add(1, Ordering::Relaxed);
    guard.insert(
        id.clone(),
        TailEntry {
            generation: gen_id,
            cancel: cancel_tx,
        },
    );
    drop(guard);

    let events_path = state_root
        .path()
        .join("runs")
        .join(id.as_str())
        .join("events.jsonl");
    let tx = tx.clone();
    let tails = Arc::clone(tails);
    let metrics = Arc::clone(metrics);
    let runs = Arc::clone(runs);
    let state_root_owned = state_root.clone();
    let id_for_task = id.clone();
    tokio::spawn(async move {
        run_tail_with_restart(
            events_path,
            id_for_task.clone(),
            tx,
            cancel_rx,
            start,
            Arc::clone(&metrics),
            state_root_owned,
            Arc::clone(&runs),
        )
        .await;
        // Self-cleanup: only delete OUR entry. A Remove → Create
        // coalescence may have replaced us with a fresh tail at a
        // higher generation; deleting that would leave a zombie
        // tail unreachable from stop_tail.
        let mut guard = tails.lock().await;
        if let Some(entry) = guard.get(&id_for_task)
            && entry.generation == gen_id
        {
            guard.remove(&id_for_task);
        }
    });
}

async fn stop_tail(tails: &Tails, id: &RunId) {
    if let Some(entry) = tails.lock().await.remove(id) {
        let _ = entry.cancel.send(());
    }
}

/// Drive `tail_events_once` in a restart loop until the tail
/// completes cleanly (cancellation) or the broadcast channel is
/// gone. Per-iteration IO failures (transient EIO, `NotFound`) log
/// and retry with backoff; this is the resilience hook that keeps
/// a single bad read from killing the tail forever.
///
/// The poll carries a byte-level partial buffer (not a `String`)
/// so a UTF-8 codepoint or PIPE_BUF-sized event split across two
/// poll windows survives reassembly. A complete byte-line that
/// fails UTF-8 decoding is logged and skipped; a trailing run of
/// bytes without a terminating `\n` is held in `partial` for the
/// next pass.
#[allow(clippy::too_many_arguments)]
async fn run_tail_with_restart(
    events_path: PathBuf,
    run_id: RunId,
    tx: broadcast::Sender<StreamedEvent>,
    mut cancel_rx: oneshot::Receiver<()>,
    start: StartOffset,
    metrics: Arc<Metrics>,
    state_root: StateRoot,
    runs: Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
) {
    let mut backoff = RESTART_INITIAL_BACKOFF;
    let mut offset: u64 = match start {
        StartOffset::Beginning => 0,
        StartOffset::Eof => initial_eof_offset(&events_path).await,
    };
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let result = tail_events_once(
            &events_path,
            &run_id,
            &tx,
            &mut cancel_rx,
            &mut offset,
            &mut partial,
            &metrics,
            &state_root,
            &runs,
        )
        .await;
        match result {
            TailStep::Cancelled | TailStep::SubscribersGone => return,
            TailStep::Idle => {
                backoff = RESTART_INITIAL_BACKOFF;
            }
            TailStep::TransientError(err) => {
                tracing::warn!(run_id = %run_id, %err, ?backoff, "tail transient error; backing off");
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(RESTART_MAX_BACKOFF);
            }
        }
    }
}

/// Snapshot `events.jsonl` length at startup so [`StartOffset::Eof`]
/// tails skip historical events. A missing file → offset 0
/// (subsequent appends will be picked up).
async fn initial_eof_offset(events_path: &Path) -> u64 {
    match tokio::fs::metadata(events_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

#[derive(Debug)]
enum TailStep {
    /// `cancel_rx` fired; tail should exit cleanly.
    Cancelled,
    /// Broadcast channel has no subscribers AND no live `Sender`
    /// references besides ours — tail can exit (history is on disk
    /// for any future history endpoint).
    SubscribersGone,
    /// One poll cycle completed without error.
    Idle,
    /// Transient IO error; outer loop should sleep and retry.
    TransientError(io::Error),
}

/// Single-poll tail body. Forwards new bytes from `events_path`
/// into `tx`, carrying the trailing partial line across calls in
/// `partial`. Returns a [`TailStep`] for the outer restart loop.
#[allow(clippy::too_many_arguments)]
async fn tail_events_once(
    events_path: &Path,
    run_id: &RunId,
    tx: &broadcast::Sender<StreamedEvent>,
    cancel_rx: &mut oneshot::Receiver<()>,
    offset: &mut u64,
    partial: &mut Vec<u8>,
    metrics: &Arc<Metrics>,
    state_root: &StateRoot,
    runs: &Arc<Mutex<HashMap<RunId, Arc<ProjectionEntry>>>>,
) -> TailStep {
    tokio::select! {
        biased;
        _ = &mut *cancel_rx => return TailStep::Cancelled,
        () = tokio::time::sleep(TAIL_POLL_INTERVAL) => {}
    }

    let mut file = match tokio::fs::File::open(events_path).await {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return TailStep::Idle,
        Err(e) => return TailStep::TransientError(e),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return TailStep::TransientError(e),
    };
    if len < *offset {
        // Truncation: events.jsonl is supposed to be append-only.
        // Reset offset + partial so we don't silently skip the
        // re-grown bytes.
        tracing::warn!(
            run_id = %run_id,
            prior_offset = *offset,
            new_len = len,
            "events.jsonl shrank; resetting tail",
        );
        metrics.truncations_total.fetch_add(1, Ordering::Relaxed);
        *offset = 0;
        partial.clear();
    }
    if len == *offset {
        return TailStep::Idle;
    }
    if let Err(e) = file.seek(SeekFrom::Start(*offset)).await {
        return TailStep::TransientError(e);
    }
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let read = match reader.read_to_end(&mut buf).await {
        Ok(n) => n,
        Err(e) => return TailStep::TransientError(e),
    };
    *offset += read as u64;
    partial.extend_from_slice(&buf);
    if partial.len() > PARTIAL_BUFFER_CAP {
        // Writer never emitted a newline within 1 MiB — drop bytes
        // up to the most recent newline so we resynchronize on the
        // next valid line boundary. If no newline exists, clear the
        // whole buffer.
        metrics
            .partial_overflow_total
            .fetch_add(1, Ordering::Relaxed);
        let drop_to = partial
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        tracing::warn!(
            run_id = %run_id,
            dropped_bytes = drop_to,
            "tail partial buffer overflowed PARTIAL_BUFFER_CAP; resyncing to next newline",
        );
        if drop_to == 0 {
            partial.clear();
        } else {
            partial.drain(..drop_to);
        }
    }
    let outcome = emit_complete_lines(partial, run_id, tx);
    if outcome.events_appended {
        update_projection(state_root, runs, run_id).await;
    }
    if !outcome.subscribers_alive {
        return TailStep::SubscribersGone;
    }
    TailStep::Idle
}

/// Result of one `emit_complete_lines` pass.
#[derive(Debug, Clone, Copy)]
struct EmitOutcome {
    /// At least one well-formed event was parsed and forwarded. The
    /// caller uses this to decide whether to re-project the run's
    /// snapshot for projected-event subscribers.
    events_appended: bool,
    /// `false` only if a `tx.send` failed AND there are no live
    /// subscribers — caller may then exit the tail.
    subscribers_alive: bool,
}

/// Pop complete (newline-terminated) byte-lines from `buffer`,
/// decode UTF-8, parse JSON, and forward. A complete byte-line that
/// fails UTF-8 decode is logged and skipped. The trailing partial
/// byte-run — if any — is left in `buffer` for the next read.
fn emit_complete_lines(
    buffer: &mut Vec<u8>,
    run_id: &RunId,
    tx: &broadcast::Sender<StreamedEvent>,
) -> EmitOutcome {
    let mut alive = true;
    let mut events_appended = false;
    while let Some(nl) = buffer.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=nl).take(nl).collect();
        let line = match std::str::from_utf8(&raw) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(run_id = %run_id, %err, "skipping non-utf8 event line");
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<OodaEvent>(trimmed) {
            Ok(event) => {
                events_appended = true;
                let streamed = StreamedEvent {
                    run_id: run_id.as_str().to_string(),
                    event,
                };
                if tx.send(streamed).is_err() && tx.receiver_count() == 0 {
                    alive = false;
                }
            }
            Err(err) => {
                tracing::warn!(run_id = %run_id, %err, line = %trimmed, "skipping malformed event line");
            }
        }
    }
    EmitOutcome {
        events_appended,
        subscribers_alive: alive,
    }
}

// ── Shutdown ─────────────────────────────────────────────────────────

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received"),
        () = terminate => tracing::info!("SIGTERM received"),
    }
}

// ── CLI parsing ──────────────────────────────────────────────────────

struct Args {
    port: u16,
    bind: Option<IpAddr>,
    state_root: Option<PathBuf>,
}

/// Parser-stage error discriminant. `Usage` carries a single-line
/// diagnostic that maps to `ExitCode::UsageError` (64); `Other`
/// wraps any non-CLI runtime failure that surfaces during arg
/// resolution.
enum CliError {
    Usage(String),
    #[allow(dead_code)]
    Other(Box<dyn std::error::Error>),
}

impl From<String> for CliError {
    fn from(msg: String) -> Self {
        Self::Usage(msg)
    }
}

fn parse_args() -> Result<Args, CliError> {
    // Help-pre-scan establishes the help-dominates-parse-failure
    // invariant; without it, a malformed earlier flag would shadow a
    // later `--help`.
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print_usage();
        std::process::exit(0);
    }

    let mut port = DEFAULT_PORT;
    let mut bind = None;
    let mut state_root = None;
    let mut saw_serve = false;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                // Unreachable under the help-pre-scan invariant.
                // Retained as a structural backstop.
                print_usage();
                std::process::exit(0);
            }
            "serve" => {
                if saw_serve {
                    return Err(CliError::Usage("`serve` subcommand repeated".into()));
                }
                saw_serve = true;
            }
            "--port" => {
                let v = iter
                    .next()
                    .ok_or_else(|| CliError::Usage("--port requires a value".into()))?;
                port = v
                    .parse()
                    .map_err(|e| CliError::Usage(format!("--port: {e}")))?;
            }
            "--bind" => {
                let v = iter
                    .next()
                    .ok_or_else(|| CliError::Usage("--bind requires a value".into()))?;
                bind = Some(
                    v.parse()
                        .map_err(|e| CliError::Usage(format!("--bind: {e}")))?,
                );
            }
            "--state-root" => {
                let v = iter
                    .next()
                    .ok_or_else(|| CliError::Usage("--state-root requires a value".into()))?;
                state_root = Some(PathBuf::from(v));
            }
            other => return Err(CliError::Usage(format!("unknown argument: {other}"))),
        }
    }
    Ok(Args {
        port,
        bind,
        state_root,
    })
}

fn print_usage() {
    println!(
        "cockpit — local web companion for OODA agents\n\
         \n\
         Usage:\n  cockpit serve [--port N] [--bind ADDR] [--state-root PATH]\n\
         \n\
         Options:\n  --port N            HTTP port (default {DEFAULT_PORT})\n  --bind ADDR         bind address (default: 0.0.0.0 under WSL2, 127.0.0.1 elsewhere).\n                      Pick 127.0.0.1 for loopback-only on WSL2 if your\n                      .wslconfig has localhostForwarding=true and it works.\n  --state-root PATH   override OODA state root (default: ooda-state env chain)\n  -h, --help          show this help and exit\n"
    );
}

// ── Platform-aware bind defaults ─────────────────────────────────────

/// `WSL_DISTRO_NAME` is set inside a WSL2 distribution. When that
/// holds, default the bind to `0.0.0.0` so a browser on the
/// Windows host can reach the daemon — Windows's
/// `localhostForwarding` against `127.0.0.1` inside WSL2 has been
/// historically unreliable. On native Linux / macOS, default to
/// loopback for the usual no-external-surface guarantee.
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
}

fn default_bind_ip() -> IpAddr {
    if is_wsl() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_state::EventBody;
    use tokio::sync::broadcast;

    #[test]
    fn streamed_event_serializes_with_run_id_and_kind() {
        let ev = StreamedEvent {
            run_id: "abc".into(),
            event: OodaEvent::now(EventBody::RunStarted {
                domain: "pr".into(),
                target: serde_json::json!({ "slug": "w3-io/x", "pr": 1 }),
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["run_id"], "abc");
        assert_eq!(v["kind"], "run_started");
        assert_eq!(v["domain"], "pr");
        assert!(v["ts"].is_string());
    }

    #[test]
    fn emit_complete_lines_parses_and_buffers_partial() {
        let (tx, mut rx) = broadcast::channel::<StreamedEvent>(8);
        let run_id = RunId::new("test-run").unwrap();
        let ev = OodaEvent::now(EventBody::IterationDecided {
            iteration: 1,
            decision_kind: "Execute".into(),
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(serde_json::to_string(&ev).unwrap().as_bytes());
        buf.push(b'\n');
        buf.extend_from_slice(
            b"{\"ts\":\"2026-05-17T00:00:00Z\",\"kind\":\"iteration_decided\",\"iteration\":2,\"decision_kind\":\"H",
        );

        emit_complete_lines(&mut buf, &run_id, &tx);

        // First line was complete and forwarded.
        let got = rx.try_recv().unwrap();
        assert_eq!(got.run_id, "test-run");
        // Partial second line is left in the buffer.
        assert!(buf.starts_with(b"{\"ts\""));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_complete_lines_skips_malformed_line() {
        let (tx, mut rx) = broadcast::channel::<StreamedEvent>(8);
        let run_id = RunId::new("test-run").unwrap();
        let mut buf: Vec<u8> = b"not json\n".to_vec();
        emit_complete_lines(&mut buf, &run_id, &tx);
        assert!(buf.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_complete_lines_holds_split_utf8_across_polls() {
        let (tx, mut rx) = broadcast::channel::<StreamedEvent>(8);
        let run_id = RunId::new("test-run").unwrap();
        // Event body contains a non-ASCII codepoint ("é" = c3 a9).
        // First poll delivers everything up to and including the
        // c3 byte; second poll delivers a9 + newline. With the
        // String-based reader, the first read would fail with
        // InvalidData; with the byte-buffer reader, it holds the
        // partial bytes and the second pass completes the event.
        let ev = OodaEvent::now(EventBody::IterationDecided {
            iteration: 1,
            decision_kind: "Exécute".into(),
        });
        let line = serde_json::to_string(&ev).unwrap();
        let bytes = line.as_bytes();
        let split = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        let mut buf: Vec<u8> = bytes[..split].to_vec();
        emit_complete_lines(&mut buf, &run_id, &tx);
        assert!(rx.try_recv().is_err(), "no complete line yet (no newline)");
        buf.extend_from_slice(&bytes[split..]);
        buf.push(b'\n');
        emit_complete_lines(&mut buf, &run_id, &tx);
        assert!(rx.try_recv().is_ok());
        assert!(buf.is_empty());
    }

    #[test]
    fn run_id_from_marker_rejects_hidden_and_traversal() {
        assert!(run_id_from_marker(Path::new("/x/live/..")).is_none());
        assert!(run_id_from_marker(Path::new("/x/live/.swp")).is_none());
        assert_eq!(
            run_id_from_marker(Path::new("/x/live/abc-123"))
                .unwrap()
                .as_str(),
            "abc-123"
        );
    }

    #[test]
    fn is_terminal_kind_covers_three_closed_variants() {
        assert!(is_terminal_kind("run_halted"));
        assert!(is_terminal_kind("run_stalled"));
        assert!(is_terminal_kind("run_cap_reached"));
        assert!(!is_terminal_kind("run_started"));
        assert!(!is_terminal_kind("iteration_decided"));
        assert!(!is_terminal_kind("domain_specific"));
    }

    #[test]
    fn event_body_kind_matches_serde_tag_for_terminal_bodies() {
        let halted = EventBody::RunHalted {
            outcome: "DoneMerged".into(),
            exit_code: 0,
        };
        let stalled = EventBody::RunStalled {
            last_action: "x".into(),
        };
        let cap = EventBody::RunCapReached {
            last_action: "y".into(),
        };
        assert_eq!(event_body_kind(&halted), "run_halted");
        assert_eq!(event_body_kind(&stalled), "run_stalled");
        assert_eq!(event_body_kind(&cap), "run_cap_reached");
        // Spot-check: kind discriminant matches the serde tag.
        let v: serde_json::Value = serde_json::to_value(OodaEvent::now(halted)).unwrap();
        assert_eq!(v["kind"], "run_halted");
    }

    #[test]
    fn check_origin_allows_missing_header() {
        let h = HeaderMap::new();
        assert!(check_origin(&h, 7777).is_ok());
    }

    #[test]
    fn check_origin_allows_localhost_and_loopback_on_port() {
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "http://localhost:7777".parse().unwrap());
        assert!(check_origin(&h, 7777).is_ok());
        h.insert(header::ORIGIN, "http://127.0.0.1:7777".parse().unwrap());
        assert!(check_origin(&h, 7777).is_ok());
    }

    #[test]
    fn check_origin_rejects_other_hosts_and_wrong_port() {
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        assert!(check_origin(&h, 7777).is_err());
        h.insert(header::ORIGIN, "http://localhost:7778".parse().unwrap());
        assert!(check_origin(&h, 7777).is_err());
        h.insert(header::ORIGIN, "https://localhost:7777".parse().unwrap());
        assert!(check_origin(&h, 7777).is_err());
    }

    #[test]
    fn run_id_writer_pid_round_trips_on_generated_ids() {
        let id = RunId::generate();
        let pid = id.writer_pid().expect("generated id has pid suffix");
        assert_eq!(pid, std::process::id());
    }
}
