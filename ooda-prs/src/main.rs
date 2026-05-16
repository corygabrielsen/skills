use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod act;
mod comment;
mod dashboard;
mod decide;
mod ids;
mod multi_outcome;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod suite;
mod suite_recorder;
mod text;

use dashboard::Dashboard;
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::candidates;
use decide::decision::{Decision, DecisionHalt};
use ids::{PullRequestNumber, RepoSlug};
use multi_outcome::{MultiOutcome, ProcessOutcome};
use observe::github::{FetchOutcome, fetch_all};
use ooda_core::decide_from_candidates;
use orient::orient;
use outcome::Outcome;
use recorder::{Recorder, RecorderConfig, RunMode};
use runner::{LoopConfig, current_timestamp, run_loop};
use suite_recorder::{SuiteRecorder, SuiteRecorderConfig};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-prs — drive N PRs through observe → orient → decide → act until each halts.\n\
         \n\
         Usage:\n  ooda-prs [options] <suite>            run the loop on every PR in <suite>\n  ooda-prs inspect [options] <suite>    one pass per PR; print MultiOutcome; exit\n\
         \n\
         Suite grammar:\n  <suite>      ::= <group> ( ',' <group> )*\n  <group>      ::= <owner/repo>? <pr>+\n  <owner/repo>  — explicit slug for this group; if omitted, inherit from the prior\n                  group, else infer from cwd (`gh repo view --json nameWithOwner`).\n  <pr>          — positive integer.\n  Examples:\n    ooda-prs 42 45                              # cwd-slug, two PRs\n    ooda-prs acme/widget 42 43, acme/infra 100  # multi-slug; comma separates groups\n    ooda-prs acme/widget 42, 43                 # group 2 inherits acme/widget\n\
         \n\
         Options:\n  --max-iter N         loop iteration cap per PR (default 50, must be ≥ 1; ignored by inspect)\n  --concurrency K      max in-flight PRs (default = |suite|, must be ≥ 1)\n  --status-comment     post a status comment on each PR every iteration (deduped)\n  --state-root PATH    write always-on harness state under PATH\n  --trace PATH         also append the compact trace to PATH\n  -h, --help           show this help and exit\n\
         \n\
         Exit codes — aggregate priority projection over per-PR Outcomes:\n   0 all DoneMerged/DoneClosed/Paused (no further action)\n   1 (unused at suite level — Paused folds into 0)\n   2 any WouldAdvance\n   3 any HandoffHuman\n   4 any HandoffAgent\n   5 (unused at suite level — DoneClosed folds into 0)\n   6 any StuckRepeated\n   7 any StuckCapReached\n  64 UsageError\n  70 any BinaryError\n  (130 SIGINT, 143 SIGTERM reserved)\n\
         Priority order (highest first): UsageError > BinaryError > HandoffAgent > HandoffHuman > StuckCapReached > StuckRepeated > WouldAdvance > terminal."
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loop,
    Inspect,
}

struct Args {
    mode: Mode,
    /// The parsed suite: a non-empty, deduplicated `Vec<(slug, pr)>`
    /// in input order. Each pair is driven by its own `run_loop`
    /// (sequentially in this stage; parallelized later).
    suite: Vec<(RepoSlug, PullRequestNumber)>,
    max_iter: std::num::NonZeroU32,
    status_comment: bool,
    state_root: Option<PathBuf>,
    trace: Option<PathBuf>,
    /// Optional cap on concurrent in-flight PRs. `None` resolves
    /// to `|suite|` at the spawn loop (no cap). Enforced by
    /// `suite::drive_suite` via an `AtomicUsize` work index.
    concurrency: Option<u32>,
}

/// Parse CLI args. On failure, returns `Outcome::UsageError(_)` so
/// the boundary always speaks Outcome — no exception path.
///
/// `-h` / `--help` short-circuits **before** any other validation:
/// a pre-scan checks every argument for the help flag; if present
/// anywhere (including after a malformed `--max-iter` etc.), usage
/// is printed to stdout and the process exits 0. This matches the
/// SKILL.md promise that `--help` is honored regardless of position.
fn parse_args() -> Result<Args, ooda_core::SingleLineString> {
    // Pre-scan: --help wins over any other parse failure.
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print_usage(&mut std::io::stdout());
        std::process::exit(0);
    }

    let mut mode = Mode::Loop;
    let mut max_iter: std::num::NonZeroU32 = std::num::NonZeroU32::new(50).expect("50 is non-zero");
    let mut status_comment = false;
    let mut state_root: Option<PathBuf> = None;
    let mut trace: Option<PathBuf> = None;
    let mut concurrency: Option<u32> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut saw_subcommand = false;
    let mut saw_max_iter = false;
    let mut saw_status_comment = false;
    let mut saw_state_root = false;
    let mut saw_trace = false;
    let mut saw_concurrency = false;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                // Unreachable — pre-scan above caught these. Kept
                // as defense-in-depth in case the pre-scan is ever
                // restructured.
                print_usage(&mut std::io::stdout());
                std::process::exit(0);
            }
            "--status-comment" => {
                if saw_status_comment {
                    return Err(usage("--status-comment repeated"));
                }
                saw_status_comment = true;
                status_comment = true;
            }
            "--max-iter" => {
                if saw_max_iter {
                    return Err(usage("--max-iter repeated"));
                }
                saw_max_iter = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--max-iter requires a value"));
                };
                // Distinguish three rejection cases for actionable error
                // messages: negative (sign-prefix check), non-numeric
                // (parse failure), and zero (parsed but invalid).
                // The validated value flows out as `NonZeroU32` so the
                // runner's "iter 1 always runs" invariant is structural.
                if v.starts_with('-') {
                    return Err(usage(&format!(
                        "--max-iter must be ≥ 1; got negative value: {v}"
                    )));
                }
                let Ok(n) = v.parse::<u32>() else {
                    return Err(usage(&format!("--max-iter: not an integer: {v}")));
                };
                let Some(n) = std::num::NonZeroU32::new(n) else {
                    return Err(usage("--max-iter must be ≥ 1; got 0"));
                };
                max_iter = n;
            }
            "--trace" => {
                if saw_trace {
                    return Err(usage("--trace repeated"));
                }
                saw_trace = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--trace requires a value"));
                };
                trace = Some(PathBuf::from(v));
            }
            "--state-root" => {
                if saw_state_root {
                    return Err(usage("--state-root repeated"));
                }
                saw_state_root = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--state-root requires a value"));
                };
                state_root = Some(PathBuf::from(v));
            }
            "--concurrency" => {
                if saw_concurrency {
                    return Err(usage("--concurrency repeated"));
                }
                saw_concurrency = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--concurrency requires a value"));
                };
                if v.starts_with('-') {
                    return Err(usage(&format!(
                        "--concurrency must be ≥ 1; got negative value: {v}"
                    )));
                }
                let Ok(n) = v.parse::<u32>() else {
                    return Err(usage(&format!("--concurrency: not an integer: {v}")));
                };
                if n == 0 {
                    return Err(usage("--concurrency must be ≥ 1; got 0"));
                }
                concurrency = Some(n);
            }
            "inspect" if !saw_subcommand && positional.is_empty() => {
                mode = Mode::Inspect;
                saw_subcommand = true;
            }
            _ if arg.starts_with("--") => {
                return Err(usage(&format!("unknown flag: {arg}")));
            }
            _ => positional.push(arg),
        }
    }

    let suite = parse_suite(&positional)?;

    Ok(Args {
        mode,
        suite,
        max_iter,
        status_comment,
        state_root,
        trace,
        concurrency,
    })
}

