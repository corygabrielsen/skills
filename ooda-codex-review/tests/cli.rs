//! End-to-end binary contract tests for the Phase 7 CLI surface.
//!
//! Coverage:
//!   * `--help` short-circuits to exit 0 (regardless of position)
//!   * Mode flag validation: exactly one of
//!     {--uncommitted, --base, --commit, --pr} required
//!   * Per-flag value parsing (--level, -n, --max-iter)
//!   * Unknown args → exit 64
//!   * Smoke: end-to-end RunReviews with a missing `codex` binary
//!     surfaces as `BinaryError` (exit 6)

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ooda-codex-review");

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN).args(args).output().expect("spawn");
    (
        out.status.code().expect("no exit code"),
        String::from_utf8(out.stdout).expect("stdout not utf-8"),
        String::from_utf8(out.stderr).expect("stderr not utf-8"),
    )
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// ----- --help short-circuit ---------------------------------------------

#[test]
fn help_long_exits_zero_via_stdout() {
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0, "stderr was: {stderr}");
    assert!(
        stdout.starts_with("ooda-codex-review"),
        "stdout: {stdout:?}"
    );
    assert_eq!(stderr, "", "--help must not write to stderr");
}

#[test]
fn help_short_exits_zero_via_stdout() {
    let (code, stdout, stderr) = run(&["-h"]);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("ooda-codex-review"));
    assert_eq!(stderr, "");
}

#[test]
fn help_short_circuits_other_validation() {
    let (code, _, _) = run(&["--bogus", "--help"]);
    assert_eq!(code, 0);
}

// ----- mode flag validation --------------------------------------------

#[test]
fn no_mode_flag_is_usage_error() {
    let (code, stdout, stderr) = run(&[]);
    assert_eq!(code, 64, "stderr={stderr}");
    assert_eq!(stdout, "");
    assert!(first_line(&stderr).starts_with("UsageError: "));
    assert!(stderr.contains("--uncommitted"));
}

#[test]
fn multiple_mode_flags_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--base", "master"]);
    assert_eq!(code, 64);
    assert!(first_line(&stderr).starts_with("UsageError: "));
}

#[test]
fn unknown_arg_is_usage_error() {
    let (code, stdout, stderr) = run(&["--bogus"]);
    assert_eq!(code, 64);
    assert_eq!(stdout, "");
    let header = first_line(&stderr);
    assert!(header.starts_with("UsageError: "), "header: {header:?}");
    assert!(!header.contains('\n'), "header must be single-line");
}

#[test]
fn random_positional_is_usage_error() {
    let (code, _, stderr) = run(&["owner/repo"]);
    assert_eq!(code, 64);
    assert!(first_line(&stderr).starts_with("UsageError: "));
}

// ----- per-flag parsing ------------------------------------------------

#[test]
fn invalid_level_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--level", "max"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--level"));
}

#[test]
fn negative_n_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "-n", "-3"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("-n"));
}

#[test]
fn zero_n_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "-n", "0"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("-n"));
}

#[test]
fn zero_max_iter_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--max-iter", "0"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--max-iter"));
}

#[test]
fn invalid_pr_number_is_usage_error() {
    let (code, _, stderr) = run(&["--pr", "abc"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--pr"));
}

#[test]
fn ceiling_below_floor_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--level", "high", "--ceiling", "low"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--ceiling"));
    assert!(stderr.contains("--level"));
}

#[test]
fn invalid_ceiling_value_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--ceiling", "max"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--ceiling"));
}

