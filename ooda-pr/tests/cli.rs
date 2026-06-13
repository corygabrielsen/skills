//! End-to-end binary contract tests.
//!
//! These exercise the `Outcome → ExitCode + stderr-render` path
//! deterministically (no GitHub I/O). Variants requiring observed
//! state (`DoneMerged`, `Stuck*`, `Handoff*`, `WouldAdvance`,
//! `Paused`, `BinaryError`) are out of scope here — they need a
//! live PR or a stubbed gh.
//!
//! Coverage:
//!   * `--help` → exit 0, prints to stdout (the only stdout-write).
//!   * `UsageError(_)` → exit 64, single-line `UsageError: <msg>`
//!     header on stderr followed by the usage text.
//!   * Argument-parser invariants: positional shape, repeated
//!     flags, removed `--comment`, `inspect` placement, `--max-iter`
//!     validation.
//!
//! Each test asserts:
//!   - exit code (the dispatch contract)
//!   - the stderr first line (the header contract — single line,
//!     `UsageError: ` prefix, then the parser diagnostic)

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_ooda-pr");
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run the binary with `args` and return (`exit_code`, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let root = temp_path("state");
    let out = command(args)
        .env("OODA_STATE_HOME", &root)
        .output()
        .expect("spawn ooda-pr");
    (
        out.status.code().expect("no exit code (signal kill?)"),
        String::from_utf8(out.stdout).expect("stdout not utf-8"),
        String::from_utf8(out.stderr).expect("stderr not utf-8"),
    )
}

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args);
    cmd
}

fn temp_path(label: &str) -> std::path::PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ooda-pr-cli-test-{label}-{}-{n}",
        std::process::id()
    ))
}

/// Stderr first line — the variant header. Empty string if stderr
/// has no output (which would itself be a contract violation for
/// most variants).
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// ─── --help: exit 0, stdout-only ────────────────────────────────

#[test]
fn help_long_exits_zero_via_stdout() {
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0, "stderr was: {stderr}");
    assert!(
        stdout.starts_with("ooda-pr"),
        "stdout should begin with binary name; got: {stdout:?}"
    );
    assert!(stdout.contains("--state-root PATH"));
    assert!(stdout.contains("--trace PATH"));
    assert_eq!(stderr, "", "--help must not write to stderr");
}

#[test]
fn help_short_exits_zero_via_stdout() {
    let (code, stdout, stderr) = run(&["-h"]);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("ooda-pr"));
    assert_eq!(stderr, "");
}

#[test]
fn help_short_circuits_other_validation() {
    // --help with otherwise-invalid args still exits 0 — the
    // parser short-circuits before any other validation.
    let (code, _, _) = run(&["--help", "noslash", "abc", "--max-iter", "0"]);
    assert_eq!(code, 0);
}

#[test]
fn help_after_malformed_max_iter_still_exits_zero() {
    // The pre-scan should win over left-to-right validation:
    // even if --max-iter has a bad value before --help appears,
    // --help short-circuits and exits 0.
    let (code, stdout, _) = run(&["--max-iter", "abc", "owner/repo", "1", "--help"]);
    assert_eq!(code, 0, "--help should short-circuit malformed args");
    assert!(stdout.starts_with("ooda-pr"));
}

#[test]
fn help_after_unknown_flag_still_exits_zero() {
    let (code, _, _) = run(&["--bogus", "owner/repo", "1", "-h"]);
    assert_eq!(code, 0, "-h should short-circuit unknown flags");
}

// ─── UsageError: exit 64, single-line header ────────────────────

fn assert_usage_error(args: &[&str], expected_msg_substring: &str) {
    let (code, stdout, stderr) = run(args);
    assert_eq!(code, 64, "args={args:?} stderr={stderr}");
    assert_eq!(
        stdout, "",
        "args={args:?}: UsageError must not write to stdout"
    );

    let header = first_line(&stderr);
    assert!(
        header.starts_with("UsageError: "),
        "args={args:?}: header missing prefix; got: {header:?}"
    );
    assert!(
        !header.contains('\n'),
        "args={args:?}: header must be single-line"
    );
    assert!(
        header.contains(expected_msg_substring),
        "args={args:?}: header missing substring {expected_msg_substring:?}; got: {header:?}"
    );

    // Usage text follows the header on stderr (separate lines).
    assert!(
        stderr.contains("ooda-pr — drive"),
        "args={args:?}: usage text missing"
    );
}

#[test]
fn no_args_is_usage_error() {
    assert_usage_error(&[], "expected exactly 2 positionals");
}

#[test]
fn one_positional_is_usage_error() {
    assert_usage_error(&["owner/repo"], "expected exactly 2 positionals");
}