/// Parse positional tokens into a non-empty, deduplicated suite of
/// `(slug, pr)` pairs.
///
/// Grammar: `<group> ( ',' <group> )*` where `<group> ::= <slug>?
/// <pr>+`. A `<slug>` is detected by the presence of `'/'` in the
/// token; integers (any non-`/` token) are PR numbers.
///
/// Slug resolution: a group without an explicit slug inherits from
/// the prior group's slug (within the same invocation). The very
/// first group, if it has no explicit slug, falls back to inferring
/// the cwd's repository via `gh repo view --json nameWithOwner`.
///
/// Errors map to `Outcome::UsageError` so the parser path stays
/// total-over-Argv.
fn parse_suite(
    positional: &[String],
) -> Result<Vec<(RepoSlug, PullRequestNumber)>, ooda_core::SingleLineString> {
    if positional.is_empty() {
        return Err(usage(
            "no PRs specified; expected <owner/repo>? <pr>+ (',' <owner/repo>? <pr>+)*",
        ));
    }

    // Joining with spaces and re-splitting on ',' handles all comma
    // surface forms uniformly: `42, 43`, `42 ,43`, `42,43`, and the
    // shell-tokenized `42,` then `43`. Each split segment is then
    // whitespace-tokenized into individual <slug>?<pr>+ tokens.
    let joined: String = positional.join(" ");
    let group_strs: Vec<&str> = joined.split(',').map(str::trim).collect();

    let mut suite: Vec<(RepoSlug, PullRequestNumber)> = Vec::new();
    let mut last_slug: Option<RepoSlug> = None;

    for (idx, group_str) in group_strs.iter().enumerate() {
        if group_str.is_empty() {
            return Err(usage(&format!(
                "empty group at position {} (commas must separate non-empty groups)",
                idx + 1
            )));
        }
        let tokens: Vec<&str> = group_str.split_whitespace().collect();
        // group_str non-empty after trim implies tokens non-empty.

        let (slug, pr_tokens) = if tokens[0].contains('/') {
            let slug = RepoSlug::parse(tokens[0]).map_err(|e| usage(&e.to_string()))?;
            (slug, &tokens[1..])
        } else {
            let slug = match &last_slug {
                Some(s) => s.clone(),
                None => infer_cwd_slug().map_err(|e| usage(&e))?,
            };
            (slug, &tokens[..])
        };

        if pr_tokens.is_empty() {
            return Err(usage(&format!(
                "group {} has slug {slug} but no PR numbers",
                idx + 1
            )));
        }
        for pr_token in pr_tokens {
            let pr = PullRequestNumber::parse(pr_token).map_err(|e| usage(&e.to_string()))?;
            if suite.iter().any(|(s, p)| s == &slug && *p == pr) {
                return Err(usage(&format!("duplicate PR: {slug}#{pr}")));
            }
            suite.push((slug.clone(), pr));
        }
        last_slug = Some(slug);
    }

    Ok(suite)
}

/// Infer the cwd's repository slug via `gh repo view --json
/// nameWithOwner --jq .nameWithOwner`. Used only when the first
/// suite group has no explicit slug. Failures (no gh, not a repo,
/// non-UTF-8 stdout, malformed slug) all flatten to a single
/// human-readable string per the `UsageError` newline-free invariant.
fn infer_cwd_slug() -> Result<RepoSlug, String> {
    let out = std::process::Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .output()
        .map_err(|e| format!("cwd slug inference: spawn `gh` failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        return Err(format!(
            "cwd is not a github repo; specify <owner/repo> explicitly (gh exit {code}: {})",
            stderr.trim().replace('\n', " ")
        ));
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| format!("cwd slug inference: stdout not UTF-8: {e}"))?;
    let trimmed = stdout.trim();
    RepoSlug::parse(trimmed).map_err(|e| format!("cwd slug parse from gh stdout {trimmed:?}: {e}"))
}

/// Construct a parser-stage usage diagnostic. Typed as
/// `SingleLineString` (not `Outcome`) so `parse_args` can return
/// `Result<Args, SingleLineString>` — the call site lifts the
/// message into both `Outcome::UsageError` (for stderr) and
/// `MultiOutcome::UsageError` (for the suite-level exit code)
/// without a runtime `unreachable!()` on a `match Outcome`.
fn usage(msg: &str) -> ooda_core::SingleLineString {
    msg.into()
}

fn main() -> ExitCode {
    let multi = match parse_args() {
        Ok(args) => {
            // Open the suite-level Recorder before any PR thread
            // spawns so the manifest exists at audit time even if a
            // worker panics mid-run.
            let suite_recorder = match SuiteRecorder::open(SuiteRecorderConfig {
                suite: args.suite.clone(),
                mode: run_mode(args.mode),
                max_iter: args.max_iter,
                status_comment: args.status_comment,
                state_root: args.state_root.clone(),
                concurrency: args.concurrency,
            }) {
                Ok(r) => Some(r),
                Err(e) => {
                    // Suite-recorder open failure is not fatal: per-
                    // PR recorders still run, so the per-PR ledgers
                    // exist. Surface the failure on stderr so it's
                    // visible to the operator, then proceed.
                    eprintln!("warning: suite recorder open failed: {e}");
                    None
                }
            };

            // Parallel per-PR dispatch under `thread::scope`. Each
            // thread runs `drive_one_pr` which opens its own per-PR
            // Recorder, installs it as the thread-local tool-call
            // sink, runs the observe/orient/decide/act pipeline,
            // renders the per-PR variant block on stderr, and
            // records the outcome on its own Recorder. The aggregate
            // exit code is the typed priority projection on
            // `MultiOutcome` — see `multi_outcome.rs`.
            //
            // Cross-thread isolation:
            //   • `THREAD_RECORDER` is thread-local so PR_i's tool
            //     calls cannot land in PR_j's ledger.
            //   • Each PR's `Recorder` is `Arc<Mutex<_>>`-backed for
            //     its own internal serialization; only one thread
            //     ever holds it.
            //   • `SuiteRecorder` is `Arc<Mutex<_>>`-backed; worker
            //     threads call `register_pr` after their per-PR
            //     Recorder has emitted its `run_id`.
            //   • `run_loop`'s stall-detection state
            //     (`last_non_wait`, `last_attempted`) is local to
            //     the worker stack frame.
            let process_outcomes = suite::drive_suite(&args.suite, args.concurrency, |slug, pr| {
                drive_one_pr(slug, pr, &args, suite_recorder.as_ref())
            });
            let multi = MultiOutcome::Bundle(process_outcomes);
            // Stdout — the agent-harness contract. One JSONL record
            // per PR, in input order. Stderr remains for human
            // triage; `$?` remains the coarse dispatch signal.
            render_multi_jsonl(&mut std::io::stdout(), &multi);
            // Finalize suite recorder: writes outcome.json + appends
            // the per-PR summary table to trace.md.
            if let Some(rec) = suite_recorder.as_ref() {
                rec.record_outcome(&multi, multi.exit_code());
            }
            multi
        }
        Err(usage_msg) => {
            // `parse_args` returns the diagnostic message directly
            // (typed `SingleLineString`); lift it into the per-binary
            // `Outcome::UsageError` for stderr formatting and into
            // `MultiOutcome::UsageError` for the suite-level exit
            // code. No `match` on `Outcome` is needed — the
            // structural narrowing eliminates the prior
            // `unreachable!()`.
            let outcome: Outcome = Outcome::UsageError(usage_msg.clone());
            render_outcome(&mut std::io::stderr(), &outcome);
            MultiOutcome::UsageError(usage_msg)
        }
    };
    ExitCode::from(multi.exit_code())
}

