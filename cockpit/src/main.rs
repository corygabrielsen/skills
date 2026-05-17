//! Cockpit V1 daemon — HTTP + SSE shell over the OODA state tree.
//!
//! # Role
//!
//! Background companion to the OODA agent family. V1 stands up the
//! daemon shell: axum HTTP server on localhost, placeholder HTML
//! page, health endpoint, and a `/api/events` SSE stream that
//! emits a periodic heartbeat. The file-watcher implementation
//! (the real mutation feed) lands in a follow-up — a recursive
//! watch on the full state tree (~300k directories at the time
//! of writing) is too costly without first narrowing scope to
//! "recently active runs". V1 ships the working pipeline so the
//! watcher can be slotted in once that scoping policy is picked.
//!
//! V1 scope is observational. The bidirectional control plane
//! (POST endpoints, WebSocket) is V2. See
//! `~/.claude/projects/-home-cory-code-skills/memory/project-cockpit-design.md`.
//!
//! # Invariants
//!
//! - **Read-only**: V1 never writes to the state tree. Cockpit is a
//!   viewer, not a participant. (V2 control endpoints will gate
//!   writes through an explicit allowlist.)
//! - **Bind defaults are platform-aware**: WSL2 forwards
//!   `127.0.0.1` to the Windows host inconsistently, so the
//!   default bind is `0.0.0.0` under WSL2 and `127.0.0.1`
//!   everywhere else. `--bind ADDR` overrides; pick `127.0.0.1`
//!   if you want loopback-only on WSL2 (and have verified
//!   `localhostForwarding` works in your `.wslconfig`).
//! - **Fail open on disconnect**: SSE clients can lag, disconnect,
//!   reconnect; the daemon never blocks on a slow client.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use futures_util::stream::Stream;
use serde::Serialize;
use tokio_stream::StreamExt;

const DEFAULT_PORT: u16 = 7777;

#[derive(Clone)]
struct AppState {
    state_root: PathBuf,
}

#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
    state_root: String,
    version: &'static str,
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
    let state_root = resolve_state_root(args.state_root.as_deref());
    let bind_ip = args.bind.unwrap_or_else(default_bind_ip);
    let addr = SocketAddr::new(bind_ip, args.port);

    if is_wsl() {
        tracing::info!(
            ?addr,
            ?state_root,
            "cockpit starting (WSL2 detected — default bind is 0.0.0.0 so Windows browsers can reach the daemon)",
        );
    } else {
        tracing::info!(?addr, ?state_root, "cockpit starting");
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/events", get(events_sse))
        .with_state(AppState {
            state_root: state_root.clone(),
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
    State(_app): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // V1 placeholder stream: emits a `heartbeat` event every 5
    // seconds so the SSE plumbing is end-to-end testable. The
    // real file-watcher (which observes per-PR mutations and
    // pushes structured events) lands in a follow-up — see the
    // module-level doc comment for the scoping rationale.
    let interval = tokio::time::interval(Duration::from_secs(5));
    let stream = tokio_stream::wrappers::IntervalStream::new(interval).map(|_| {
        Ok(Event::default()
            .event("heartbeat")
            .data(chrono::Utc::now().to_rfc3339()))
    });
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
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
            "serve" => {} // sole subcommand (V1); accepted for future-proofing
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
         Options:\n  --port N            HTTP port (default {DEFAULT_PORT})\n  --bind ADDR         bind address (default: 0.0.0.0 under WSL2, 127.0.0.1 elsewhere).\n                      Pick 127.0.0.1 for loopback-only on WSL2 if your\n                      .wslconfig has localhostForwarding=true and it works.\n  --state-root PATH   override OODA state root (default: env chain resolution)\n  -h, --help          show this help and exit\n"
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

// ── State-root resolution ────────────────────────────────────────────

fn resolve_state_root(override_path: Option<&Path>) -> PathBuf {
    // V1 reads ooda-pr's state tree. Future versions will also read
    // other domains' state roots (codex-review, suites/) — for now
    // we point at the same env chain ooda-pr resolves.
    ooda_core::state_root::resolve_ooda_pr_state_root(override_path)
}
