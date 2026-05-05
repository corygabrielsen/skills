#![allow(dead_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod act;
mod comment;
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

use decide::action::Automation;
use decide::decision::{Decision, DecisionHalt};
use decide::{candidates, decide_from_candidates};
use ids::{PullRequestNumber, RepoSlug};
use multi_outcome::{MultiOutcome, ProcessOutcome};
use observe::github::fetch_all;
use orient::orient;
use outcome::Outcome;
use recorder::{Recorder, RecorderConfig, RunMode};
use runner::{LoopConfig, run_loop};
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
         Exit codes — aggregate priority projection over per-PR Outcomes:\n  0 all DoneMerged (or mixed terminal: Paused/DoneClosed/DoneMerged)\n  1 any StuckRepeated      2 any StuckCapReached    3 any HandoffHuman\n  4 any WouldAdvance       5 any HandoffAgent       6 any BinaryError\n  7 (unused at suite level — Paused folded into 0)\n  8 (unused at suite level — DoneClosed folded into 0)\n  64 UsageError\n\
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
    max_iter: u32,
    status_comment: bool,
    state_root: Option<PathBuf>,
    trace: Option<PathBuf>,
    /// Optional cap on concurrent in-flight PRs. `None` means no cap
    /// (= |suite|). Wired into the suite spawn loop in a later stage;
    /// currently parsed but not yet enforced.
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
fn parse_args() -> Result<Args, Outcome> {
    // Pre-scan: --help wins over any other parse failure.
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print_usage(&mut std::io::stdout());
        std::process::exit(0);
    }

    let mut mode = Mode::Loop;
    let mut max_iter: u32 = 50;
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
                // (parse failure of remaining cases), and zero (validated
                // after parse).
                if v.starts_with('-') {
                    return Err(usage(&format!(
                        "--max-iter must be ≥ 1; got negative value: {v}"
                    )));
                }
                let Ok(n) = v.parse::<u32>() else {
                    return Err(usage(&format!("--max-iter: not an integer: {v}")));
                };
                if n == 0 {
                    return Err(usage("--max-iter must be ≥ 1; got 0"));
                }
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
fn parse_suite(positional: &[String]) -> Result<Vec<(RepoSlug, PullRequestNumber)>, Outcome> {
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

fn usage(msg: &str) -> Outcome {
    // Newline-strip per SKILL.md UsageError invariant.
    let flat = if msg.contains('\n') {
        msg.replace('\n', " ")
    } else {
        msg.to_string()
    };
    Outcome::UsageError(flat)
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
        Err(usage_outcome) => {
            // Render the UsageError variant block to stderr (header
            // + usage text) before exiting. The parser path's typed
            // `Outcome::UsageError` carries the diagnostic; we lift
            // it to the suite boundary as `MultiOutcome::UsageError`
            // for exit-code symmetry, but keep the existing
            // `render_outcome` formatting on stderr.
            render_outcome(&mut std::io::stderr(), &usage_outcome);
            let msg = match &usage_outcome {
                Outcome::UsageError(s) => s.clone(),
                _ => unreachable!(
                    "parse_args returns Outcome::UsageError on the Err path; got {usage_outcome:?}"
                ),
            };
            MultiOutcome::UsageError(msg)
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
            let msg = flatten(format!("recorder: {e}"));
            // No recorder was opened for this PR; render to stderr
            // so the suite-level summary still observes the failure.
            let outcome = Outcome::BinaryError(msg);
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
        Ok(o) => {
            recorder.record_observe_end(1, Ok(()));
            o
        }
        Err(e) => {
            recorder.record_observe_end(1, Err(e.to_string()));
            return Outcome::BinaryError(flatten(format!("observe: {e}")));
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
    let oriented = orient(&obs, None);
    let candidate_actions = candidates(&oriented);
    let decision = decide_from_candidates(candidate_actions.clone(), obs.pr_view.state);
    recorder.record_iteration(1, &obs, &oriented, &candidate_actions, &decision);
    if args.status_comment {
        let rendered = comment::render::render(&oriented, &decision);
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(1));
        log_post_result("comment", true, r, Some(recorder));
    }
    Outcome::from(decision)
}

fn run_full(slug: &RepoSlug, pr: PullRequestNumber, args: &Args, recorder: &Recorder) -> Outcome {
    let cfg = LoopConfig {
        max_iterations: args.max_iter,
    };
    let on_state = |i: u32,
                    obs: &observe::github::GitHubObservations,
                    oriented: &orient::OrientedState,
                    candidate_actions: &[decide::action::Action],
                    d: &Decision| {
        recorder.set_iteration(Some(i));
        recorder.record_iteration(i, obs, oriented, candidate_actions, d);
        let line = iteration_line(i, d);
        eprintln!("{line}");
        recorder.write_trace_line(&line);
        if args.status_comment {
            let rendered = comment::render::render(oriented, d);
            recorder.record_status_comment_rendered(
                Some(i),
                &rendered,
                format!("[iter {i}] comment rendered"),
            );
            let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(i));
            log_post_result(&format!("[iter {i}] comment"), false, r, Some(recorder));
        }
    };
    match run_loop(slug, pr, cfg, recorder, on_state) {
        Ok(reason) => Outcome::from(reason),
        Err(e) => Outcome::from(e),
    }
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
        Err(e) => Some(format!("{prefix}: {}", flatten(e.to_string()))),
    }
}

