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
use axum::extract::State;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use futures_util::stream::Stream;
use notify::{EventKind, RecursiveMode, Watcher};
use ooda_core::ExitCode;
use ooda_state::{Event as OodaEvent, RunId, StateRoot};
use serde::Serialize;
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
    spawn_live_watcher(state_root.clone(), tx.clone(), Arc::clone(&metrics));

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/events", get(events_sse))
        .with_state(AppState {
            state_root: state_root_path.clone(),
            tx,
            metrics,
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
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
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
            async move { run_live_watcher_once(state_root, tx, tails, metrics, generation).await }
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

async fn start_tail(
    state_root: &StateRoot,
    tx: &broadcast::Sender<StreamedEvent>,
    tails: &Tails,
    metrics: &Arc<Metrics>,
    generation: &Arc<AtomicU64>,
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
    let id_for_task = id.clone();
    tokio::spawn(async move {
        run_tail_with_restart(
            events_path,
            id_for_task.clone(),
            tx,
            cancel_rx,
            start,
            Arc::clone(&metrics),
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
async fn run_tail_with_restart(
    events_path: PathBuf,
    run_id: RunId,
    tx: broadcast::Sender<StreamedEvent>,
    mut cancel_rx: oneshot::Receiver<()>,
    start: StartOffset,
    metrics: Arc<Metrics>,
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
async fn tail_events_once(
    events_path: &Path,
    run_id: &RunId,
    tx: &broadcast::Sender<StreamedEvent>,
    cancel_rx: &mut oneshot::Receiver<()>,
    offset: &mut u64,
    partial: &mut Vec<u8>,
    metrics: &Arc<Metrics>,
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
    if !emit_complete_lines(partial, run_id, tx) {
        return TailStep::SubscribersGone;
    }
    TailStep::Idle
}

/// Pop complete (newline-terminated) byte-lines from `buffer`,
/// decode UTF-8, parse JSON, and forward. Returns `false` if a
/// `tx.send` failed AND there are no live subscribers — caller may
/// then exit the tail. A complete byte-line that fails UTF-8 decode
/// is logged and skipped. The trailing partial byte-run — if any —
/// is left in `buffer` for the next read.
fn emit_complete_lines(
    buffer: &mut Vec<u8>,
    run_id: &RunId,
    tx: &broadcast::Sender<StreamedEvent>,
) -> bool {
    let mut alive = true;
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
    alive
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
}
