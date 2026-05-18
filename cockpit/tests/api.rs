//! Integration tests for the cockpit HTTP surface.
//!
//! Each test spawns the compiled `cockpit` binary against a fresh
//! tempdir state root populated with hand-built fixture runs, then
//! exercises the projection endpoints over the wire. Process-level
//! testing is intentional: it covers the full axum + tokio path the
//! browser actually hits.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ooda_state::{EventBody, RunId, StateRoot};
use serde_json::Value;
use tempfile::TempDir;

/// Locate the just-built `cockpit` binary. Integration tests run
/// after `cargo build`; the release vs. debug profile depends on how
/// `cargo test` was invoked.
fn cockpit_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo at compile time so we can
    // address the integration target's bin without scraping
    // target/{debug,release}/.
    PathBuf::from(env!("CARGO_BIN_EXE_cockpit"))
}

/// Reserve a port by binding to 0 and reading the assigned port back.
/// The listener is dropped immediately; the kernel keeps the port
/// free for a moment before the daemon claims it. Racy in principle;
/// acceptable for serial integration tests.
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

/// Write a complete halted-run fixture to `state_root`. Returns the
/// run id so tests can address it.
fn fixture_halted_run(state_root: &Path) -> RunId {
    let root = StateRoot::new(state_root).unwrap();
    let id = RunId::generate();
    let mut writer = root.create_run(id.clone()).unwrap();
    writer
        .start(EventBody::RunStarted {
            domain: "pr".into(),
            target: serde_json::json!({"slug": "owner/repo", "pr": 42}),
        })
        .unwrap();
    let blob = writer.write_blob(b"\"observed bytes\"", "json").unwrap();
    writer
        .append(EventBody::IterationObserved { iteration: 1, blob })
        .unwrap();
    writer
        .append(EventBody::IterationDecided {
            iteration: 1,
            decision_kind: "Execute".into(),
        })
        .unwrap();
    writer
        .append(EventBody::IterationExecuted {
            iteration: 1,
            action_kind: "ReRunCi".into(),
            success: true,
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

/// Write a never-halted (active) run fixture. The writer is dropped
/// without `halt`; ooda-state's Drop impl appends a synthetic
/// `DroppedWithoutHalt` terminal so the projector still treats it as
/// halted on the read side. To get a truly-active fixture we instead
/// skip Drop's terminal by leaving the writer's marker on disk
/// directly — easier: just create another halted run with a different
/// timestamp.
fn fixture_second_halted_run(state_root: &Path) -> RunId {
    // Sleep so RunId::generate yields a fresh id (timestamp+nanos).
    std::thread::sleep(Duration::from_millis(5));
    let root = StateRoot::new(state_root).unwrap();
    let id = RunId::generate();
    let mut writer = root.create_run(id.clone()).unwrap();
    writer
        .start(EventBody::RunStarted {
            domain: "codex-review".into(),
            target: serde_json::json!({"level": "low"}),
        })
        .unwrap();
    writer
        .halt(EventBody::RunHalted {
            outcome: "DoneFixedPoint".into(),
            exit_code: 0,
        })
        .unwrap();
    id
}

fn get_json(base: &str, path: &str) -> (reqwest::StatusCode, Value) {
    let client = reqwest::blocking::Client::new();
    let resp = client.get(format!("{base}{path}")).send().unwrap();
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn health_endpoint_responds() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    let (status, body) = get_json(&daemon.base(), "/api/health");
    assert!(status.is_success());
    assert_eq!(body.get("status").and_then(Value::as_str), Some("ok"));
}

#[test]
fn list_runs_all_returns_fixtures() {
    let tmp = TempDir::new().unwrap();
    let id_a = fixture_halted_run(tmp.path());
    let id_b = fixture_second_halted_run(tmp.path());
    let daemon = spawn_daemon(tmp);

    let (status, body) = get_json(&daemon.base(), "/api/runs?status=all");
    assert!(status.is_success());
    let arr = body.as_array().expect("array");
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(|v| v.get("run_id").and_then(Value::as_str))
        .collect();
    assert!(ids.contains(&id_a.as_str()));
    assert!(ids.contains(&id_b.as_str()));
}

#[test]
fn list_runs_active_excludes_halted_runs() {
    let tmp = TempDir::new().unwrap();
    fixture_halted_run(tmp.path());
    let daemon = spawn_daemon(tmp);

    let (status, body) = get_json(&daemon.base(), "/api/runs?status=active");
    assert!(status.is_success());
    let arr = body.as_array().expect("array");
    // The fixture halted, so the live marker is gone — active list is
    // empty.
    assert!(arr.is_empty());
}

#[test]
fn get_run_returns_full_snapshot() {
    let tmp = TempDir::new().unwrap();
    let id = fixture_halted_run(tmp.path());
    let daemon = spawn_daemon(tmp);

    let (status, body) = get_json(&daemon.base(), &format!("/api/runs/{}", id.as_str()));
    assert!(status.is_success());
    assert_eq!(
        body.get("run_id").and_then(Value::as_str),
        Some(id.as_str())
    );
    assert_eq!(body.get("domain").and_then(Value::as_str), Some("pr"));
    assert_eq!(body.get("status").and_then(Value::as_str), Some("halted"));
    let iters = body.get("iterations").and_then(Value::as_array).unwrap();
    assert_eq!(iters.len(), 1);
}

#[test]
fn get_run_unknown_returns_404() {
    let tmp = TempDir::new().unwrap();
    let daemon = spawn_daemon(tmp);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!(
            "{}/api/runs/20990101T000000Z-000000000-p9999",
            daemon.base()
        ))
        .send()
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[test]
fn get_blob_returns_bytes_with_content_type() {
    let tmp = TempDir::new().unwrap();
    // Build a run with a blob we can address by sha.
    let blob_bytes = b"\"observed bytes\"";
    let id;
    let sha;
    {
        let root = StateRoot::new(tmp.path()).unwrap();
        id = RunId::generate();
        let mut writer = root.create_run(id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "pr".into(),
                target: serde_json::json!({"slug": "owner/repo", "pr": 1}),
            })
            .unwrap();
        let b = writer.write_blob(blob_bytes, "json").unwrap();
        sha = b.sha.clone();
        writer
            .append(EventBody::IterationObserved {
                iteration: 1,
                blob: b,
            })
            .unwrap();
        writer
            .halt(EventBody::RunHalted {
                outcome: "DoneMerged".into(),
                exit_code: 0,
            })
            .unwrap();
    }
    let daemon = spawn_daemon(tmp);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!(
            "{}/api/runs/{}/blobs/{}",
            daemon.base(),
            id.as_str(),
            sha
        ))
        .send()
        .unwrap();
    assert!(resp.status().is_success());
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("application/json"));
    let body = resp.bytes().unwrap();
    assert_eq!(body.as_ref(), blob_bytes);
}