/// Drive a single PR end-to-end: open a Recorder keyed by `(slug,
/// pr)`, install it as the thread-local tool-call sink, register
/// the PR's `run_id` with the suite recorder, run the configured
/// mode (`Loop` or `Inspect`), render the resulting `Outcome` to
/// stderr, and record the outcome to the per-PR ledger. Returns
/// the `Outcome` for the suite-level aggregator.
fn drive_one_pr(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    args: &Args,
    suite_recorder: Option<&SuiteRecorder>,
) -> Outcome {
    let recorder = match Recorder::open(RecorderConfig {
        slug: slug.clone(),
        pr,
        mode: run_mode(args.mode),
        max_iter: args.max_iter,
        status_comment: args.status_comment,
        state_root: args.state_root.clone(),
        legacy_trace: args.trace.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            // No recorder was opened for this PR; render to stderr
            // so the suite-level summary still observes the failure.
            let outcome = Outcome::binary_error(format!("recorder: {e}"));
            render_outcome(&mut std::io::stderr(), &outcome);
            return outcome;
        }
    };
    recorder.install_process_recorder();
    if let Some(sr) = suite_recorder {
        sr.register_pr(slug, pr, &recorder.run_id());
    }
    let outcome = match args.mode {
        Mode::Inspect => run_inspect(slug, pr, args, &recorder),
        Mode::Loop => run_full(slug, pr, args, &recorder),
    };
    let code = outcome.exit_code();
    render_outcome(&mut std::io::stderr(), &outcome);
    let mut rendered = Vec::new();
    render_outcome(&mut rendered, &outcome);
    if let Ok(text) = String::from_utf8(rendered) {
        for line in text.lines() {
            recorder.write_trace_line(line);
        }
    }
    recorder.record_outcome(&outcome, code);
    outcome
}

fn run_mode(mode: Mode) -> RunMode {
    match mode {
        Mode::Loop => RunMode::Loop,
        Mode::Inspect => RunMode::Inspect,
    }
}

fn run_inspect(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    args: &Args,
    recorder: &Recorder,
) -> Outcome {
    recorder.set_iteration(Some(1));
    recorder.record_observe_start(1);
    let obs = match fetch_all(slug, pr) {
        Ok(FetchOutcome::Observations(o)) => {
            recorder.record_observe_end(1, Ok(()));
            *o
        }
        Ok(FetchOutcome::RateLimited(hit)) => {
            // Rate-limited mid-inspect: no orient/decide possible
            // (we have no observations). Surface the synthetic
            // WaitForRateLimit through the same Outcome::from
            // pipeline as any other Execute decision so wrappers
            // see a `WouldAdvance` exit code with this action's
            // payload — exactly what the full-loop runner would
            // dispatch on iter 1.
            let line = format!(
                "rate-limited on {}; would wait {}s",
                hit.scope.name(),
                hit.retry_after.as_duration().as_secs(),
            );
            eprintln!("{line}");
            recorder.write_trace_line(&line);
            recorder.record_observe_end(1, Ok(()));
            let action = rate_limit_wait_action(hit);
            return Outcome::from(Decision::Execute(action));
        }
        Err(e) => {
            recorder.record_observe_end(1, Err(e.to_string()));
            return Outcome::binary_error(format!("observe: {e}"));
        }
    };
    if obs.stack_root_branch != obs.pr_view.base_ref_name {
        // Diagnostic note when the PR's immediate base differs
        // from the stack root used for branch-rule lookups. The
        // suffix repeated `<root>` was redundant; dropped to make
        // the line match the documented `stack: <base> → <root>`
        // format exactly.
        let line = format!(
            "stack: {} → {}",
            obs.pr_view.base_ref_name, obs.stack_root_branch,
        );
        eprintln!("{line}");
        recorder.write_trace_line(&line);
    }
    let oriented = orient(&obs, None, current_timestamp());
    let candidate_actions = candidates(&oriented);
    let decision = decide_from_candidates(candidate_actions.clone(), obs.pr_view.state);
    recorder.record_iteration(1, &obs, &oriented, &candidate_actions, &decision);
    if args.status_comment {
        let rendered =
            comment::render::render(slug, pr, Some(1), &oriented, &candidate_actions, &decision);
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(1));
        log_post_result("comment", true, r, Some(recorder));
    }
    let snapshot = HandoffSnapshot {
        oriented: oriented.clone(),
        head_short: obs.pr_view.head_ref_oid.as_str().chars().take(7).collect(),
        base_branch: obs.pr_view.base_ref_name.to_string(),
        dashboard: Dashboard::from_iteration(&oriented, &candidate_actions, &decision),
    };
    decorate_handoff_human(Outcome::from(decision), slug, pr, Some(&snapshot))
}

fn run_full(slug: &RepoSlug, pr: PullRequestNumber, args: &Args, recorder: &Recorder) -> Outcome {
    let cfg = LoopConfig {
        max_iterations: args.max_iter,
    };
    let mut snapshot: Option<HandoffSnapshot> = None;
    let on_state = |i: u32,
                    obs: &observe::github::GitHubObservations,
                    oriented: &orient::OrientedState,
                    candidate_actions: &[decide::action::Action],
                    d: &Decision| {
        snapshot = Some(HandoffSnapshot {
            oriented: oriented.clone(),
            head_short: obs.pr_view.head_ref_oid.as_str().chars().take(7).collect(),
            base_branch: obs.pr_view.base_ref_name.to_string(),
            dashboard: Dashboard::from_iteration(oriented, candidate_actions, d),
        });
        recorder.set_iteration(Some(i));
        recorder.record_iteration(i, obs, oriented, candidate_actions, d);
        let line = iteration_line(i, d);
        eprintln!("{line}");
        recorder.write_trace_line(&line);
        if args.status_comment {
            let rendered =
                comment::render::render(slug, pr, Some(i), oriented, candidate_actions, d);
            recorder.record_status_comment_rendered(
                Some(i),
                &rendered,
                format!("[iter {i}] comment rendered"),
            );
            let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(i));
            log_post_result(&format!("[iter {i}] comment"), false, r, Some(recorder));
        }
    };
    let outcome = match run_loop(slug, pr, cfg, recorder, on_state) {
        Ok(reason) => Outcome::from(reason),
        Err(e) => Outcome::from(e),
    };
    decorate_handoff_human(outcome, slug, pr, snapshot.as_ref())
}

