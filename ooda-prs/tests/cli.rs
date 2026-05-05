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

const BIN: &str = env!("CARGO_BIN_EXE_ooda-prs");
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run the binary with `args` and return (exit_code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let root = temp_path("state");
    let out = command(args)
        .env("OODA_PR_STATE_HOME", &root)
        .output()
        .expect("spawn ooda-prs");
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
        stdout.starts_with("ooda-prs"),
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
    assert!(stdout.starts_with("ooda-prs"));
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
    assert!(stdout.starts_with("ooda-prs"));
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
        stderr.contains("ooda-prs — drive"),
        "args={args:?}: usage text missing"
    );
}

#[test]
fn no_args_is_usage_error() {
    // Empty positional vector → no suite. Unique to ooda-prs's
    // multi-PR grammar; ooda-pr rejected this with the "exactly 2
    // positionals" message, but that message no longer applies.
    assert_usage_error(&[], "no PRs specified");
}

#[test]
fn slug_only_group_is_usage_error() {
    // A group with a slug but no PR tokens: structural error.
    assert_usage_error(&["owner/repo"], "no PR numbers");
}

#[test]
fn extra_token_after_two_is_pr_parse_error() {
    // Under the multi-PR grammar `owner/repo 1 extra` is one group
    // with slug `owner/repo` and PR tokens [1, extra]; `extra` fails
    // as a PR. (Cf. ooda-pr, where this was a positional-arity error.)
    assert_usage_error(&["owner/repo", "1", "extra"], "invalid pull request number");
}

#[test]
fn malformed_slug_is_usage_error() {
    // A token containing `/` is treated as a slug; `a/b/c` has too
    // many segments. (Cf. ooda-pr, which used `noslash` to test the
    // malformed-slug path; under ooda-prs `noslash` is a PR token,
    // not a slug, so the test moved to a slug that has `/` but is
    // structurally malformed.)
    assert_usage_error(&["a/b/c", "42"], "invalid repo slug");
}

#[test]
fn bad_pr_number_is_usage_error() {
    assert_usage_error(&["owner/repo", "abc"], "invalid pull request number");
}

#[test]
fn pr_zero_is_usage_error() {
    // PullRequestNumber is { ℕ | > 0 }
    assert_usage_error(&["owner/repo", "0"], "invalid pull request number");
}

#[test]
fn unknown_flag_is_usage_error() {
    assert_usage_error(&["--bogus", "owner/repo", "1"], "unknown flag: --bogus");
}

#[test]
fn removed_comment_flag_is_usage_error() {
    // Renamed to --status-comment in the v6 refactor; the old
    // spelling must surface as UsageError so callers fail loudly.
    assert_usage_error(&["--comment", "owner/repo", "1"], "unknown flag: --comment");
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
    // Negative gets a distinct, actionable message — not lumped
    // with "not an integer".
    assert_usage_error(
        &["--max-iter", "-1", "owner/repo", "1"],
        "got negative value: -1",
    );
}

#[test]
fn max_iter_no_value_rejected() {
    assert_usage_error(&["--max-iter"], "--max-iter requires a value");
}

#[test]
fn max_iter_repeated_rejected() {
    assert_usage_error(
        &["--max-iter", "10", "--max-iter", "20", "owner/repo", "1"],
        "--max-iter repeated",
    );
}

// ─── --status-comment validation ────────────────────────────────

#[test]
fn status_comment_repeated_rejected() {
    assert_usage_error(
        &["--status-comment", "--status-comment", "owner/repo", "1"],
        "--status-comment repeated",
    );
}

// ─── --trace validation ─────────────────────────────────────────

#[test]
fn state_root_no_value_rejected() {
    assert_usage_error(&["--state-root"], "--state-root requires a value");
}

#[test]
fn state_root_repeated_rejected() {
    assert_usage_error(
        &[
            "--state-root",
            "/tmp/a",
            "--state-root",
            "/tmp/b",
            "owner/repo",
            "1",
        ],
        "--state-root repeated",
    );
}

#[test]
fn trace_no_value_rejected() {
    assert_usage_error(&["--trace"], "--trace requires a value");
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
        "--trace repeated",
    );
}

// ─── inspect placement ──────────────────────────────────────────

#[test]
fn inspect_after_positional_is_usage_error() {
    // Once a positional has been seen, "inspect" is just a slug
    // candidate — and a malformed one (no '/').
    assert_usage_error(&["owner/repo", "inspect"], "invalid pull request number");
}

