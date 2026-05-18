//! Integration tests for `POST /api/runs/<id>/halt`.
//!
//! The endpoint signals a writer PID embedded in the run id. Each
//! test spawns the cockpit daemon against a fresh tempdir state root,
//! optionally hand-builds a fixture run + live marker that points at
//! a real (but unrelated) child process this test owns, and asserts
//! the daemon's response code matches the spec matrix.
//!
//! The fixture writer is `sleep 999` — it has no signal handler and
//! dies promptly on SIGTERM, which is exactly the property the test
//! asserts (the process is gone after the halt POST returns 200).

use std::fs::{self, File};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ooda_state::{EventBody, RunId, StateRoot};
use serde_json::{Value, json};
use tempfile::TempDir;

fn cockpit_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cockpit"))
}

fn pick_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct DaemonHandle {
    child: Child,
    port: u16,
    _tmp: TempDir,
}

impl DaemonHandle {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(tmp: TempDir) -> DaemonHandle {
    let port = pick_port();
    let child = Command::new(cockpit_bin())
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--state-root")
        .arg(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cockpit");
    let handle = DaemonHandle {
        child,
        port,
        _tmp: tmp,
    };
    wait_until_ready(&handle.base());
    handle
}

fn wait_until_ready(base: &str) {
    let client = reqwest::blocking::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let url = format!("{base}/api/health");
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send()
            && resp.status().is_success()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("cockpit did not become ready within 10s");
}

/// Halted-run fixture for the 409 case. Returns the run id; the
/// writer is dropped after `halt()` so the live marker is gone too —
/// the test asserts both 409 (terminal) and 404 (no marker) variants.
fn fixture_halted_run(state_root: &Path) -> RunId {
    let root = StateRoot::new(state_root).unwrap();
    let id = RunId::generate();
    let mut writer = root.create_run(id.clone()).unwrap();
    writer
        .start(EventBody::RunStarted {
            domain: "pr".into(),
            target: json!({"slug": "owner/repo", "pr": 1}),
        })
        .unwrap();
    writer
        .halt(EventBody::RunHalted {
            outcome: "DoneMerged".into(),
            exit_code: 0,
        })
        .unwrap();
    id
}

/// Build a "live" run fixture whose `-p<pid>` suffix points at a
/// `sleep 999` child this test spawned. The run-id is hand-built so
/// the embedded PID matches the sleep child (NOT this test process).
///
/// Returns the run id and the live child. Dropping the child kills
/// it; the test should `.wait()` or `.kill()` explicitly to assert
/// the halt POST is what terminated it.
fn fixture_live_run_with_sleep(state_root: &Path) -> (RunId, Child) {
    let child = Command::new("sleep")
        .arg("999")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    // Match RunId::generate's shape: <YYYYMMDDTHHMMSSZ>-<entropy>-p<pid>
    let id_str = format!("20990101T000000Z-{pid:09}-p{pid}");
    let id = RunId::new(id_str).unwrap();

    // Materialize the run dir + an empty events.jsonl, then create
    // the live marker by hand (RunWriter would record OUR pid in the
    // marker path indirectly; we want a marker whose name embeds the
    // sleep child's pid).
    let runs_dir = state_root.join("runs").join(id.as_str());
    fs::create_dir_all(runs_dir.join("blobs")).unwrap();
    File::create(runs_dir.join("events.jsonl")).unwrap();
    let live_marker = state_root.join("live").join(id.as_str());
    fs::create_dir_all(state_root.join("live")).unwrap();
    File::create(&live_marker).unwrap();
    (id, child)
}

fn post_halt(base: &str, run_id: &str, origin: Option<&str>) -> (reqwest::StatusCode, Value) {
    let client = reqwest::blocking::Client::new();
    let mut req = client
        .post(format!("{base}/api/runs/{run_id}/halt"))
        .header("content-type", "application/json")
        .body("{}");
    if let Some(o) = origin {
        req = req.header("origin", o);
    }
    let resp = req.send().unwrap();
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    (status, body)
}

/// Poll a child handle for exit; return whether it terminated within
/// the window. Uses `try_wait` so the test reaps the child as soon as
/// the kernel marks it dead — `/proc/<pid>` lingers for zombies, so a
/// pure path-exists check would always read "alive" for the test's
/// own children.
fn wait_until_dead(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    matches!(child.try_wait(), Ok(Some(_)))
}

// ── Error cases ──────────────────────────────────────────────────────

#[test]
fn halt_missing_pid_suffix_returns_412() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    // run id without -p<pid> suffix
    let (status, body) = post_halt(&daemon.base(), "no-pid-suffix-here", None);
    assert_eq!(status, reqwest::StatusCode::PRECONDITION_FAILED);
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("-p<pid>"),
        "got body: {body}"
    );
}