fn log_post_result(
    prefix: &str,
    verbose_skip: bool,
    r: Result<bool, comment::post::PostError>,
    recorder: Option<&Recorder>,
) {
    let line = post_result_line(prefix, verbose_skip, r);
    if let Some(line) = line {
        eprintln!("{line}");
        if let Some(recorder) = recorder {
            recorder.write_trace_line(&line);
        }
    }
}

fn post_result_line(
    prefix: &str,
    verbose_skip: bool,
    r: Result<bool, comment::post::PostError>,
) -> Option<String> {
    match r {
        Ok(true) => Some(format!("{prefix}: posted")),
        Ok(false) if verbose_skip => Some(format!("{prefix}: skipped (unchanged)")),
        Ok(false) => None,
        // Flatten newlines so the comment-log line stays single-line
        // — GhError::NonZero etc. don't strip embedded newlines from
        // gh's stderr, so a multi-line error would otherwise break the
        // implied one-line-per-comment-event contract documented in
        // SKILL.md.
        Err(e) => Some(format!(
            "{prefix}: {}",
            ooda_core::SingleLineString::new(e.to_string())
        )),
    }
}

fn iteration_line(i: u32, d: &Decision) -> String {
    match d {
        Decision::Execute(action) => {
            format!(
                "[iter {i}] {} ({}) blocker: {}",
                action.kind.name(),
                format_effect(&action.effect),
                action.blocker,
            )
        }
        Decision::Halt(halt) => {
            // Use halt.name() (finite token set) instead of {:?}
            // so the per-iteration halt line stays single-line
            // and bounded — Debug would expand AgentNeeded(Action {
            // effect: ActionEffect::Agent { prompt: ... } }) into the
            // action payload, which breaks the
            // one-line-per-iteration invariant.
            match halt_blocker(halt) {
                Some(blocker) => format!(
                    "[iter {i}] halt: {} blocker: {}",
                    DecisionHalt::name(halt),
                    blocker,
                ),
                None => format!("[iter {i}] halt: {}", DecisionHalt::name(halt)),
            }
        }
    }
}

fn halt_blocker(halt: &DecisionHalt) -> Option<&ooda_core::BlockerKey> {
    match halt {
        DecisionHalt::AgentNeeded(handoff) | DecisionHalt::HumanNeeded(handoff) => {
            Some(&handoff.blocker)
        }
        DecisionHalt::Success | DecisionHalt::Terminal(_) => None,
    }
}

/// Snapshot of the per-iteration state that the human-handoff
/// decorator needs after `run_loop` returns. Captured from the last
/// `on_state` callback so the post-loop decorator can render the PR
/// link + a short situational summary without re-observing.
///
/// `dashboard` carries the Phase-B preamble payload (tier-grouped
/// candidates, per-axis signals, blockers). Constructed at the
/// boundary from the same `(oriented, candidates, decision)` triple
/// the recorder uses — option (a) from the spec, kept just-in-time
/// so no new thread is plumbed through the runner.
#[derive(Debug, Clone)]
struct HandoffSnapshot {
    oriented: orient::OrientedState,
    head_short: String,
    base_branch: String,
    dashboard: Dashboard,
}

fn pr_url(slug: &RepoSlug, pr: PullRequestNumber) -> String {
    format!("https://github.com/{slug}/pull/{pr}")
}

// Rebase is `HandoffAgent`, not `HandoffHuman` — `ActionEffect::Agent`
// projects to `Outcome::HandoffAgent` in `classify()`. The boundary
// decorator must cover both classes of outcome where the situational
// context (PR URL, branch, CI snapshot) is useful to whoever picks up
// the prompt. As more `HandoffAgent` actions need the same context,
// add them to `agent_action_needs_context` rather than spawning a
// sibling decorator per kind.
/// Append a PR-context block to handoff prompts so the stderr
/// hand-off is usable on its own — no tab-juggling. Covers every
/// `HandoffHuman` and the `HandoffAgent` variants whose recipient
/// also needs the situational frame. Pass-through for every other
/// `Outcome` variant.
///
/// Two layers of decoration:
/// * The dashboard preamble (Phase B) — universal across every
///   `HandoffHuman` and `HandoffAgent` outcome. Prepended to the
///   prompt's sections so the recipient sees tier-grouped
///   candidates, per-axis signals, and blockers before the
///   per-action body.
/// * The per-action context block (5bf9c7c) — gated by the
///   `agent_action_needs_context` allowlist. Appended via
///   `push_handoff_context` after the existing prompt body so
///   `HandoffHuman` and allowlisted `HandoffAgent` recipients pick
///   up PR URL / branch / CI / reviews on the trailing edge.
fn decorate_handoff_human(
    outcome: Outcome,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    snapshot: Option<&HandoffSnapshot>,
) -> Outcome {
    match outcome {
        Outcome::HandoffHuman(mut action) => {
            prepend_dashboard_preamble(&mut action, snapshot);
            push_handoff_context(&mut action, slug, pr, snapshot);
            Outcome::HandoffHuman(action)
        }
        Outcome::HandoffAgent(mut action) => {
            prepend_dashboard_preamble(&mut action, snapshot);
            if agent_action_needs_context(&action.kind) {
                push_handoff_context(&mut action, slug, pr, snapshot);
            }
            Outcome::HandoffAgent(action)
        }
        other => other,
    }
}

/// Prepend the dashboard preamble sections (tier-grouped
/// candidates, per-axis signals, blockers) to the handoff prompt's
/// sections vec. No-op when the snapshot is absent (e.g. usage
/// errors that surface a synthetic handoff without ever entering
/// the iteration loop) or when the dashboard projects no
/// candidates (terminal halts already render an empty preamble).
fn prepend_dashboard_preamble(
    handoff: &mut ooda_core::HandoffAction<decide::action::ActionKind>,
    snapshot: Option<&HandoffSnapshot>,
) {
    let Some(snap) = snapshot else {
        return;
    };
    let preamble = snap.dashboard.render_handoff_preamble();
    if preamble.is_empty() {
        return;
    }
    let mut sections = preamble;
    sections.extend(std::mem::take(&mut handoff.prompt.sections));
    handoff.prompt.sections = sections;
}

/// `HandoffAgent` actions whose prompts benefit from the same
/// PR / branch / CI context the `HandoffHuman` decorator appends.
/// Today: `Rebase` (returning human triages the merge state).
/// Add new variants here rather than open-coding the match at the
/// call site.
fn agent_action_needs_context(kind: &decide::action::ActionKind) -> bool {
    matches!(kind, decide::action::ActionKind::Rebase)
}