#[test]
fn fresh_with_side_effect_is_usage_error() {
    let (code, _, stderr) = run(&["--uncommitted", "--fresh", "--mark-retro-clean"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--fresh"));
    assert!(stderr.contains("side-effect"));
}

#[test]
fn criteria_is_usage_error_until_codex_cli_supports_target_prompts() {
    let (code, _, stderr) = run(&["--uncommitted", "--criteria", "check auth"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--criteria"));
    assert!(stderr.contains("not supported"));
}

// ----- end-to-end smoke ------------------------------------------------

#[test]
fn missing_codex_binary_surfaces_as_binary_error() {
    // Repo discovery succeeds (cargo test runs inside the skills/
    // git repo). RunReviews tries to spawn the missing binary; the
    // spawn error propagates as Outcome::BinaryError (exit 6). The
    // batch_dir path goes under TMPDIR so we don't pollute the
    // user's state.
    let state_root =
        std::env::temp_dir().join(format!("ooda-codex-review-cli-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_root);

    let (code, _, stderr) = run(&[
        "--uncommitted",
        "--codex-bin",
        "/nonexistent/codex-bin-for-test",
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "1",
    ]);
    assert_eq!(code, 6, "stderr={stderr}");
    assert!(first_line(&stderr).starts_with("BinaryError: "));
    let _ = std::fs::remove_dir_all(&state_root);
}

// ----- end-to-end with a fake codex ------------------------------------

#[test]
#[cfg(unix)]
fn end_to_end_with_fake_codex_halts_on_address_batch() {
    // The fake codex script writes a complete log block (the ^codex$
    // marker plus a verdict body that classify() recognizes as
    // HasIssues). The loop should: spawn, observe Complete, decide
    // AddressBatch, and halt with HandoffAgent.
    use std::os::unix::fs::PermissionsExt;

    let state_root =
        std::env::temp_dir().join(format!("ooda-codex-review-e2e-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_root);
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_codex = state_root.join("fake-codex.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          printf 'thinking\\n  reasoning...\\nexec\\n  ran cmd\\ncodex\\n\
Review comment: src/foo.rs:42\\nSQL injection detected.\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("OODA_AWAIT_SECS", "1"); // 1s between observations
    cmd.args([
        "--uncommitted",
        "-n",
        "1", // single review keeps the test deterministic
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let out = cmd.output().expect("spawn");
    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert_eq!(code, 5, "expected HandoffAgent (5); stderr={stderr}");
    assert_eq!(first_line(&stderr), "HandoffAgent: AddressBatch");
    assert!(stderr.contains("Verify and address"), "stderr: {stderr}");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
#[cfg(unix)]
fn end_to_end_with_fake_codex_clean_below_ceiling_halts_on_retro() {
    // Fake codex emits a clean verdict ("No issues found"). With
    // current = floor = low and ceiling = xhigh (default), the loop
    // should halt on Retrospective (HandoffAgent).
    use std::os::unix::fs::PermissionsExt;

    let state_root = std::env::temp_dir().join(format!(
        "ooda-codex-review-e2e-clean-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_root);
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_codex = state_root.join("fake-codex-clean.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          printf 'thinking\\n  reasoning...\\ncodex\\nNo issues found\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("OODA_AWAIT_SECS", "1");
    cmd.args([
        "--uncommitted",
        "-n",
        "1",
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let out = cmd.output().expect("spawn");
    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert_eq!(code, 5, "expected HandoffAgent (5); stderr={stderr}");
    assert_eq!(first_line(&stderr), "HandoffAgent: Retrospective");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
#[cfg(unix)]
fn end_to_end_with_fake_codex_clean_at_ceiling_halts_done_fixed_point() {
    // Same fake codex (clean), but --level xhigh --ceiling xhigh.
    // The loop should detect ceiling-fixed-point and emit
    // DoneFixedPoint (exit 0).
    use std::os::unix::fs::PermissionsExt;

    let state_root = std::env::temp_dir().join(format!(
        "ooda-codex-review-e2e-ceiling-done-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_root);
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_codex = state_root.join("fake-codex-clean.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          printf 'thinking\\n  reasoning...\\ncodex\\nNo issues found\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("OODA_AWAIT_SECS", "1");
    cmd.args([
        "--uncommitted",
        "--level",
        "xhigh",
        "--ceiling",
        "xhigh",
        "-n",
        "1",
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let out = cmd.output().expect("spawn");
    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert_eq!(code, 0, "expected DoneFixedPoint (0); stderr={stderr}");
    assert_eq!(first_line(&stderr), "DoneFixedPoint");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
#[cfg(unix)]
fn pr_mode_resolves_base_branch_before_spawning_codex() {
    use std::os::unix::fs::PermissionsExt;

    let state_root = std::env::temp_dir().join(format!(
        "ooda-codex-review-e2e-pr-resolve-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_root);
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_gh = state_root.join("gh");
    std::fs::write(
        &fake_gh,
        b"#!/bin/sh\n\
          if [ \"$1 $2 $3\" != \"pr view 42\" ]; then exit 9; fi\n\
          printf 'main\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let fake_codex = state_root.join("fake-codex-pr.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          saw_base=0\n\
          saw_pr=0\n\
          prev=''\n\
          for arg in \"$@\"; do\n\
            if [ \"$arg\" = '--pr' ]; then saw_pr=1; fi\n\
            if [ \"$prev\" = '--base' ] && [ \"$arg\" = 'main' ]; then saw_base=1; fi\n\
            prev=\"$arg\"\n\
          done\n\
          if [ \"$saw_pr\" = 1 ] || [ \"$saw_base\" = 0 ]; then\n\
            printf 'bad argv: %s\\n' \"$*\" >&2\n\
            exit 12\n\
          fi\n\
          printf 'thinking\\n  reasoning...\\ncodex\\nNo issues found\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", state_root.display());
    let mut cmd = Command::new(BIN);
    cmd.env("PATH", path);
    cmd.env("OODA_AWAIT_SECS", "1");
    cmd.args([
        "--pr",
        "42",
        "--level",
        "xhigh",
        "--ceiling",
        "xhigh",
        "-n",
        "1",
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let out = cmd.output().expect("spawn");
    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert_eq!(code, 0, "expected DoneFixedPoint (0); stderr={stderr}");
    assert_eq!(first_line(&stderr), "DoneFixedPoint");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
#[cfg(unix)]
fn codex_usage_error_exit_file_surfaces_as_binary_error() {
    use std::os::unix::fs::PermissionsExt;

    let state_root = std::env::temp_dir().join(format!(
        "ooda-codex-review-e2e-codex-exit-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_root);
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_codex = state_root.join("fake-codex-exit.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          printf 'error: unexpected argument --pr\\n' >&2\n\
          exit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = Command::new(BIN);
    cmd.env("OODA_AWAIT_SECS", "1");
    cmd.args([
        "--uncommitted",
        "-n",
        "1",
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let out = cmd.output().expect("spawn");
    let code = out.status.code().expect("no exit code");
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert_eq!(code, 6, "expected BinaryError (6); stderr={stderr}");
    assert!(stderr.contains("slot 1 exited 2"), "stderr: {stderr}");

    let _ = std::fs::remove_dir_all(&state_root);
}

// ----- --mark-* side effects -------------------------------------------

fn fresh_state_root(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ooda-codex-review-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn mark_retro_clean_at_ceiling_emits_done_fixed_point() {
    let state_root = fresh_state_root("mark-retro-ceiling");

    // Seed a run at level=xhigh, ceiling=xhigh by spawning with a
    // missing codex bin (creates the run dir + manifest, errors on
    // spawn but recorder is already open).
    let _ = run(&[
        "--uncommitted",
        "--level",
        "xhigh",
        "--ceiling",
        "xhigh",
        "--codex-bin",
        "/nonexistent/codex-bin-for-test",
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "1",
    ]);

    // Now mark the retrospective clean. At ceiling → DoneFixedPoint.
    let (code, stdout, stderr) = run(&[
        "--uncommitted",
        "--level",
        "xhigh",
        "--ceiling",
        "xhigh",
        "--state-root",
        state_root.to_str().unwrap(),
        "--mark-retro-clean",
    ]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("fixed point reached"), "stdout: {stdout:?}");
    assert_eq!(first_line(&stderr), "DoneFixedPoint");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
fn mark_retro_clean_below_ceiling_advances_and_idles() {
    let state_root = fresh_state_root("mark-retro-advance");

    let _ = run(&[
        "--uncommitted",
        "--level",
        "low",
        "--codex-bin",
        "/nonexistent/codex-bin-for-test",
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "1",
    ]);

    let (code, stdout, _) = run(&[
        "--uncommitted",
        "--level",
        "low",
        "--state-root",
        state_root.to_str().unwrap(),
        "--mark-retro-clean",
    ]);
    assert_eq!(code, 7, "expected Idle"); // Idle = 7
    assert!(stdout.contains("advanced to medium"), "stdout: {stdout:?}");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
fn mark_address_failed_emits_handoff_human() {
    let state_root = fresh_state_root("mark-address-fail");

    let _ = run(&[
        "--uncommitted",
        "--codex-bin",
        "/nonexistent/codex-bin-for-test",
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "1",
    ]);

    let (code, _stdout, stderr) = run(&[
        "--uncommitted",
        "--state-root",
        state_root.to_str().unwrap(),
        "--mark-address-failed",
        "test_signup_flow failed: expected 200, got 500",
    ]);
    assert_eq!(code, 3, "expected HandoffHuman"); // HandoffHuman = 3
    assert_eq!(first_line(&stderr), "HandoffHuman: TestsFailedTriage");
    assert!(
        stderr.contains("test_signup_flow failed"),
        "stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
#[cfg(unix)]
fn mark_address_passed_at_floor_moves_to_next_batch() {
    use std::os::unix::fs::PermissionsExt;

    let state_root = fresh_state_root("mark-address-floor-next-batch");
    std::fs::create_dir_all(&state_root).unwrap();

    let fake_codex = state_root.join("fake-codex-issue.sh");
    std::fs::write(
        &fake_codex,
        b"#!/bin/sh\n\
          printf 'thinking\\n  reasoning...\\ncodex\\n\
Review comment: src/foo.rs:42\\nRegression detected.\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut seed = Command::new(BIN);
    seed.env("OODA_AWAIT_SECS", "1");
    seed.args([
        "--uncommitted",
        "-n",
        "1",
        "--codex-bin",
        fake_codex.to_str().unwrap(),
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "10",
    ]);
    let seed_out = seed.output().expect("spawn");
    assert_eq!(seed_out.status.code().unwrap(), 5);

    let (code, stdout, stderr) = run(&[
        "--uncommitted",
        "--state-root",
        state_root.to_str().unwrap(),
        "--mark-address-passed",
    ]);
    assert_eq!(code, 7, "stderr={stderr}");
    assert!(stdout.contains("advanced to batch 2"), "stdout: {stdout:?}");

    let _ = std::fs::remove_dir_all(&state_root);
}

#[test]
fn side_effect_flags_are_mutually_exclusive() {
    let (code, _, stderr) = run(&["--uncommitted", "--mark-retro-clean", "--advance-level"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("mutually exclusive"));
}

#[test]
fn mark_retro_changes_requires_a_reason() {
    let (code, _, stderr) = run(&["--uncommitted", "--mark-retro-changes"]);
    assert_eq!(code, 64);
    assert!(stderr.contains("--mark-retro-changes"));
}

#[test]
fn fresh_flag_creates_a_new_run() {
    // Run once with a missing codex bin (exits fast with
    // BinaryError but still creates the run dir + latest pointer).
    // Then re-run with --fresh and confirm the latest pointer
    // changed. We don't test the *resume* path here because it
    // would hit a real AwaitReviews 30s wait (the partial log from
    // the first failed spawn looks like an in-flight review). The
    // recorder unit tests cover resume thoroughly.
    let state_root = std::env::temp_dir().join(format!(
        "ooda-codex-review-cli-fresh-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_root);

    let common: Vec<&str> = vec![
        "--uncommitted",
        "--codex-bin",
        "/nonexistent/codex-bin-for-test",
        "--state-root",
        state_root.to_str().unwrap(),
        "--max-iter",
        "1",
    ];

    let (_, _, _) = run(&common);
    let target_root_glob = std::fs::read_dir(&state_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("uncommitted");
    let latest_path = target_root_glob.join("latest");
    let first_id = std::fs::read_to_string(&latest_path).unwrap();

    let mut fresh_args = common.clone();
    fresh_args.push("--fresh");
    let (_, _, _) = run(&fresh_args);
    let after_fresh = std::fs::read_to_string(&latest_path).unwrap();
    assert_ne!(
        after_fresh, first_id,
        "--fresh should create a new run; latest pointer should change"
    );

    let _ = std::fs::remove_dir_all(&state_root);
}