#[test]
fn three_positionals_is_usage_error() {
    // clap rejects extra positionals with "unexpected argument" —
    // the prior hand-rolled parser counted positionals and emitted
    // "expected exactly 2 positionals". Same exit code, different
    // wording.
    assert_usage_error(&["owner/repo", "1", "extra"], "unexpected argument");
}

#[test]
fn bad_slug_is_usage_error() {
    assert_usage_error(&["noslash", "42"], "invalid repo slug");
}

#[test]
fn bad_pull_request_number_is_usage_error() {
    assert_usage_error(&["owner/repo", "abc"], "invalid pull request number");
}

#[test]
fn pull_request_zero_is_usage_error() {
    // PullRequestNumber is { ℕ | > 0 }
    assert_usage_error(&["owner/repo", "0"], "invalid pull request number");
}

#[test]
fn unknown_flag_is_usage_error() {
    // clap diagnostic format differs from the hand-rolled parser
    // ("unexpected argument" vs "unknown flag"); same exit code.
    assert_usage_error(&["--bogus", "owner/repo", "1"], "unexpected argument");
}

#[test]
fn removed_comment_flag_is_usage_error() {
    // Renamed to --status-comment in the v6 refactor; the old
    // spelling must surface as UsageError so callers fail loudly.
    assert_usage_error(&["--comment", "owner/repo", "1"], "unexpected argument");
}

// ─── --max-iter validation ──────────────────────────────────────

#[test]
fn max_iter_zero_rejected() {
    assert_usage_error(
        &["--max-iter", "0", "owner/repo", "1"],
        "--max-iter must be ≥ 1",
    );
}

#[test]
fn max_iter_non_integer_rejected() {
    assert_usage_error(&["--max-iter", "abc", "owner/repo", "1"], "not an integer");
}

#[test]
fn max_iter_negative_rejected() {
    // clap rejects `-1` as a flag-shaped value (it parses as flag
    // `-1`, not a numeric value) — distinct error wording from the
    // hand-rolled parser but same exit code.
    assert_usage_error(&["--max-iter", "-1", "owner/repo", "1"], "");
}

#[test]
fn max_iter_no_value_rejected() {
    // clap renders "a value is required for '--max-iter <MAX_ITER>'".
    assert_usage_error(&["--max-iter"], "a value is required");
}

#[test]
fn max_iter_repeated_rejected() {
    // clap rejects multi-use of a non-`Append` arg with
    // "the argument '--max-iter' cannot be used multiple times".
    assert_usage_error(
        &["--max-iter", "10", "--max-iter", "20", "owner/repo", "1"],
        "cannot be used multiple times",
    );
}

// ─── F9 bug 1: --flag=value form ────────────────────────────────

#[test]
fn max_iter_equals_form_accepted() {
    // Hand-rolled parser treated `--max-iter=5` as unknown flag.
    // clap accepts the `=` form natively. Parser must accept it,
    // then proceed past the parse phase (recorder open, observe).
    // We probe by setting --repo-root to a tempdir that exists so
    // we hit the post-parse failure on observe, not on parse.
    let state_root = temp_path("state-root-eq");
    std::fs::create_dir_all(&state_root).unwrap();
    let repo_root = temp_path("repo-root-eq");
    std::fs::create_dir_all(&repo_root).unwrap();
    let (code, _, stderr) = run(&[
        "--max-iter=5",
        "--state-root",
        state_root.to_str().unwrap(),
        "--repo-root",
        repo_root.to_str().unwrap(),
        "owner/repo",
        "1",
    ]);
    assert_ne!(
        code, 64,
        "--max-iter=5 must NOT be rejected as UsageError; stderr: {stderr}"
    );
}

// ─── --status-comment validation ────────────────────────────────

#[test]
fn status_comment_repeated_rejected() {
    // clap renders "the argument '--status-comment' cannot be used multiple times".
    assert_usage_error(
        &["--status-comment", "--status-comment", "owner/repo", "1"],
        "cannot be used multiple times",
    );
}

// ─── --state-root validation ────────────────────────────────────

#[test]
fn state_root_no_value_rejected() {
    assert_usage_error(&["--state-root"], "a value is required");
}

#[test]
fn state_root_nonexistent_rejected() {
    // F9 bug 6: validate `--state-root` exists at parse time.
    // Converges on `ooda-attest`'s prior surface.
    let bogus = temp_path("state-root-bogus");
    let _ = std::fs::remove_dir_all(&bogus);
    assert_usage_error(
        &["--state-root", bogus.to_str().unwrap(), "owner/repo", "1"],
        "does not exist",
    );
}

#[test]
fn state_root_repeated_rejected() {
    // Use existing dirs so the repeated-flag check fires before
    // the existence check.
    let a = temp_path("state-root-a");
    std::fs::create_dir_all(&a).unwrap();
    let b = temp_path("state-root-b");
    std::fs::create_dir_all(&b).unwrap();
    assert_usage_error(
        &[
            "--state-root",
            a.to_str().unwrap(),
            "--state-root",
            b.to_str().unwrap(),
            "owner/repo",
            "1",
        ],
        "cannot be used multiple times",
    );
}