/// Append the boundary context (PR URL, blocker, branch, CI,
/// reviews) onto the handoff prompt. `HandoffAction` exposes
/// `prompt` as a direct field, so there's no inner `match` on
/// `ActionEffect` and no `unreachable!()` arm — the structural
/// projection done in `classify()` carries the invariant.
fn push_handoff_context(
    handoff: &mut ooda_core::HandoffAction<decide::action::ActionKind>,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    snapshot: Option<&HandoffSnapshot>,
) {
    let blocker = handoff.blocker.to_string();
    let prompt = &mut handoff.prompt;
    prompt.push_context_line("PR", pr_url(slug, pr));
    prompt.push_context_line("Blocker", blocker);
    if let Some(snap) = snapshot {
        prompt.push_context_line(
            "Branch",
            format!("{} ← {}", snap.base_branch, snap.head_short),
        );
        let req = &snap.oriented.ci.summary.required;
        prompt.push_context_line(
            "CI",
            format!(
                "{} pass / {} failed / {} pending (required)",
                req.pass,
                req.fail(),
                req.pending()
            ),
        );
        let r = &snap.oriented.reviews;
        prompt.push_context_line(
            "Reviews",
            format!(
                "{} unresolved thread(s) / {} pending bot / {} pending human",
                r.threads_unresolved,
                r.pending_reviews.bots.len(),
                r.pending_reviews.humans.len()
            ),
        );
    }
}

/// Render the suite-level `MultiOutcome` as JSONL on stdout. One
/// record per PR (Bundle case); empty stdout for `UsageError` (parse
/// failures emit nothing on stdout — the `$? = 64` and stderr usage
/// block are sufficient).
///
/// The JSONL stream is the agent-harness contract:
///   * one record per line, in suite input order;
///   * each record carries `slug`, `pr`, `outcome` (variant name),
///     and `exit` (per-PR exit code);
///   * variant-specific fields are folded in:
///       - `action`, `blocker` for `Stuck*`, `Handoff*`, `WouldAdvance`
///       - `prompt` for `Handoff*`
///       - `automation` for `WouldAdvance`
///       - `msg` for `BinaryError` (and `UsageError`, though the
///         latter never occurs at the per-PR level).
fn render_multi_jsonl(out: &mut dyn std::io::Write, multi: &MultiOutcome) {
    let MultiOutcome::Bundle(prs) = multi else {
        return;
    };
    for po in prs {
        let _ = writeln!(out, "{}", per_pr_jsonl_record(po));
    }
}

fn per_pr_jsonl_record(po: &ProcessOutcome) -> String {
    use serde_json::{Map, Value, json};
    let mut obj: Map<String, Value> = Map::new();
    obj.insert("slug".into(), json!(po.slug.to_string()));
    obj.insert("pr".into(), json!(po.pr.get()));
    // Always include a deep link so harnesses don't have to
    // re-derive it from slug + pr per record.
    obj.insert("pr_url".into(), json!(pr_url(&po.slug, po.pr)));
    obj.insert("outcome".into(), json!(outcome_variant_name(&po.outcome)));
    obj.insert("exit".into(), json!(po.outcome.exit_code()));
    match &po.outcome {
        Outcome::StuckRepeated(a) | Outcome::StuckCapReached(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
        }
        Outcome::HandoffHuman(h) | Outcome::HandoffAgent(h) => {
            obj.insert("action".into(), json!(h.kind.name()));
            obj.insert("blocker".into(), json!(h.blocker.to_string()));
            obj.insert("prompt".into(), json!(h.prompt.to_string()));
        }
        Outcome::WouldAdvance(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
            obj.insert("effect".into(), json!(format_effect(&a.effect)));
        }
        Outcome::BinaryError(s) | Outcome::UsageError(s) => {
            obj.insert("msg".into(), json!(s));
        }
        Outcome::DoneSucceeded | Outcome::DoneAborted | Outcome::Paused => {
            // No additional fields.
        }
    }
    Value::Object(obj).to_string()
}

fn outcome_variant_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::DoneSucceeded => "DoneMerged",
        Outcome::StuckRepeated(_) => "StuckRepeated",
        Outcome::StuckCapReached(_) => "StuckCapReached",
        Outcome::HandoffHuman(_) => "HandoffHuman",
        Outcome::WouldAdvance(_) => "WouldAdvance",
        Outcome::HandoffAgent(_) => "HandoffAgent",
        Outcome::BinaryError(_) => "BinaryError",
        Outcome::Paused => "Paused",
        Outcome::DoneAborted => "DoneClosed",
        Outcome::UsageError(_) => "UsageError",
    }
}

/// Render `Outcome` to a writer (typically stderr) per the SKILL
/// contract: single-line header, optionally followed by a prompt
/// block for `Handoff*` variants. No trailing content.
fn render_outcome(out: &mut dyn std::io::Write, oc: &Outcome) {
    match oc {
        Outcome::DoneSucceeded => {
            let _ = writeln!(out, "DoneMerged");
        }
        Outcome::StuckRepeated(action) => {
            let _ = writeln!(
                out,
                "StuckRepeated: {}:{}",
                action.kind.name(),
                action.blocker
            );
        }
        Outcome::StuckCapReached(action) => {
            let _ = writeln!(
                out,
                "StuckCapReached: {}:{}",
                action.kind.name(),
                action.blocker
            );
        }
        Outcome::HandoffHuman(handoff) => {
            let _ = writeln!(out, "HandoffHuman: {}", handoff.kind.name());
            write_prompt_block(out, &handoff.prompt.to_string());
        }
        Outcome::WouldAdvance(action) => {
            let _ = writeln!(
                out,
                "WouldAdvance: {}:{}",
                action.kind.name(),
                format_effect(&action.effect)
            );
        }
        Outcome::HandoffAgent(handoff) => {
            let _ = writeln!(out, "HandoffAgent: {}", handoff.kind.name());
            write_prompt_block(out, &handoff.prompt.to_string());
        }
        Outcome::BinaryError(msg) => {
            let _ = writeln!(out, "BinaryError: {msg}");
        }
        Outcome::Paused => {
            let _ = writeln!(out, "Paused");
        }
        Outcome::DoneAborted => {
            let _ = writeln!(out, "DoneClosed");
        }
        Outcome::UsageError(msg) => {
            let _ = writeln!(out, "UsageError: {msg}");
            print_usage(out);
        }
    }
}

/// Write a prompt block: a single line beginning with the literal
/// 10-byte sequence `␣␣prompt:␣` (two spaces, "prompt", colon,
/// space) followed by the description content. Continuation lines
/// in the description carry no prefix; the block ends at the last
/// byte of content (no trailing newline beyond what the description
/// itself supplies — but `writeln!` adds one for clean line-ending).
fn write_prompt_block(out: &mut dyn std::io::Write, description: &str) {
    let _ = writeln!(out, "  prompt: {description}");
}