fn iteration_line(i: u32, d: &Decision) -> String {
    match d {
        Decision::Execute(action) => {
            format!(
                "[iter {i}] {} ({}) blocker: {}",
                action.kind.name(),
                format_automation(&action.automation),
                action.blocker,
            )
        }
        Decision::Halt(halt) => {
            // Use halt.name() (finite token set) instead of {:?}
            // so the per-iteration halt line stays single-line
            // and bounded — Debug would expand AgentNeeded(Action {
            // description: "..." }) into the action payload, which
            // breaks the one-line-per-iteration invariant.
            match halt_action(halt) {
                Some(action) => format!(
                    "[iter {i}] halt: {} blocker: {}",
                    DecisionHalt::name(halt),
                    action.blocker,
                ),
                None => format!("[iter {i}] halt: {}", DecisionHalt::name(halt)),
            }
        }
    }
}

fn halt_action(halt: &DecisionHalt) -> Option<&decide::action::Action> {
    match halt {
        DecisionHalt::AgentNeeded(action) | DecisionHalt::HumanNeeded(action) => Some(action),
        DecisionHalt::Success | DecisionHalt::Terminal(_) => None,
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
    obj.insert("outcome".into(), json!(outcome_variant_name(&po.outcome)));
    obj.insert("exit".into(), json!(po.outcome.exit_code()));
    match &po.outcome {
        Outcome::StuckRepeated(a) | Outcome::StuckCapReached(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
        }
        Outcome::HandoffHuman(a) | Outcome::HandoffAgent(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
            obj.insert("prompt".into(), json!(a.description));
        }
        Outcome::WouldAdvance(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
            obj.insert("automation".into(), json!(format_automation(&a.automation)));
        }
        Outcome::BinaryError(s) | Outcome::UsageError(s) => {
            obj.insert("msg".into(), json!(s));
        }
        Outcome::DoneMerged | Outcome::DoneClosed | Outcome::Paused => {
            // No additional fields.
        }
    }
    Value::Object(obj).to_string()
}

fn outcome_variant_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::DoneMerged => "DoneMerged",
        Outcome::StuckRepeated(_) => "StuckRepeated",
        Outcome::StuckCapReached(_) => "StuckCapReached",
        Outcome::HandoffHuman(_) => "HandoffHuman",
        Outcome::WouldAdvance(_) => "WouldAdvance",
        Outcome::HandoffAgent(_) => "HandoffAgent",
        Outcome::BinaryError(_) => "BinaryError",
        Outcome::Paused => "Paused",
        Outcome::DoneClosed => "DoneClosed",
        Outcome::UsageError(_) => "UsageError",
    }
}

