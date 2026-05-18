//! Smoke tests for the static SPA assets served by the cockpit
//! daemon. Asserts presence of the anchor IDs the JS reaches for,
//! the absence of external CDN/font references, and the correct
//! Content-Type on each asset endpoint.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

fn spawn() -> DaemonHandle {
    let tmp = TempDir::new().unwrap();
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
        .unwrap();
    let handle = DaemonHandle {
        child,
        port,
        _tmp: tmp,
    };
    let client = reqwest::blocking::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let url = format!("{}/api/health", handle.base());
    while Instant::now() < deadline {
        if let Ok(r) = client.get(&url).send()
            && r.status().is_success()
        {
            return handle;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon not ready");
}

#[test]
fn root_serves_html_with_expected_anchors() {
    let d = spawn();
    let r = reqwest::blocking::get(format!("{}/", d.base())).unwrap();
    assert!(r.status().is_success());
    let ct = r
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/html"), "expected text/html, got: {ct}");
    let body = r.text().unwrap();
    for anchor in ["id=\"run-list\"", "id=\"center-pane\"", "id=\"right-pane\""] {
        assert!(
            body.contains(anchor),
            "missing anchor {anchor} in: {}",
            &body[..body.len().min(400)]
        );
    }
}

#[test]
fn root_html_is_under_100kb() {
    let d = spawn();
    let r = reqwest::blocking::get(format!("{}/", d.base())).unwrap();
    let body = r.text().unwrap();
    assert!(body.len() < 100 * 1024, "html size: {}", body.len());
}

#[test]
fn root_html_has_no_external_assets() {
    let d = spawn();
    let r = reqwest::blocking::get(format!("{}/", d.base())).unwrap();
    let body = r.text().unwrap();
    assert!(
        !body.contains("https://"),
        "external https reference in html"
    );
    assert!(
        !body.contains("http://") || body.matches("http://").count() == 0,
        "external http reference in html"
    );
    assert!(!body.contains("cdn."), "CDN reference in html");
}

#[test]
fn style_css_served_with_correct_content_type() {
    let d = spawn();
    let r = reqwest::blocking::get(format!("{}/assets/style.css", d.base())).unwrap();
    assert!(r.status().is_success());
    let ct = r
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/css"), "got: {ct}");
    let body = r.text().unwrap();
    assert!(body.contains(":root"));
}

#[test]
fn app_js_served_with_correct_content_type() {
    let d = spawn();
    let r = reqwest::blocking::get(format!("{}/assets/app.js", d.base())).unwrap();
    assert!(r.status().is_success());
    let ct = r
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("application/javascript"), "got: {ct}");
    let body = r.text().unwrap();
    assert!(body.contains("/api/runs"), "expected fetch URL in JS");
}