/// Format `ActionEffect` for the WouldAdvance stderr render.
/// `Wait{interval, ..}` becomes `Wait(<duration>)` with the duration
/// in the smallest sensible compound unit (s, m, m+s). The log/prompt
/// payload is intentionally omitted — that's what `write_prompt_block`
/// renders separately for handoff variants.
fn format_effect(e: &ActionEffect) -> String {
    match e {
        ActionEffect::Full { .. } => "Full".to_string(),
        ActionEffect::Agent { .. } => "Agent".to_string(),
        ActionEffect::Human { .. } => "Human".to_string(),
        ActionEffect::Wait { interval, .. } => {
            format!("Wait({})", format_duration(interval.as_duration()))
        }
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s}s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff(blocker: &str) -> ooda_core::HandoffAction<decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::tag(blocker),
        }
    }

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag(blocker),
        }
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(15)), "15s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "61m1s");
    }

    #[test]
    fn format_effect_variants() {
        assert_eq!(
            format_effect(&ActionEffect::Full { log: String::new() }),
            "Full"
        );
        assert_eq!(
            format_effect(&ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("p")
            }),
            "Agent"
        );
        assert_eq!(
            format_effect(&ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new("p")
            }),
            "Human"
        );
        assert_eq!(
            format_effect(&ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: String::new(),
            }),
            "Wait(30s)"
        );
    }

    #[test]
    fn iteration_line_execute_includes_blocker() {
        let decision = Decision::Execute(action("behind_base"));
        assert_eq!(
            iteration_line(4, &decision),
            "[iter 4] Rebase (Full) blocker: behind_base"
        );
    }

    #[test]
    fn iteration_line_handoff_includes_blocker() {
        let decision = Decision::Halt(decide::decision::DecisionHalt::HumanNeeded(handoff(
            "pending_human_review: review-team",
        )));
        assert_eq!(
            iteration_line(12, &decision),
            "[iter 12] halt: HumanNeeded blocker: pending_human_review: review-team"
        );
    }

    #[test]
    fn render_done_merged() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::DoneSucceeded);
        assert_eq!(String::from_utf8(buf).unwrap(), "DoneMerged\n");
    }

    #[test]
    fn render_paused() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::Paused);
        assert_eq!(String::from_utf8(buf).unwrap(), "Paused\n");
    }

    #[test]
    fn render_done_closed() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::DoneAborted);
        assert_eq!(String::from_utf8(buf).unwrap(), "DoneClosed\n");
    }

    #[test]
    fn render_stuck_cap_reached_carries_action() {
        let action = action("rebase-needed");
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::StuckCapReached(Box::new(action)));
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "StuckCapReached: Rebase:rebase-needed\n"
        );
    }

    #[test]
    fn render_binary_error() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::BinaryError("gh: 401".into()));
        assert_eq!(String::from_utf8(buf).unwrap(), "BinaryError: gh: 401\n");
    }

    #[test]
    fn render_handoff_agent_includes_prompt() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Rebase onto base"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("rebase-needed"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::HandoffAgent(Box::new(handoff)));
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HandoffAgent: Rebase\n"));
        assert!(s.contains("\n  prompt: Rebase onto base\n"));
    }

    #[test]
    fn render_would_advance_includes_automation() {
        let action = decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(30),
                log: "x".into(),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingWait,
            blocker: ids::BlockerKey::tag("waiting"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::WouldAdvance(Box::new(action)));
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "WouldAdvance: Rebase:Wait(30s)\n"
        );
    }

    // ─── per-PR JSONL records (suite stdout contract) ───────────────

    fn po(slug: &str, pr_num: u64, outcome: Outcome) -> ProcessOutcome {
        ProcessOutcome {
            slug: RepoSlug::parse(slug).unwrap(),
            pr: PullRequestNumber::new(pr_num).unwrap(),
            outcome,
        }
    }

    fn parse_record(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("JSONL record must parse as JSON")
    }

    // ─── per-PR JSONL schema goldens ────────────────────────────────
    //
    // Exhaustive snapshot tests for `per_pr_jsonl_record`. The
    // contract is the field set emitted for each `Outcome` variant —
    // downstream tooling (jq, shell pipelines, dashboards) reads
    // these field names directly, so a rename here is a breaking
    // change that MUST surface as a test failure.
    //
    // The match in `pr_jsonl_golden` is exhaustive over `Outcome`,
    // so adding a new variant fails to compile here until a golden
    // is added. The sample list (`pr_jsonl_sample_outcomes`) is
    // hand-maintained but the length sentinel in the test catches
    // omissions.

    /// Canonical JSON shape emitted by `per_pr_jsonl_record` for
    /// each `Outcome` variant. Used by `jsonl_schema_goldens_exhaustive`.
    fn pr_jsonl_golden(outcome: &Outcome) -> serde_json::Value {
        use serde_json::json;
        // Every record carries these four fields regardless of
        // variant. The merge below adds the variant-specific tail.
        let mut o = serde_json::Map::new();
        o.insert("slug".into(), json!("acme/widget"));
        o.insert("pr".into(), json!(42));
        o.insert(
            "pr_url".into(),
            json!("https://github.com/acme/widget/pull/42"),
        );
        match outcome {
            Outcome::DoneSucceeded => {
                o.insert("outcome".into(), json!("DoneMerged"));
                o.insert("exit".into(), json!(0));
            }
            Outcome::Paused => {
                o.insert("outcome".into(), json!("Paused"));
                o.insert("exit".into(), json!(1));
            }
            Outcome::WouldAdvance(a) => {
                o.insert("outcome".into(), json!("WouldAdvance"));
                o.insert("exit".into(), json!(2));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
                o.insert("effect".into(), json!(format_effect(&a.effect)));
            }
            Outcome::HandoffHuman(h) => {
                o.insert("outcome".into(), json!("HandoffHuman"));
                o.insert("exit".into(), json!(3));
                o.insert("action".into(), json!(h.kind.name()));
                o.insert("blocker".into(), json!(h.blocker.to_string()));
                o.insert("prompt".into(), json!(h.prompt.to_string()));
            }
            Outcome::HandoffAgent(h) => {
                o.insert("outcome".into(), json!("HandoffAgent"));
                o.insert("exit".into(), json!(4));
                o.insert("action".into(), json!(h.kind.name()));
                o.insert("blocker".into(), json!(h.blocker.to_string()));
                o.insert("prompt".into(), json!(h.prompt.to_string()));
            }
            Outcome::DoneAborted => {
                o.insert("outcome".into(), json!("DoneClosed"));
                o.insert("exit".into(), json!(5));
            }
            Outcome::StuckRepeated(a) => {
                o.insert("outcome".into(), json!("StuckRepeated"));
                o.insert("exit".into(), json!(6));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
            }
            Outcome::StuckCapReached(a) => {
                o.insert("outcome".into(), json!("StuckCapReached"));
                o.insert("exit".into(), json!(7));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
            }
            Outcome::UsageError(msg) => {
                o.insert("outcome".into(), json!("UsageError"));
                o.insert("exit".into(), json!(64));
                o.insert("msg".into(), json!(msg.as_str()));
            }
            Outcome::BinaryError(msg) => {
                o.insert("outcome".into(), json!("BinaryError"));
                o.insert("exit".into(), json!(70));
                o.insert("msg".into(), json!(msg.as_str()));
            }
        }
        serde_json::Value::Object(o)
    }

    /// One sample `Outcome` per variant. Hand-maintained; the length
    /// sentinel in `jsonl_schema_goldens_exhaustive` catches drift.
    /// Variants carrying an `Action` use distinct kinds / blockers /
    /// payloads so the golden distinguishes them by shape.
    fn pr_jsonl_sample_outcomes() -> Vec<Outcome> {
        let stuck_action = action("rebase-needed");
        let mut would_advance_action = action("ci_pending: build");
        would_advance_action.effect = ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(60),
            log: "Wait for 2 pending checks".into(),
        };
        // Handoff variants now carry `HandoffAction` (the typed
        // projection with a top-level `prompt` field); construct
        // those directly instead of going via an `Action` and
        // mutating `effect`.
        let handoff_agent_action = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Address 2 unresolved review threads."),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("unresolved_threads"),
        };
        let handoff_human_action = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Approve the PR."),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::tag("pending_human_review: alice"),
        };
        vec![
            Outcome::DoneSucceeded,
            Outcome::Paused,
            Outcome::WouldAdvance(Box::new(would_advance_action)),
            Outcome::HandoffHuman(Box::new(handoff_human_action)),
            Outcome::HandoffAgent(Box::new(handoff_agent_action)),
            Outcome::DoneAborted,
            Outcome::StuckRepeated(Box::new(stuck_action.clone())),
            Outcome::StuckCapReached(Box::new(stuck_action)),
            Outcome::UsageError("bad --concurrency".into()),
            Outcome::BinaryError("observe: gh: connection refused".into()),
        ]
    }

    /// One golden assertion per `Outcome` variant. Compile-checked
    /// exhaustiveness lives in `pr_jsonl_golden`; runtime
    /// completeness for the sample list is enforced by the length
    /// sentinel — every Outcome variant in the family of 10 must
    /// be represented.
    #[test]
    fn jsonl_schema_goldens_exhaustive() {
        let samples = pr_jsonl_sample_outcomes();
        assert_eq!(
            samples.len(),
            10,
            "`pr_jsonl_sample_outcomes` must include one sample per `Outcome` variant; \
             adding a new variant requires adding both a golden arm in `pr_jsonl_golden` \
             AND a sample here.",
        );
        for outcome in samples {
            let outcome_name = outcome_variant_name(&outcome);
            let po = po("acme/widget", 42, outcome);
            let actual = parse_record(&per_pr_jsonl_record(&po));
            let expected = pr_jsonl_golden(&po.outcome);
            assert_eq!(
                actual, expected,
                "schema mismatch for variant {outcome_name}"
            );
        }
    }

    #[test]
    fn decorate_handoff_human_appends_pr_link_and_blocker() {
        use crate::decide::action::{ActionKind, TargetEffect, Urgency};
        use crate::ids::BlockerKey;
        let h = ooda_core::HandoffAction {
            kind: ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag("not_approved"),
        };
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated = decorate_handoff_human(Outcome::HandoffHuman(Box::new(h)), &slug, pr, None);
        let Outcome::HandoffHuman(h) = decorated else {
            panic!("expected HandoffHuman");
        };
        let rendered = h.prompt.to_string();
        assert!(
            rendered.contains("PR: https://github.com/acme/widget/pull/42"),
            "decoration: {rendered}",
        );
        assert!(rendered.contains("Blocker: not_approved"));
        assert!(rendered.starts_with("Request or self-approve"));
    }

    #[test]
    fn decorate_handoff_human_passes_through_other_variants() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("1").unwrap();
        assert!(matches!(
            decorate_handoff_human(Outcome::DoneSucceeded, &slug, pr, None),
            Outcome::DoneSucceeded
        ));
        assert!(matches!(
            decorate_handoff_human(Outcome::Paused, &slug, pr, None),
            Outcome::Paused
        ));
    }

    #[test]
    fn decorate_handoff_agent_rebase_gets_pr_context() {
        // Rebase emits `HandoffAgent`, not `HandoffHuman`. The
        // decorator was originally HandoffHuman-only, which left
        // Rebase prompts with zero PR/URL/blocker frame. This test
        // pins the widened decorator to Rebase so a future change
        // that drops the HandoffAgent arm regresses loudly.
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("behind_base"),
        };
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated =
            decorate_handoff_human(Outcome::HandoffAgent(Box::new(handoff)), &slug, pr, None);
        let Outcome::HandoffAgent(handoff) = decorated else {
            panic!("expected HandoffAgent");
        };
        let rendered = handoff.prompt.to_string();
        assert!(
            rendered.contains("PR: https://github.com/acme/widget/pull/42"),
            "PR context missing from Rebase HandoffAgent: {rendered}",
        );
        assert!(
            rendered.contains("Blocker: behind_base"),
            "blocker context missing: {rendered}",
        );
        // Original prompt content preserved.
        assert!(rendered.starts_with("Rebase onto the latest base branch"));
    }

    // ── Phase B: dashboard preamble injection ─────────────────────

    fn snapshot_with_dashboard(candidates: Vec<decide::action::Action>) -> HandoffSnapshot {
        use dashboard::{Dashboard, RankedCandidate};
        // Build a Dashboard directly from a candidate list — the
        // boundary-time helper `Dashboard::from_iteration` requires
        // a full `OrientedState` we don't need here. The decorator
        // only consumes `render_handoff_preamble`, which walks the
        // public fields.
        let dashboard_candidates: Vec<RankedCandidate> = candidates
            .iter()
            .map(|a| RankedCandidate {
                action_name: ooda_core::ActionKindName::name(&a.kind),
                action_log: a.rendered_payload(),
                effect_debug: format!("{:?}", a.effect),
                urgency: a.urgency,
                blocker: a.blocker.clone(),
            })
            .collect();
        let blockers: Vec<dashboard::Blocker> = dashboard_candidates
            .iter()
            .map(|c| dashboard::Blocker {
                tag: c.blocker.clone(),
                action_name: c.action_name,
            })
            .collect();
        let dashboard = Dashboard {
            candidates: dashboard_candidates,
            signals: Vec::new(),
            blockers,
        };
        HandoffSnapshot {
            oriented: stub_oriented(),
            head_short: "abcdef0".into(),
            base_branch: "master".into(),
            dashboard,
        }
    }

    fn stub_oriented() -> orient::OrientedState {
        // The preamble decorator only reads `snapshot.dashboard`,
        // but `push_handoff_context` reads ci.summary + reviews.
        // A defaulted OrientedState satisfies both — the assertions
        // in the decorator tests below pin on PR-context lines
        // (which use the slug/pr passed in, not snapshot fields).
        use crate::ids::Timestamp;
        use crate::observe::github::pr_view::{MergeStateStatus, Mergeable};
        use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
        use crate::orient::reviews::{PendingReviews, ReviewSummary};
        use crate::orient::state::PullRequestState;
        orient::OrientedState {
            ci: CiReport {
                summary: CiSummary {
                    required: CheckBucket::default(),
                    missing_names: vec![],
                    completed_at: None,
                    advisory: CheckBucket::default(),
                },
                activity: CiActivity::Resolved(ResolvedState::AllGreen),
            },
            state: PullRequestState {
                conflict: Mergeable::Mergeable,
                draft: false,
                wip: false,
                title_len: 30,
                title_ok: true,
                body: true,
                summary: true,
                test_plan: true,
                content_label: true,
                assignees: 1,
                reviewers: 1,
                merge_when_ready: false,
                commits: 1,
                behind: false,
                has_open_parent_pr: false,
                merge_state_status: MergeStateStatus::Clean,
                updated_at: Timestamp::parse("2024-01-01T00:00:00Z").unwrap(),
                last_commit_at: None,
            },
            reviews: ReviewSummary {
                decision: None,
                threads_unresolved: 0,
                threads_total: 0,
                bot_comments: 0,
                approvals_on_head: 0,
                approvals_stale: 0,
                pending_reviews: PendingReviews::default(),
                bot_reviews: vec![],
                requested_reviewers: orient::reviews::RequestedReviewerSet::default(),
                latest_human_changes_requested: None,
            },
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
        }
    }

    fn rebase_action() -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("behind_base"),
        }
    }

    #[test]
    fn decorate_handoff_human_prepends_dashboard_preamble() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::tag("not_approved"),
        };
        let snap = snapshot_with_dashboard(vec![rebase_action()]);
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated = decorate_handoff_human(
            Outcome::HandoffHuman(Box::new(handoff)),
            &slug,
            pr,
            Some(&snap),
        );
        let Outcome::HandoffHuman(handoff) = decorated else {
            panic!("expected HandoffHuman");
        };
        let rendered = handoff.prompt.to_string();
        // Preamble appears before the trailing context block. The
        // headline (per-action body's first line) still leads —
        // sections render between headline and context.
        assert!(
            rendered.contains("Recommended (blocking fix): Rebase:"),
            "preamble: {rendered}",
        );
        assert!(rendered.contains("[blocker: behind_base]"), "{rendered}");
        assert!(rendered.contains("Blockers"), "{rendered}");
        // The existing per-action context block from 5bf9c7c still
        // lands at the trailing edge — preamble does not displace it.
        assert!(
            rendered.contains("PR: https://github.com/acme/widget/pull/42"),
            "trailing context: {rendered}",
        );
        assert!(rendered.contains("Blocker: not_approved"), "{rendered}");
    }

    #[test]
    fn decorate_handoff_agent_rebase_gets_preamble_plus_existing_body() {
        // The Phase-B preamble is universal; the 5bf9c7c per-action
        // context block stays gated on the allowlist (Rebase opts
        // in). Both layers coexist — preamble on top, context block
        // on the bottom, original prompt headline in between.
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("behind_base"),
        };
        let snap = snapshot_with_dashboard(vec![rebase_action()]);
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated = decorate_handoff_human(
            Outcome::HandoffAgent(Box::new(handoff)),
            &slug,
            pr,
            Some(&snap),
        );
        let Outcome::HandoffAgent(handoff) = decorated else {
            panic!("expected HandoffAgent");
        };
        let rendered = handoff.prompt.to_string();
        assert!(
            rendered.contains("Recommended (blocking fix): Rebase:"),
            "preamble missing: {rendered}",
        );
        assert!(
            rendered.contains("PR: https://github.com/acme/widget/pull/42"),
            "5bf9c7c context missing: {rendered}",
        );
        assert!(
            rendered.contains("Blocker: behind_base"),
            "5bf9c7c blocker missing: {rendered}",
        );
        assert!(rendered.contains("Rebase onto the latest base branch"));
    }

    #[test]
    fn decorate_handoff_agent_non_allowlisted_still_gets_preamble() {
        // The preamble is universal — a HandoffAgent variant outside
        // the per-action context allowlist still picks it up. This
        // pins the "preamble is not gated by `agent_action_needs_context`"
        // contract.
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::AddressChangeRequest,
            prompt: ooda_core::HandoffPrompt::new("Address change request"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("changes_requested_summary"),
        };
        let snap = snapshot_with_dashboard(vec![rebase_action()]);
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("1").unwrap();
        let decorated = decorate_handoff_human(
            Outcome::HandoffAgent(Box::new(handoff)),
            &slug,
            pr,
            Some(&snap),
        );
        let Outcome::HandoffAgent(handoff) = decorated else {
            panic!("expected HandoffAgent");
        };
        let rendered = handoff.prompt.to_string();
        assert!(
            rendered.contains("Recommended (blocking fix): Rebase:"),
            "preamble must apply to non-allowlisted: {rendered}",
        );
        // Per-action context still gated — no PR / Blocker lines.
        assert!(!rendered.contains("PR: https://"), "{rendered}");
        assert!(!rendered.contains("Blocker: behind_base"), "{rendered}");
    }

    #[test]
    fn decorate_handoff_agent_non_rebase_passes_through_undecorated() {
        // The widened decorator is allowlisted, not blanket. Other
        // HandoffAgent variants (e.g. AddressThreads, FixCi) keep
        // their original payload — the gate prevents context creep
        // into prompts that already carry their own structure.
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::AddressChangeRequest,
            prompt: ooda_core::HandoffPrompt::new("Address change request"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("changes_requested_summary"),
        };
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("1").unwrap();
        let decorated =
            decorate_handoff_human(Outcome::HandoffAgent(Box::new(handoff)), &slug, pr, None);
        let Outcome::HandoffAgent(handoff) = decorated else {
            panic!("expected HandoffAgent");
        };
        let rendered = handoff.prompt.to_string();
        assert!(!rendered.contains("PR: https://"));
        assert!(!rendered.contains("Blocker:"));
    }

    // Note: per-variant shape assertions for `per_pr_jsonl_record`
    // are covered by `jsonl_schema_goldens_exhaustive` above. Adding
    // new per-variant tests here would be redundant — the exhaustive
    // golden's match arms are the per-variant contract.

    #[test]
    fn render_multi_jsonl_emits_one_line_per_pr_in_order() {
        let multi = MultiOutcome::Bundle(vec![
            po("a/b", 1, Outcome::DoneSucceeded),
            po(
                "a/b",
                2,
                Outcome::HandoffAgent(Box::new(handoff("unresolved"))),
            ),
            po("c/d", 9, Outcome::Paused),
        ]);
        let mut buf = Vec::new();
        render_multi_jsonl(&mut buf, &multi);
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        let first = parse_record(lines[0]);
        let second = parse_record(lines[1]);
        let third = parse_record(lines[2]);
        assert_eq!(
            (first["slug"].as_str(), first["pr"].as_u64()),
            (Some("a/b"), Some(1))
        );
        assert_eq!(
            (second["slug"].as_str(), second["pr"].as_u64()),
            (Some("a/b"), Some(2))
        );
        assert_eq!(
            (third["slug"].as_str(), third["pr"].as_u64()),
            (Some("c/d"), Some(9))
        );
        assert_eq!(second["outcome"], "HandoffAgent");
    }

    #[test]
    fn render_multi_jsonl_usage_error_emits_nothing() {
        let multi = MultiOutcome::UsageError("bad invocation".into());
        let mut buf = Vec::new();
        render_multi_jsonl(&mut buf, &multi);
        assert!(buf.is_empty(), "UsageError must not write to stdout");
    }
}