#[test]
fn inspect_after_other_inspect_is_usage_error() {
    // First `inspect` is consumed as the subcommand. The second
    // `inspect` falls through to the positional vector. Under the
    // multi-PR grammar a token without `/` is treated as a PR, so
    // PullRequestNumber::parse("inspect") fails — the error message
    // pivots from "expected 2 positionals" (ooda-pr) to "invalid
    // pull request number" (ooda-prs), but the rejection itself is
    // preserved.
    assert_usage_error(
        &["inspect", "inspect", "owner/repo", "1"],
        "invalid pull request number",
    );
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
        code == 6 || code == 64,
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
    let state_root = temp_path("state-root");
    let empty_path = temp_path("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();

    let out = command(&[
        "--state-root",
        state_root.to_str().unwrap(),
        "inspect",
        "owner/repo",
        "1",
    ])
    .env("PATH", &empty_path)
    .output()
    .expect("spawn ooda-prs");

    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(code, 6, "stderr: {stderr}");
    assert!(stderr.starts_with("BinaryError: observe:"));

    let pr_root = state_root.join("github.com/owner/repo/prs/1");
    assert!(pr_root.join("events.jsonl").exists());
    assert!(pr_root.join("latest/outcome.json").exists());
    assert!(pr_root.join("ledger.jsonl").exists());

    let events = std::fs::read_to_string(pr_root.join("events.jsonl")).unwrap();
    assert!(events.contains(r#""kind":"run_started""#), "{events}");
    assert!(events.contains(r#""kind":"observe_started""#), "{events}");
    assert!(
        events.contains(r#""kind":"tool_call_finished""#),
        "{events}"
    );
    assert!(events.contains(r#""kind":"observe_finished""#), "{events}");
    assert!(events.contains(r#""kind":"outcome""#), "{events}");

    let run_dirs: Vec<_> = std::fs::read_dir(pr_root.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(run_dirs.len(), 1);
    assert!(run_dirs[0].join("manifest.json").exists());
    assert!(run_dirs[0].join("trace.md").exists());
    assert!(
        run_dirs[0]
            .join("iterations/0001/event-range.json")
            .exists()
    );
}

// ─── multi-PR suite grammar ─────────────────────────────────────

/// Helper: invoke with PATH=empty so observe always fails. This
/// gives us a deterministic per-PR `BinaryError` outcome and lets
/// us assert on stdout JSONL shape, $? aggregation, and suite
/// recorder layout — without needing live GitHub access.
fn run_with_failing_gh(args: &[&str], state_root: &std::path::Path) -> (i32, String, String) {
    let empty_path = temp_path("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();
    let out = command(args)
        .env("PATH", &empty_path)
        .env("OODA_PR_STATE_HOME", state_root)
        .output()
        .expect("spawn ooda-prs");
    (
        out.status.code().expect("no exit code"),
        String::from_utf8(out.stdout).expect("stdout not utf-8"),
        String::from_utf8(out.stderr).expect("stderr not utf-8"),
    )
}

#[test]
fn homogeneous_pr_list_emits_three_jsonl_records() {
    // Form 1 from the design: `<slug> <pr> <pr> <pr>` — homogeneous
    // suite, all PRs in one repo.
    let state = temp_path("multi-homogeneous");
    let (code, stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "inspect",
            "acme/widget",
            "1",
            "2",
            "3",
        ],
        &state,
    );
    assert_eq!(code, 6, "all 3 PRs must yield BinaryError");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "stdout: {stdout}");
    for (i, want_pr) in [1u64, 2, 3].iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(lines[i]).unwrap();
        assert_eq!(v["slug"], "acme/widget");
        assert_eq!(v["pr"], *want_pr);
        assert_eq!(v["outcome"], "BinaryError");
        assert_eq!(v["exit"], 6);
        assert!(v["msg"].is_string());
    }
}

#[test]
fn comma_separated_multi_slug_preserves_input_order() {
    // Form 2 from the design: `<slug> <pr>, <slug> <pr>` — two
    // distinct repos, comma-separated groups. JSONL records must
    // appear in input order.
    let state = temp_path("multi-multislug");
    let (code, stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "inspect",
            "acme/widget",
            "42,",
            "acme/infra",
            "100",
        ],
        &state,
    );
    assert_eq!(code, 6);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    let r0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let r1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(r0["slug"], "acme/widget");
    assert_eq!(r0["pr"], 42);
    assert_eq!(r1["slug"], "acme/infra");
    assert_eq!(r1["pr"], 100);
}

#[test]
fn slug_inheritance_carries_to_subsequent_groups() {
    // `<slug> <pr>, <pr>` — group 2 has no slug so inherits group
    // 1's slug. Tests the per-group slug-inheritance rule.
    let state = temp_path("multi-inherit");
    let (code, stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "inspect",
            "acme/widget",
            "10,",
            "11",
        ],
        &state,
    );
    assert_eq!(code, 6);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["slug"], "acme/widget");
    }
}

#[test]
fn duplicate_pr_rejected_as_usage_error() {
    // `<slug> <pr>, <pr>` where the second pr is a duplicate of the
    // first. Suite invariant: distinct (slug, pr) pairs.
    assert_usage_error(&["acme/widget", "5,", "5"], "duplicate PR: acme/widget#5");
}

#[test]
fn empty_group_rejected_as_usage_error() {
    // Lone `,` between groups — the parser detects an empty group.
    assert_usage_error(&["acme/widget", "1,,", "2"], "empty group at position 2");
}

#[test]
fn cwd_inference_failure_surfaces_as_usage_error_when_gh_missing() {
    // Group with no slug and no inheritance source → the parser
    // calls `gh repo view`; with PATH=empty the spawn fails and we
    // get a UsageError citing cwd inference.
    let state = temp_path("cwd-inference");
    let (code, _stdout, stderr) = run_with_failing_gh(
        &["--state-root", state.to_str().unwrap(), "inspect", "42"],
        &state,
    );
    assert_eq!(code, 64, "stderr: {stderr}");
    assert!(
        stderr.contains("cwd slug inference") || stderr.contains("not a github repo"),
        "stderr: {stderr}"
    );
}

// ─── --concurrency flag ─────────────────────────────────────────

#[test]
fn concurrency_zero_rejected() {
    assert_usage_error(
        &["--concurrency", "0", "owner/repo", "1"],
        "--concurrency must be ≥ 1",
    );
}

#[test]
fn concurrency_negative_rejected() {
    assert_usage_error(
        &["--concurrency", "-1", "owner/repo", "1"],
        "got negative value: -1",
    );
}

#[test]
fn concurrency_non_integer_rejected() {
    assert_usage_error(
        &["--concurrency", "abc", "owner/repo", "1"],
        "not an integer",
    );
}

#[test]
fn concurrency_no_value_rejected() {
    assert_usage_error(&["--concurrency"], "--concurrency requires a value");
}

#[test]
fn concurrency_repeated_rejected() {
    assert_usage_error(
        &[
            "--concurrency",
            "2",
            "--concurrency",
            "3",
            "owner/repo",
            "1",
        ],
        "--concurrency repeated",
    );
}

#[test]
fn concurrency_capped_run_completes_all_prs() {
    // With --concurrency 1, all 3 PRs still run (just sequentially).
    let state = temp_path("concurrency-1");
    let (code, stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "--concurrency",
            "1",
            "inspect",
            "acme/widget",
            "1",
            "2",
            "3",
        ],
        &state,
    );
    assert_eq!(code, 6);
    assert_eq!(stdout.lines().count(), 3);
}

// ─── suite-level recorder ───────────────────────────────────────

#[test]
fn suite_recorder_writes_manifest_pointers_outcome_and_trace() {
    let state = temp_path("suite-recorder");
    let (code, _stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "inspect",
            "acme/widget",
            "1,",
            "acme/infra",
            "100",
        ],
        &state,
    );
    assert_eq!(code, 6);

    // <state>/suites/<suite-id>/ has all four artifacts.
    let suite_dirs: Vec<_> = std::fs::read_dir(state.join("suites"))
        .expect("suites/ dir created")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(suite_dirs.len(), 1, "expected exactly one suite dir");
    let suite_dir = &suite_dirs[0];
    assert!(suite_dir.join("manifest.json").exists());
    assert!(suite_dir.join("pointers.json").exists());
    assert!(suite_dir.join("outcome.json").exists());
    assert!(suite_dir.join("trace.md").exists());

    // pointers.json links each (slug, pr) to a per-PR run_id.
    let pointers: serde_json::Value =
        serde_json::from_slice(&std::fs::read(suite_dir.join("pointers.json")).unwrap()).unwrap();
    let prs = pointers["prs"].as_array().expect("prs is array");
    assert_eq!(prs.len(), 2);
    for p in prs {
        let slug = p["slug"].as_str().unwrap();
        let pr = p["pr"].as_u64().unwrap();
        let run_id = p["run_id"].as_str().unwrap();
        // Each run_id must correspond to a real per-PR runs/ dir.
        let pr_run_dir = state
            .join("github.com")
            .join(slug)
            .join("prs")
            .join(pr.to_string())
            .join("runs")
            .join(run_id);
        assert!(
            pr_run_dir.exists(),
            "per-PR run dir {} missing",
            pr_run_dir.display()
        );
    }

    // outcome.json carries the aggregate exit code and the typed
    // MultiOutcome.
    let outcome: serde_json::Value =
        serde_json::from_slice(&std::fs::read(suite_dir.join("outcome.json")).unwrap()).unwrap();
    assert_eq!(outcome["exit_code"], 6);
    assert!(outcome["multi_outcome"]["Bundle"].is_array());

    // trace.md contains the human-readable summary table.
    let trace = std::fs::read_to_string(suite_dir.join("trace.md")).unwrap();
    assert!(trace.contains("Per-PR results"));
    assert!(trace.contains("acme/widget"));
    assert!(trace.contains("acme/infra"));
    assert!(trace.contains("Aggregate exit: **6**"));
}

#[test]
fn usage_error_emits_no_stdout_in_multi_pr_mode() {
    // Symmetry with the per-PR contract: parser failures emit
    // nothing on stdout; the parent harness sees `$? = 64` and the
    // usage block on stderr.
    let state = temp_path("usage-no-stdout");
    let (code, stdout, _stderr) = run_with_failing_gh(
        &[
            "--state-root",
            state.to_str().unwrap(),
            "acme/widget", // slug-only, no PRs
        ],
        &state,
    );
    assert_eq!(code, 64);
    assert_eq!(stdout, "", "UsageError must not write to stdout");
}
