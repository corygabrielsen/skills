//! Integration tests for the `ooda-attest` binary.
//!
//! Each test builds an isolated environment: a tempdir for the
//! state-root and (where needed) a tempdir initialised as a git
//! repo as the working directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

fn init_git_repo(dir: &Path) -> String {
    run(dir, "git", &["init", "-q", "-b", "main"]);
    run(dir, "git", &["config", "user.email", "test@example.com"]);
    run(dir, "git", &["config", "user.name", "Test"]);
    run(dir, "git", &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("README.md"), b"hello\n").unwrap();
    run(dir, "git", &["add", "README.md"]);
    run(dir, "git", &["commit", "-q", "-m", "init"]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git rev-parse failed: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?} failed");
}

fn bin() -> Command {
    Command::cargo_bin("ooda-attest").unwrap()
}

fn read_attestation(path: &Path) -> serde_json::Value {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap()
}

fn expected_path(state_root: &Path, pr_id: &str) -> PathBuf {
    state_root
        .canonicalize()
        .unwrap()
        .join(pr_id)
        .join("pr_meta_attest.json")
}

#[test]
fn happy_path_writes_attestation_for_head() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "42", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();

    let path = expected_path(state_root.path(), "42");
    assert!(
        path.exists(),
        "attestation not written at {}",
        path.display()
    );
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
    assert_eq!(json["version"].as_u64().unwrap(), 1);
    assert!(json["attested_at"].as_str().is_some());
}

#[test]
fn invalid_pr_id_exits_64() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "abc", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("--pr-id"));
}

#[test]
fn missing_state_root_exits_64() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let missing = repo.path().join("nope-not-here");

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "1", "--state-root"])
        .arg(&missing)
        .assert()
        .failure()
        .code(64)
        .stderr(contains(missing.display().to_string()));
}

#[test]
fn not_a_git_repo_exits_65() {
    let non_repo = TempDir::new().unwrap();
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(non_repo.path())
        .args(["pr-meta", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(65);
}

#[test]
fn idempotent_second_run_overwrites_and_advances_timestamp() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let path = expected_path(state_root.path(), "7");
    let first = read_attestation(&path);
    let first_at = first["attested_at"].as_str().unwrap().to_string();

    std::thread::sleep(std::time::Duration::from_millis(1100));

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let second = read_attestation(&path);
    let second_at = second["attested_at"].as_str().unwrap();

    assert_eq!(
        first["attested_sha"].as_str().unwrap(),
        second["attested_sha"].as_str().unwrap()
    );
    assert_ne!(
        first_at, second_at,
        "attested_at should advance between runs"
    );
    assert!(
        second_at > first_at.as_str(),
        "timestamp must monotonically advance"
    );
}