#[test]
fn get_blob_unknown_returns_404() {
    let tmp = TempDir::new().unwrap();
    let id = fixture_halted_run(tmp.path());
    let daemon = spawn_daemon(tmp);
    let client = reqwest::blocking::Client::new();
    // 64-char hex string that won't exist on disk.
    let bogus = "0".repeat(64);
    let resp = client
        .get(format!(
            "{}/api/runs/{}/blobs/{}",
            daemon.base(),
            id.as_str(),
            bogus
        ))
        .send()
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[test]
fn run_events_sse_first_frame_is_snapshot() {
    let tmp = TempDir::new().unwrap();
    let id = fixture_halted_run(tmp.path());
    let daemon = spawn_daemon(tmp);
    // SSE: hold the connection long enough to read at least one
    // event:projected frame, then assert it parses as Snapshot.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut resp = client
        .get(format!("{}/api/runs/{}/events", daemon.base(), id.as_str()))
        .send()
        .unwrap();
    assert!(resp.status().is_success());

    // Read up to a small cap; the backfill frame is sent immediately.
    let mut buf = vec![0u8; 16 * 1024];
    let n = resp.read(&mut buf).unwrap();
    let chunk = std::str::from_utf8(&buf[..n]).unwrap();
    // SSE wire shape: lines beginning with `event: projected\n`
    // followed by `data: {"kind":"snapshot",...}`.
    assert!(
        chunk.contains("event: projected"),
        "expected projected SSE frame; got: {chunk}"
    );
    assert!(
        chunk.contains("\"kind\":\"snapshot\""),
        "expected snapshot variant; got: {chunk}"
    );
}