/// Render `Outcome` to a writer (typically stderr) per the SKILL
/// contract: single-line header, optionally followed by a prompt
/// block for `Handoff*` variants. No trailing content.
fn render_outcome(out: &mut dyn std::io::Write, oc: &Outcome) {
    match oc {
        Outcome::DoneMerged => {
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
        Outcome::HandoffHuman(action) => {
            let _ = writeln!(out, "HandoffHuman: {}", action.kind.name());
            write_prompt_block(out, &action.description);
        }
        Outcome::WouldAdvance(action) => {
            let _ = writeln!(
                out,
                "WouldAdvance: {}:{}",
                action.kind.name(),
                format_automation(&action.automation)
            );
        }
        Outcome::HandoffAgent(action) => {
            let _ = writeln!(out, "HandoffAgent: {}", action.kind.name());
            write_prompt_block(out, &action.description);
        }
        Outcome::BinaryError(msg) => {
            let _ = writeln!(out, "BinaryError: {msg}");
        }
        Outcome::Paused => {
            let _ = writeln!(out, "Paused");
        }
        Outcome::DoneClosed => {
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

/// Format `Automation` for the WouldAdvance stderr render.
/// `Wait{interval}` becomes `Wait(<duration>)` with the duration in
/// the smallest sensible compound unit (s, m, m+s).
fn format_automation(a: &Automation) -> String {
    match a {
        Automation::Full => "Full".to_string(),
        Automation::Agent => "Agent".to_string(),
        Automation::Human => "Human".to_string(),
        Automation::Wait { interval } => format!("Wait({})", format_duration(*interval)),
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

fn flatten(s: String) -> String {
    if s.contains('\n') {
        s.replace('\n', " ")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            automation: Automation::Full,
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            description: "x".into(),
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
    fn format_automation_variants() {
        assert_eq!(format_automation(&Automation::Full), "Full");
        assert_eq!(format_automation(&Automation::Agent), "Agent");
        assert_eq!(format_automation(&Automation::Human), "Human");
        assert_eq!(
            format_automation(&Automation::Wait {
                interval: Duration::from_secs(30)
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
        let decision = Decision::Halt(decide::decision::DecisionHalt::HumanNeeded(action(
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
        render_outcome(&mut buf, &Outcome::DoneMerged);
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
        render_outcome(&mut buf, &Outcome::DoneClosed);
        assert_eq!(String::from_utf8(buf).unwrap(), "DoneClosed\n");
    }

    #[test]
    fn render_stuck_cap_reached_carries_action() {
        let action = action("rebase-needed");
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::StuckCapReached(action));
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
        let action = decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            automation: Automation::Agent,
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            description: "Rebase onto base".into(),
            blocker: ids::BlockerKey::tag("rebase-needed"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::HandoffAgent(action));
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HandoffAgent: Rebase\n"));
        assert!(s.contains("\n  prompt: Rebase onto base\n"));
    }

    #[test]
    fn render_would_advance_includes_automation() {
        let action = decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            automation: Automation::Wait {
                interval: Duration::from_secs(30),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingWait,
            description: "x".into(),
            blocker: ids::BlockerKey::tag("waiting"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::WouldAdvance(action));
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

    #[test]
    fn jsonl_done_merged_minimal_shape() {
        let r = per_pr_jsonl_record(&po("a/b", 42, Outcome::DoneMerged));
        let v = parse_record(&r);
        assert_eq!(v["slug"], "a/b");
        assert_eq!(v["pr"], 42);
        assert_eq!(v["outcome"], "DoneMerged");
        assert_eq!(v["exit"], 0);
        assert!(v.get("action").is_none());
        assert!(v.get("prompt").is_none());
    }

    #[test]
    fn jsonl_done_closed_minimal_shape() {
        let r = per_pr_jsonl_record(&po("a/b", 1, Outcome::DoneClosed));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "DoneClosed");
        assert_eq!(v["exit"], 8);
    }

    #[test]
    fn jsonl_paused_minimal_shape() {
        let r = per_pr_jsonl_record(&po("a/b", 1, Outcome::Paused));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "Paused");
        assert_eq!(v["exit"], 7);
    }

    #[test]
    fn jsonl_stuck_repeated_includes_action_and_blocker() {
        let r = per_pr_jsonl_record(&po(
            "a/b",
            7,
            Outcome::StuckRepeated(action("rebase-needed")),
        ));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "StuckRepeated");
        assert_eq!(v["exit"], 1);
        assert_eq!(v["action"], "Rebase");
        assert_eq!(v["blocker"], "rebase-needed");
        assert!(v.get("prompt").is_none());
    }

    #[test]
    fn jsonl_stuck_cap_reached_includes_action_and_blocker() {
        let r = per_pr_jsonl_record(&po(
            "a/b",
            7,
            Outcome::StuckCapReached(action("rebase-needed")),
        ));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "StuckCapReached");
        assert_eq!(v["exit"], 2);
        assert_eq!(v["action"], "Rebase");
        assert_eq!(v["blocker"], "rebase-needed");
    }

    #[test]
    fn jsonl_handoff_agent_includes_prompt() {
        let mut a = action("unresolved_threads");
        a.description = "Address 2 unresolved review threads.".into();
        let r = per_pr_jsonl_record(&po("a/b", 7, Outcome::HandoffAgent(a)));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "HandoffAgent");
        assert_eq!(v["exit"], 5);
        assert_eq!(v["action"], "Rebase");
        assert_eq!(v["blocker"], "unresolved_threads");
        assert_eq!(v["prompt"], "Address 2 unresolved review threads.");
    }

    #[test]
    fn jsonl_handoff_human_includes_prompt() {
        let mut a = action("pending_human_review: alice");
        a.description = "Approve the PR.".into();
        let r = per_pr_jsonl_record(&po("a/b", 7, Outcome::HandoffHuman(a)));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "HandoffHuman");
        assert_eq!(v["exit"], 3);
        assert_eq!(v["prompt"], "Approve the PR.");
    }

    #[test]
    fn jsonl_would_advance_includes_automation_string() {
        let mut a = action("ci_pending: build");
        a.automation = Automation::Wait {
            interval: Duration::from_secs(60),
        };
        let r = per_pr_jsonl_record(&po("a/b", 7, Outcome::WouldAdvance(a)));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "WouldAdvance");
        assert_eq!(v["exit"], 4);
        assert_eq!(v["automation"], "Wait(1m)");
    }

    #[test]
    fn jsonl_binary_error_includes_msg() {
        let r = per_pr_jsonl_record(&po(
            "a/b",
            7,
            Outcome::BinaryError("observe: gh: connection refused".into()),
        ));
        let v = parse_record(&r);
        assert_eq!(v["outcome"], "BinaryError");
        assert_eq!(v["exit"], 6);
        assert_eq!(v["msg"], "observe: gh: connection refused");
        assert!(v.get("action").is_none());
    }

    #[test]
    fn render_multi_jsonl_emits_one_line_per_pr_in_order() {
        let multi = MultiOutcome::Bundle(vec![
            po("a/b", 1, Outcome::DoneMerged),
            po("a/b", 2, Outcome::HandoffAgent(action("unresolved"))),
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
