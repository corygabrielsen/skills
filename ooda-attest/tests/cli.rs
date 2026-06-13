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
    init_git_repo_with_content(dir, b"hello\n", "init")
}

fn init_git_repo_with_content(dir: &Path, content: &[u8], message: &str) -> String {
    run(dir, "git", &["init", "-q", "-b", "main"]);
    run(dir, "git", &["config", "user.email", "test@example.com"]);
    run(dir, "git", &["config", "user.name", "Test"]);
    run(dir, "git", &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("README.md"), content).unwrap();
    run(dir, "git", &["add", "README.md"]);
    run(dir, "git", &["commit", "-q", "-m", message]);
    let output = scrub_git_env(Command::new("git"))
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git rev-parse failed: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let status = scrub_git_env(Command::new(program))
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?} failed");
}

/// Drop git env vars that pre-commit (and similar git-hook
/// runners) export to subprocesses. Without this, `git init` and
/// `git config` inside test tempdirs target the OUTER repo's
/// `.git/config` via `GIT_DIR`, causing lock contention when tests
/// run in parallel and silently corrupting state on success.
fn scrub_git_env(mut cmd: Command) -> Command {
    for var in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_CONFIG",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn bin() -> Command {
    // ooda-attest spawns `git rev-parse` internally; scrub git env
    // vars so they don't leak into those subprocesses.
    scrub_git_env(Command::cargo_bin("ooda-attest").unwrap())
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

fn expected_doc_review_path(state_root: &Path, pr_id: &str) -> PathBuf {
    state_root
        .canonicalize()
        .unwrap()
        .join(pr_id)
        .join("doc_review_attest.json")
}

fn expected_closeout_path(state_root: &Path, pr_id: &str) -> PathBuf {
    state_root
        .canonicalize()
        .unwrap()
        .join(pr_id)
        .join("closeout_attest.json")
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
fn invalid_pull_request_id_exits_64() {
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
fn not_a_git_repo_exits_64() {
    // Repo-root resolution runs at startup and returns
    // EXIT_VALIDATION (64) when no `--repo-root` is supplied and
    // CWD is not inside a git working tree. The wrong-tree case
    // is caught before the `git rev-parse HEAD` subprocess.
    let non_repo = TempDir::new().unwrap();
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(non_repo.path())
        .args(["pr-meta", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("not inside a git working tree"));
}

#[test]
fn ooda_pr_state_home_env_var_supplies_default_state_root() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env("OODA_STATE_HOME", state_root.path())
        .env_remove("XDG_STATE_HOME")
        .args(["pr-meta", "--pr-id", "753"])
        .assert()
        .success();

    let path = expected_path(state_root.path(), "753");
    assert!(
        path.exists(),
        "attestation not written at {}",
        path.display()
    );
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn home_fallback_supplies_default_state_root_when_no_env_overrides() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let fake_home = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env_remove("OODA_STATE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", fake_home.path())
        .args(["pr-meta", "--pr-id", "753"])
        .assert()
        .success();

    let path = fake_home
        .path()
        .canonicalize()
        .unwrap()
        .join(".local")
        .join("state")
        .join("ooda")
        .join("753")
        .join("pr_meta_attest.json");
    assert!(
        path.exists(),
        "attestation not written at {}",
        path.display()
    );
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn explicit_state_root_wins_over_env_var() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let explicit = TempDir::new().unwrap();
    let env_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env("OODA_STATE_HOME", env_root.path())
        .args(["pr-meta", "--pr-id", "42", "--state-root"])
        .arg(explicit.path())
        .assert()
        .success();

    let chosen = expected_path(explicit.path(), "42");
    let unchosen = expected_path(env_root.path(), "42");
    assert!(
        chosen.exists(),
        "explicit path missing: {}",
        chosen.display()
    );
    assert!(
        !unchosen.exists(),
        "env-var path should not have been written: {}",
        unchosen.display(),
    );
}

// ── doc-review subcommand ──────────────────────────────────────────

#[test]
fn doc_review_happy_path_writes_attestation_for_head() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["doc-review", "--pr-id", "42", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();

    let path = expected_doc_review_path(state_root.path(), "42");
    assert!(
        path.exists(),
        "doc-review attestation not written at {}",
        path.display()
    );
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
    assert_eq!(json["version"].as_u64().unwrap(), 1);
    assert!(json["attested_at"].as_str().is_some());
}

#[test]
fn doc_review_invalid_pull_request_id_exits_64() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["doc-review", "--pr-id", "abc", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("--pr-id"));
}

#[test]
fn doc_review_not_a_git_repo_exits_64() {
    // See `not_a_git_repo_exits_64` — same reasoning.
    let non_repo = TempDir::new().unwrap();
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(non_repo.path())
        .args(["doc-review", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("not inside a git working tree"));
}

#[test]
fn doc_review_ooda_pr_state_home_env_var_supplies_default_state_root() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env("OODA_STATE_HOME", state_root.path())
        .env_remove("XDG_STATE_HOME")
        .args(["doc-review", "--pr-id", "753"])
        .assert()
        .success();

    let path = expected_doc_review_path(state_root.path(), "753");
    assert!(path.exists(), "attestation not at {}", path.display());
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn doc_review_home_fallback_supplies_default_state_root_when_no_env_overrides() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let fake_home = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env_remove("OODA_STATE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", fake_home.path())
        .args(["doc-review", "--pr-id", "753"])
        .assert()
        .success();

    let path = fake_home
        .path()
        .canonicalize()
        .unwrap()
        .join(".local")
        .join("state")
        .join("ooda")
        .join("753")
        .join("doc_review_attest.json");
    assert!(path.exists(), "attestation not at {}", path.display());
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn doc_review_idempotent_second_run_overwrites_and_advances_timestamp() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["doc-review", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let path = expected_doc_review_path(state_root.path(), "7");
    let first = read_attestation(&path);
    let first_at = first["attested_at"].as_str().unwrap().to_string();

    std::thread::sleep(std::time::Duration::from_millis(1100));

    bin()
        .current_dir(repo.path())
        .args(["doc-review", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let second = read_attestation(&path);
    let second_at = second["attested_at"].as_str().unwrap();

    assert_eq!(
        first["attested_sha"].as_str().unwrap(),
        second["attested_sha"].as_str().unwrap()
    );
    assert_ne!(first_at, second_at);
    assert!(second_at > first_at.as_str());
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

// ── closeout subcommand ────────────────────────────────────────────

#[test]
fn closeout_happy_path_writes_attestation_for_head() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["closeout", "--pr-id", "42", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();

    let path = expected_closeout_path(state_root.path(), "42");
    assert!(
        path.exists(),
        "closeout attestation not written at {}",
        path.display()
    );
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
    assert_eq!(json["version"].as_u64().unwrap(), 1);
    assert!(json["attested_at"].as_str().is_some());
}

#[test]
fn closeout_invalid_pull_request_id_exits_64() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["closeout", "--pr-id", "abc", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("--pr-id"));
}

#[test]
fn closeout_not_a_git_repo_exits_64() {
    // See `not_a_git_repo_exits_64` — same reasoning.
    let non_repo = TempDir::new().unwrap();
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(non_repo.path())
        .args(["closeout", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .assert()
        .failure()
        .code(64)
        .stderr(contains("not inside a git working tree"));
}

#[test]
fn closeout_ooda_pr_state_home_env_var_supplies_default_state_root() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env("OODA_STATE_HOME", state_root.path())
        .env_remove("XDG_STATE_HOME")
        .args(["closeout", "--pr-id", "753"])
        .assert()
        .success();

    let path = expected_closeout_path(state_root.path(), "753");
    assert!(path.exists(), "attestation not at {}", path.display());
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn closeout_home_fallback_supplies_default_state_root_when_no_env_overrides() {
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let fake_home = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .env_remove("OODA_STATE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", fake_home.path())
        .args(["closeout", "--pr-id", "753"])
        .assert()
        .success();

    let path = fake_home
        .path()
        .canonicalize()
        .unwrap()
        .join(".local")
        .join("state")
        .join("ooda")
        .join("753")
        .join("closeout_attest.json");
    assert!(path.exists(), "attestation not at {}", path.display());
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

// ── --repo-root: spoofing regression (F6 saturation) ──────────────

#[test]
fn repo_root_flag_pins_head_sha_against_cwd_spoof() {
    // Regression: without `current_dir()` on the `git rev-parse
    // HEAD` subprocess, invoking ooda-attest from sibling repo A
    // with --pr-id N would write A's HEAD SHA into the attestation
    // file under <state-root>/N/, even though the operator
    // intended the attestation to bind repo B. Two repos with
    // overlapping PR numbers could clobber each other with
    // spoofed SHAs — defeating the integrity property the binary
    // exists to provide.
    let repo_a = TempDir::new().unwrap();
    let head_a = init_git_repo_with_content(repo_a.path(), b"repo-a\n", "repo-a init");

    let repo_b = TempDir::new().unwrap();
    let head_b = init_git_repo_with_content(repo_b.path(), b"repo-b\n", "repo-b init");

    assert_ne!(head_a, head_b, "test premise: HEAD SHAs must differ");

    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo_a.path())
        .args(["pr-meta", "--pr-id", "42", "--state-root"])
        .arg(state_root.path())
        .args(["--repo-root"])
        .arg(repo_b.path())
        .assert()
        .success();

    let path = expected_path(state_root.path(), "42");
    let json = read_attestation(&path);
    let attested = json["attested_sha"].as_str().unwrap();
    assert_eq!(
        attested, head_b,
        "--repo-root must pin SHA to repo_b's HEAD, not the CWD repo_a"
    );
    assert_ne!(
        attested, head_a,
        "spoof path: CWD's HEAD must NOT have leaked through"
    );
}

#[test]
fn repo_root_flag_canonicalizes_relative_indirection() {
    // `--repo-root <path>/./` should canonicalize to `<path>`;
    // the SHA captured must be the repo's HEAD regardless of
    // surface form.
    let repo = TempDir::new().unwrap();
    let head = init_git_repo(repo.path());
    let other = TempDir::new().unwrap();
    init_git_repo_with_content(other.path(), b"other\n", "other init");
    let state_root = TempDir::new().unwrap();

    let indirect = repo.path().join(".");

    bin()
        .current_dir(other.path())
        .args(["pr-meta", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .args(["--repo-root"])
        .arg(&indirect)
        .assert()
        .success();

    let path = expected_path(state_root.path(), "1");
    let json = read_attestation(&path);
    assert_eq!(json["attested_sha"].as_str().unwrap(), head);
}

#[test]
fn repo_root_flag_nonexistent_path_exits_64() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();
    let bogus = repo.path().join("nope-not-a-repo");

    bin()
        .current_dir(repo.path())
        .args(["pr-meta", "--pr-id", "1", "--state-root"])
        .arg(state_root.path())
        .args(["--repo-root"])
        .arg(&bogus)
        .assert()
        .failure()
        .code(64)
        .stderr(contains("--repo-root"));
}

#[test]
fn closeout_idempotent_second_run_overwrites_and_advances_timestamp() {
    let repo = TempDir::new().unwrap();
    init_git_repo(repo.path());
    let state_root = TempDir::new().unwrap();

    bin()
        .current_dir(repo.path())
        .args(["closeout", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let path = expected_closeout_path(state_root.path(), "7");
    let first = read_attestation(&path);
    let first_at = first["attested_at"].as_str().unwrap().to_string();

    std::thread::sleep(std::time::Duration::from_millis(1100));

    bin()
        .current_dir(repo.path())
        .args(["closeout", "--pr-id", "7", "--state-root"])
        .arg(state_root.path())
        .assert()
        .success();
    let second = read_attestation(&path);
    let second_at = second["attested_at"].as_str().unwrap();

    assert_eq!(
        first["attested_sha"].as_str().unwrap(),
        second["attested_sha"].as_str().unwrap()
    );
    assert_ne!(first_at, second_at);
    assert!(second_at > first_at.as_str());
}
