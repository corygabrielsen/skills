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
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use futures_util::stream::Stream;
use notify::{EventKind, RecursiveMode, Watcher};
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

/// How often the per-run tail task wakes to read new bytes. The
/// `live/` watcher is event-driven, but each `events.jsonl` is
/// polled because adding a second inotify watch per active run is
/// not worth the cost given typical agent cadence (~1 event/sec).
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    state_root: PathBuf,
    tx: broadcast::Sender<StreamedEvent>,
}

#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
    state_root: String,
    version: &'static str,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cockpit=info,tower_http=info".into()),
        )
        .init();

    let args = parse_args()?;
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

    let (tx, _rx) = broadcast::channel::<StreamedEvent>(BROADCAST_CAPACITY);
    spawn_live_watcher(state_root.clone(), tx.clone());

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/events", get(events_sse))
        .with_state(AppState {
            state_root: state_root_path.clone(),
            tx,
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
    })
}

async fn events_sse(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // BroadcastStream surfaces a `Lagged(n)` error when a slow
    // subscriber falls behind the channel capacity. Cockpit's
    // contract is "live tail, best effort" — log the lag and drop
    // the missed batch; do not tear down the SSE connection.
    let rx = app.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => match serde_json::to_string(&ev) {
            Ok(json) => Some(Ok(SseEvent::default().event("mutation").data(json))),
            Err(err) => {
                tracing::warn!(%err, "dropping event: serialize failed");
                None
            }
        },
        Err(err) => {
            tracing::warn!(%err, "broadcast subscriber lagged; dropping batch");
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

/// Spawn the `live/` watcher and pre-tail any runs already live at
/// startup. The watcher runs for the lifetime of the daemon.
fn spawn_live_watcher(state_root: StateRoot, tx: broadcast::Sender<StreamedEvent>) {
    tokio::spawn(async move {
        if let Err(err) = run_live_watcher(state_root, tx).await {
            tracing::error!(%err, "live watcher exited with error");
        }
    });
}

async fn run_live_watcher(
    state_root: StateRoot,
    tx: broadcast::Sender<StreamedEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let live_dir = state_root.path().join("live");
    // StateRoot::new created `live/` if missing; tolerate the race
    // where it was removed between construction and watcher startup.
    tokio::fs::create_dir_all(&live_dir).await?;

    let tails: Arc<Mutex<HashMap<RunId, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Pre-tail every already-live run from offset 0. The first
    // pass replays the on-disk history into the broadcast channel;
    // subsequent appends keep flowing through the same tail task.
    //
    // Caveat: `tokio::broadcast` does not replay past messages to
    // late subscribers, so an SSE client that connects after this
    // pre-tail completes will not see the replayed events — only
    // appends that follow. Per-connection backfill (read disk →
    // forward to the client → subscribe to broadcast) is future
    // work; for now, late connectors get the live tail only.
    for id in state_root.live_runs()? {
        start_tail(&state_root, &tx, &tails, id).await;
    }

    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) => {
                // Best-effort: receiver gone means the daemon is
                // shutting down; drop quietly.
                let _ = notify_tx.send(ev);
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
                        start_tail(&state_root, &tx, &tails, id).await;
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

    // Channel closed: watcher dropped.  Move ownership into the
    // task so it lives as long as the loop runs.
    drop(watcher);
    Ok(())
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
    tails: &Arc<Mutex<HashMap<RunId, oneshot::Sender<()>>>>,
    id: RunId,
) {
    let mut guard = tails.lock().await;
    if guard.contains_key(&id) {
        return;
    }
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    guard.insert(id.clone(), cancel_tx);
    drop(guard);

    let events_path = state_root
        .path()
        .join("runs")
        .join(id.as_str())
        .join("events.jsonl");
    let tx = tx.clone();
    let tails = Arc::clone(tails);
    let id_for_task = id.clone();
    tokio::spawn(async move {
        if let Err(err) = tail_events(events_path, id_for_task.clone(), tx, cancel_rx).await {
            tracing::warn!(run_id = %id_for_task, %err, "tail task exited with error");
        }
        // Self-cleanup so a halted run can be re-tailed if its
        // marker reappears (test scenarios; not expected in
        // production where run-ids are unique).
        tails.lock().await.remove(&id_for_task);
    });
}

async fn stop_tail(tails: &Arc<Mutex<HashMap<RunId, oneshot::Sender<()>>>>, id: &RunId) {
    if let Some(cancel) = tails.lock().await.remove(id) {
        let _ = cancel.send(());
    }
}

/// Tail `events_path` from offset 0, forwarding each parsed line as
/// a `StreamedEvent` to `tx`. Returns when `cancel_rx` fires or the
/// broadcast channel is dropped.
async fn tail_events(
    events_path: PathBuf,
    run_id: RunId,
    tx: broadcast::Sender<StreamedEvent>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut offset: u64 = 0;
    let mut partial = String::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => return Ok(()),
            () = tokio::time::sleep(TAIL_POLL_INTERVAL) => {}
        }

        let mut file = match tokio::fs::File::open(&events_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let len = file.metadata().await?.len();
        if len <= offset {
            // No new bytes (or file truncated — Cockpit treats
            // truncation as "nothing new"; events.jsonl is
            // append-only by the writer contract).
            continue;
        }
        file.seek(SeekFrom::Start(offset)).await?;
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        let read = reader.read_to_string(&mut buf).await?;
        offset += read as u64;
        partial.push_str(&buf);
        emit_complete_lines(&mut partial, &run_id, &tx);
    }
}

/// Pop complete (newline-terminated) lines from `buffer`, parse,
/// and forward. The trailing partial line — if any — is left in
/// `buffer` for the next read.
fn emit_complete_lines(buffer: &mut String, run_id: &RunId, tx: &broadcast::Sender<StreamedEvent>) {
    while let Some(nl) = buffer.find('\n') {
        let line = buffer[..nl].to_string();
        buffer.drain(..=nl);
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
                // Send-error means no subscribers; that's fine — the
                // event is still on disk for late connectors via a
                // future history endpoint.
                let _ = tx.send(streamed);
            }
            Err(err) => {
                tracing::warn!(run_id = %run_id, %err, line = %trimmed, "skipping malformed event line");
            }
        }
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

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut port = DEFAULT_PORT;
    let mut bind = None;
    let mut state_root = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "serve" => {} // sole subcommand today; accepted for future-proofing
            "--port" => {
                let v = iter.next().ok_or("--port requires a value")?;
                port = v.parse().map_err(|e| format!("--port: {e}"))?;
            }
            "--bind" => {
                let v = iter.next().ok_or("--bind requires a value")?;
                bind = Some(v.parse().map_err(|e| format!("--bind: {e}"))?);
            }
            "--state-root" => {
                let v = iter.next().ok_or("--state-root requires a value")?;
                state_root = Some(PathBuf::from(v));
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
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
        let mut buf = String::new();
        buf.push_str(&serde_json::to_string(&ev).unwrap());
        buf.push('\n');
        buf.push_str("{\"ts\":\"2026-05-17T00:00:00Z\",\"kind\":\"iteration_decided\",\"iteration\":2,\"decision_kind\":\"H");

        emit_complete_lines(&mut buf, &run_id, &tx);

        // First line was complete and forwarded.
        let got = rx.try_recv().unwrap();
        assert_eq!(got.run_id, "test-run");
        // Partial second line is left in the buffer.
        assert!(buf.starts_with("{\"ts\""));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_complete_lines_skips_malformed_line() {
        let (tx, mut rx) = broadcast::channel::<StreamedEvent>(8);
        let run_id = RunId::new("test-run").unwrap();
        let mut buf = String::from("not json\n");
        emit_complete_lines(&mut buf, &run_id, &tx);
        assert!(buf.is_empty());
        assert!(rx.try_recv().is_err());
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