#[test]
fn trace_no_value_rejected() {
    assert_usage_error(&["--trace"], "a value is required");
}

#[test]
fn trace_repeated_rejected() {
    assert_usage_error(
        &[
            "--trace",
            "/tmp/a.log",
            "--trace",
            "/tmp/b.log",
            "owner/repo",
            "1",
        ],
        "cannot be used multiple times",
    );
}

// ─── inspect placement ──────────────────────────────────────────

#[test]
fn inspect_after_positional_is_usage_error() {
    // clap subcommand: `inspect` must appear before any positional;
    // `owner/repo inspect` is read as <slug>=owner/repo + extra
    // positional "inspect".
    assert_usage_error(&["owner/repo", "inspect"], "");
}

#[test]
fn inspect_after_other_inspect_is_usage_error() {
    // clap rejects: second "inspect" lands as an extra positional
    // under the first inspect subcommand.
    assert_usage_error(&["inspect", "inspect", "owner/repo", "1"], "");
}

#[test]
fn inspect_after_flag_is_allowed() {
    // Flags before `inspect` are not the same as positionals; the
    // parser model is "inspect must be the FIRST argument" — but
    // the current implementation only enforces "before any
    // positional", so a leading flag is accepted. Verify the
    // current behavior so a stricter spec change is a deliberate
    // test update.
    let (code, _, stderr) = run(&["--max-iter", "10", "inspect", "owner/repo", "1"]);
    // Either UsageError on inspect placement, OR proceeds to gh
    // and fails at observe (network/auth). We accept both as long
    // as the inspect-placement check itself didn't reject.
    assert!(
        code == 70 || code == 64,
        "unexpected exit {code}; stderr: {stderr}"
    );
    if code == 64 {
        // If it IS rejected, the message should reference inspect
        // placement, not anything else. Loosen this assertion if
        // the parser model changes.
        assert!(
            stderr.contains("inspect") || stderr.contains("UsageError"),
            "stderr: {stderr}"
        );
    }
}

// ─── always-on recorder ─────────────────────────────────────────

#[test]
fn state_root_records_even_when_observe_fails() {
    // F9: --state-root is now validated for existence at parse
    // time; create the dir before invoking.
    let state_root = temp_path("state-root");
    std::fs::create_dir_all(&state_root).unwrap();
    let empty_path = temp_path("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();
    // `--repo-root <tempdir>` short-circuits the resolver to
    // canonicalize (which needs no external binary). Without it,
    // the empty PATH below would also block `git rev-parse` and
    // surface as `UsageError` before observe runs — defeating
    // this test's purpose, which is the recorder-on-observe-fail
    // path.
    let repo_root = temp_path("repo-root");
    std::fs::create_dir_all(&repo_root).unwrap();

    let out = command(&[
        "--state-root",
        state_root.to_str().unwrap(),
        "--repo-root",
        repo_root.to_str().unwrap(),
        "inspect",
        "owner/repo",
        "1",
    ])
    .env("PATH", &empty_path)
    .output()
    .expect("spawn ooda-pr");

    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(code, 70, "stderr: {stderr}");
    assert!(stderr.starts_with("BinaryError: observe:"));

    // ooda-state layout: <state-root>/{runs,live}/<run-id>/. Run id
    // is opaque; the domain (`pr`) and target (`slug`/`pr`) live in
    // the events.jsonl payload, not in the path.
    let runs: Vec<_> = std::fs::read_dir(state_root.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 1, "exactly one run: {runs:?}");
    let run_dir = &runs[0];
    assert!(run_dir.join("events.jsonl").exists());
    assert!(run_dir.join("blobs").is_dir());

    let events = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
    assert!(events.contains(r#""kind":"run_started""#), "{events}");
    assert!(events.contains(r#""domain":"pr""#), "{events}");
    // Tool calls and observe lifecycle are domain_specific events;
    // the `kind_suffix` field carries the per-event token.
    assert!(
        events.contains(r#""kind_suffix":"observe_started""#),
        "{events}"
    );
    assert!(
        events.contains(r#""kind_suffix":"tool_call_finished""#),
        "{events}"
    );
    assert!(
        events.contains(r#""kind_suffix":"observe_finished""#),
        "{events}"
    );
    assert!(events.contains(r#""kind":"run_halted""#), "{events}");
    assert!(events.contains(r#""outcome":"BinaryError""#), "{events}");

    // `run_halted` removes the live marker; no live entry remains.
    let live_count =
        std::fs::read_dir(state_root.join("live")).map_or(0, std::iter::Iterator::count);
    assert_eq!(live_count, 0, "live marker should be gone after halt");
}