#[test]
fn halt_with_no_live_marker_returns_404() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    // id with correct shape, no fixture on disk
    let (status, _) = post_halt(&daemon.base(), "20990101T000000Z-000000000-p99999", None);
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

#[test]
fn halt_against_terminal_run_returns_404_or_409() {
    // A halted run's live marker is released by the writer's Drop
    // impl on halt(), so the actual response surfaced first is 404
    // (no live marker). To trigger the terminal-event path we have
    // to re-create the marker by hand.
    let tmp = TempDir::new().unwrap();
    let id = fixture_halted_run(tmp.path());
    // Re-create the marker so the terminal-event check fires before
    // the marker check would 404 us.
    let marker = tmp.path().join("live").join(id.as_str());
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    File::create(&marker).unwrap();
    let daemon = spawn_daemon(tmp);
    let (status, body) = post_halt(&daemon.base(), id.as_str(), None);
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("terminal"),
        "got body: {body}"
    );
}

#[test]
fn halt_against_foreign_pid_returns_403() {
    // Build a marker whose -p<pid> suffix points at the cockpit
    // daemon's own PID — the cockpit binary IS named `cockpit`, not
    // `ooda-*`, so the PID-ownership check should reject it.
    let tmp = TempDir::new().unwrap();
    let state_root_path = tmp.path().to_path_buf();
    // Spawn the daemon FIRST so we know its PID, then build a marker
    // pointing at it.
    let daemon = spawn_daemon(tmp);
    let daemon_pid = daemon.child.id();
    let id_str = format!("20990101T000000Z-000000000-p{daemon_pid}");
    let id = RunId::new(id_str).unwrap();
    let runs_dir = state_root_path.join("runs").join(id.as_str());
    fs::create_dir_all(runs_dir.join("blobs")).unwrap();
    File::create(runs_dir.join("events.jsonl")).unwrap();
    let marker = state_root_path.join("live").join(id.as_str());
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    File::create(&marker).unwrap();
    let (status, body) = post_halt(&daemon.base(), id.as_str(), None);
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("does not appear"),
        "got body: {body}"
    );
}

#[test]
fn halt_with_dead_pid_returns_404() {
    // Spawn + kill a sleep so its PID is recently freed; build a
    // marker pointing at the dead PID. The kernel may have recycled
    // the PID by the time we look, but the test still passes if the
    // ownership check or liveness check rejects it (both surface as
    // 404 or 403).
    let mut child = Command::new("sleep")
        .arg("999")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.kill().unwrap();
    child.wait().unwrap();
    // Brief window before kernel recycles the pid.
    let tmp = TempDir::new().unwrap();
    let state_root_path = tmp.path().to_path_buf();
    let id_str = format!("20990101T000000Z-000000000-p{pid}");
    let id = RunId::new(id_str).unwrap();
    let runs_dir = state_root_path.join("runs").join(id.as_str());
    fs::create_dir_all(runs_dir.join("blobs")).unwrap();
    File::create(runs_dir.join("events.jsonl")).unwrap();
    let marker = state_root_path.join("live").join(id.as_str());
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    File::create(&marker).unwrap();
    let daemon = spawn_daemon(tmp);
    let (status, _) = post_halt(&daemon.base(), id.as_str(), None);
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

// ── Origin matrix ────────────────────────────────────────────────────

#[test]
fn halt_with_disallowed_origin_returns_403() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    let (status, body) = post_halt(
        &daemon.base(),
        "20990101T000000Z-000000000-p99999",
        Some("http://evil.example"),
    );
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("origin"),
        "got body: {body}"
    );
}

#[test]
fn halt_with_wrong_port_origin_returns_403() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    let other_port = daemon.port.wrapping_add(1);
    let (status, _) = post_halt(
        &daemon.base(),
        "20990101T000000Z-000000000-p99999",
        Some(&format!("http://localhost:{other_port}")),
    );
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
}

#[test]
fn halt_with_allowed_origin_localhost_proceeds() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    // Surface the Origin check separately from PID resolution: the
    // 404 (no live marker) means we got past the Origin gate.
    let origin = format!("http://localhost:{}", daemon.port);
    let (status, _) = post_halt(
        &daemon.base(),
        "20990101T000000Z-000000000-p99999",
        Some(&origin),
    );
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

#[test]
fn halt_with_allowed_origin_loopback_proceeds() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    let origin = format!("http://127.0.0.1:{}", daemon.port);
    let (status, _) = post_halt(
        &daemon.base(),
        "20990101T000000Z-000000000-p99999",
        Some(&origin),
    );
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

// ── Happy path ───────────────────────────────────────────────────────

#[test]
#[cfg(target_os = "linux")]
fn halt_signals_writer_pid_and_returns_200() {
    // PID-ownership check requires /proc/<pid>/comm to start with
    // `ooda-`. `/proc/<pid>/comm` reflects the executable's basename
    // (not argv[0]), so we symlink the real `sleep` binary to a name
    // beginning with `ooda-` and exec the symlink. On Linux this
    // makes `comm` read as the symlink basename.
    let bin_tmp = TempDir::new().unwrap();
    let fake_path = bin_tmp.path().join("ooda-fake-writer");
    let sleep_real = if Path::new("/usr/bin/sleep").exists() {
        "/usr/bin/sleep"
    } else if Path::new("/bin/sleep").exists() {
        "/bin/sleep"
    } else {
        panic!("sleep binary not found");
    };
    std::os::unix::fs::symlink(sleep_real, &fake_path).unwrap();
    let mut child = Command::new(&fake_path)
        .arg("999")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    // Wait briefly for exec to land and /proc/<pid>/comm to be
    // updated; without this the comm read can race the exec.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm"))
            && comm.trim().starts_with("ooda-")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap();
    assert!(
        comm.trim().starts_with("ooda-"),
        "expected ooda- prefix in comm; got {comm:?}",
    );

    let tmp = TempDir::new().unwrap();
    let state_root_path = tmp.path().to_path_buf();
    let id_str = format!("20990101T000000Z-000000000-p{pid}");
    let id = RunId::new(id_str).unwrap();
    let runs_dir = state_root_path.join("runs").join(id.as_str());
    fs::create_dir_all(runs_dir.join("blobs")).unwrap();
    // Seed with a non-terminal RunStarted so the terminal-event check
    // passes.
    let mut f = File::create(runs_dir.join("events.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"ts":"2026-05-17T00:00:00Z","kind":"run_started","domain":"pr","target":{{}}}}"#
    )
    .unwrap();
    drop(f);
    let marker = state_root_path.join("live").join(id.as_str());
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    File::create(&marker).unwrap();

    let daemon = spawn_daemon(tmp);
    let (status, body) = post_halt(&daemon.base(), id.as_str(), None);
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("signalled")
    );
    assert_eq!(
        body.get("pid").and_then(Value::as_u64),
        Some(u64::from(pid))
    );
    assert_eq!(
        body.get("run_id").and_then(Value::as_str),
        Some(id.as_str())
    );

    // SIGTERM should kill `sleep` (no handler) within milliseconds.
    assert!(
        wait_until_dead(&mut child, Duration::from_secs(5)),
        "sleep pid {pid} still alive after SIGTERM",
    );
}

#[test]
fn halt_against_live_sleep_succeeds_or_403_on_ownership() {
    // Cross-platform variant of the happy-path test. On Linux without
    // the `exec -a` rename trick, /proc/<pid>/comm reports `sleep`,
    // not `ooda-*`, so the endpoint rejects with 403. On BSD/macOS
    // the ownership check is skipped and the SIGTERM proceeds (200).
    let tmp = TempDir::new().unwrap();
    let state_root_path = tmp.path().to_path_buf();
    let (id, mut child) = fixture_live_run_with_sleep(&state_root_path);
    let pid = child.id();
    let daemon = spawn_daemon(tmp);
    let (status, _) = post_halt(&daemon.base(), id.as_str(), None);
    if cfg!(target_os = "linux") {
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
        // Cleanup: kill the sleep ourselves.
        let _ = child.kill();
        let _ = child.wait();
    } else {
        assert_eq!(status, reqwest::StatusCode::OK);
        assert!(wait_until_dead(&mut child, Duration::from_secs(5)));
        // Cross-platform: `pid` is referenced for diagnostic clarity.
        let _ = pid;
    }
}
