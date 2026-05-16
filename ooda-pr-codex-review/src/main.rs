use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod act;
mod comment;
mod dashboard;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod text;

use act::{ActContext, CodexActContext};
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::candidates;
use decide::decision::{Decision, DecisionHalt};
use ids::{CodexReasoningLevel, PullRequestNumber, RepoSlug};
use observe::codex::fetch_all as fetch_codex;
use observe::github::{FetchOutcome, fetch_all};
use ooda_core::decide_from_candidates;
use orient::orient;
use outcome::Outcome;
use recorder::{Recorder, RecorderConfig, RunMode};
use runner::{CodexReviewConfig, LoopConfig, current_timestamp, run_loop};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-pr-codex-review — drive a PR through observe → orient → decide → act until halt, optionally with local codex review.\n\
         \n\
         Usage:\n  ooda-pr-codex-review [options] <owner/repo> <pr>           run the loop (default)\n  ooda-pr-codex-review inspect [options] <owner/repo> <pr>   one pass; print Outcome; exit\n\
         \n\
         Options:\n  --max-iter N                  loop iteration cap (default 50, must be ≥ 1; ignored by inspect)\n  --status-comment              post a status comment on the PR each iteration (deduped)\n  --state-root PATH             write always-on harness state under PATH\n  --trace PATH                  also append the compact trace to PATH\n  --codex-review-ceiling LVL    enable codex review with ceiling LVL: off|low|medium|high|xhigh (default off — codex review disabled)\n  --codex-review-floor LVL      codex review starting rung: low|medium|high|xhigh (default low; must be ≤ ceiling)\n  --codex-review-n N            codex review parallel reviewers per level (default 3, must be ≥ 1)\n  --codex-review-bin PATH       path to the codex binary (default codex, PATH lookup)\n  -h, --help                    show this help and exit\n\
         \n\
         Exit codes (stderr header — see SKILL.md for variant mapping):\n   0 DoneMerged       1 Paused             2 WouldAdvance      3 HandoffHuman\n   4 HandoffAgent     5 DoneClosed         6 StuckRepeated     7 StuckCapReached\n  64 UsageError      70 BinaryError       (130 SIGINT, 143 SIGTERM reserved)"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loop,
    Inspect,
}

struct Args {
    mode: Mode,
    slug: RepoSlug,
    pr: PullRequestNumber,
    max_iter: std::num::NonZeroU32,
    status_comment: bool,
    state_root: Option<PathBuf>,
    trace: Option<PathBuf>,
    /// Codex review ceiling. `None` means the axis is disabled
    /// entirely (ooda-pr-equivalent behavior); `Some(level)` enables
    /// the axis with that level as its upper bound.
    codex_review_ceiling: Option<CodexReasoningLevel>,
    /// Codex review floor — the starting rung of the ladder. Must be
    /// ≤ ceiling when ceiling is set. Default `Low`.
    codex_review_floor: CodexReasoningLevel,
    /// Number of parallel `codex review` subprocesses per batch.
    /// Default 3, must be ≥ 1.
    codex_review_n: u32,
    /// Path to the `codex` binary. Default `codex` (PATH lookup).
    codex_review_bin: PathBuf,
}

fn parse_ceiling(s: &str) -> Result<Option<CodexReasoningLevel>, String> {
    match s {
        "off" => Ok(None),
        "low" => Ok(Some(CodexReasoningLevel::Low)),
        "medium" => Ok(Some(CodexReasoningLevel::Medium)),
        "high" => Ok(Some(CodexReasoningLevel::High)),
        "xhigh" => Ok(Some(CodexReasoningLevel::Xhigh)),
        _ => Err(format!(
            "--codex-review-ceiling: unknown value `{s}` (expected: off|low|medium|high|xhigh)"
        )),
    }
}

fn parse_level(s: &str, flag: &str) -> Result<CodexReasoningLevel, String> {
    match s {
        "low" => Ok(CodexReasoningLevel::Low),
        "medium" => Ok(CodexReasoningLevel::Medium),
        "high" => Ok(CodexReasoningLevel::High),
        "xhigh" => Ok(CodexReasoningLevel::Xhigh),
        _ => Err(format!(
            "{flag}: unknown value `{s}` (expected: low|medium|high|xhigh)"
        )),
    }
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
    let mut max_iter: std::num::NonZeroU32 = std::num::NonZeroU32::new(50).expect("50 is non-zero");
    let mut status_comment = false;
    let mut state_root: Option<PathBuf> = None;
    let mut trace: Option<PathBuf> = None;
    let mut codex_review_ceiling: Option<CodexReasoningLevel> = None;
    let mut codex_review_floor: CodexReasoningLevel = CodexReasoningLevel::Low;
    let mut codex_review_n: u32 = 3;
    let mut codex_review_bin: PathBuf = PathBuf::from("codex");
    let mut positional: Vec<String> = Vec::new();
    let mut saw_subcommand = false;
    let mut saw_max_iter = false;
    let mut saw_status_comment = false;
    let mut saw_state_root = false;
    let mut saw_trace = false;
    let mut saw_codex_review_ceiling = false;
    let mut saw_codex_review_floor = false;
    let mut saw_codex_review_n = false;
    let mut saw_codex_review_bin = false;

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
                // (parse failure), and zero (parsed but invalid). The
                // validated value flows out as `NonZeroU32` so the
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
            "--codex-review-ceiling" => {
                if saw_codex_review_ceiling {
                    return Err(usage("--codex-review-ceiling repeated"));
                }
                saw_codex_review_ceiling = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--codex-review-ceiling requires a value"));
                };
                codex_review_ceiling = parse_ceiling(&v).map_err(|e| usage(&e))?;
            }
            "--codex-review-floor" => {
                if saw_codex_review_floor {
                    return Err(usage("--codex-review-floor repeated"));
                }
                saw_codex_review_floor = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--codex-review-floor requires a value"));
                };
                codex_review_floor =
                    parse_level(&v, "--codex-review-floor").map_err(|e| usage(&e))?;
            }
            "--codex-review-n" => {
                if saw_codex_review_n {
                    return Err(usage("--codex-review-n repeated"));
                }
                saw_codex_review_n = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--codex-review-n requires a value"));
                };
                if v.starts_with('-') {
                    return Err(usage(&format!(
                        "--codex-review-n must be ≥ 1; got negative value: {v}"
                    )));
                }
                let Ok(n) = v.parse::<u32>() else {
                    return Err(usage(&format!("--codex-review-n: not an integer: {v}")));
                };
                if n == 0 {
                    return Err(usage("--codex-review-n must be ≥ 1; got 0"));
                }
                codex_review_n = n;
            }
            "--codex-review-bin" => {
                if saw_codex_review_bin {
                    return Err(usage("--codex-review-bin repeated"));
                }
                saw_codex_review_bin = true;
                let Some(v) = iter.next() else {
                    return Err(usage("--codex-review-bin requires a value"));
                };
                codex_review_bin = PathBuf::from(v);
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

    if positional.len() != 2 {
        return Err(usage(&format!(
            "expected exactly 2 positionals (owner/repo, pr); got {}",
            positional.len()
        )));
    }
    let slug = RepoSlug::parse(&positional[0]).map_err(|e| usage(&e.to_string()))?;
    let pr = PullRequestNumber::parse(&positional[1]).map_err(|e| usage(&e.to_string()))?;

    if let Some(ceiling) = codex_review_ceiling
        && codex_review_floor > ceiling
    {
        return Err(usage(&format!(
            "--codex-review-floor ({}) must be ≤ --codex-review-ceiling ({})",
            codex_review_floor.as_str(),
            ceiling.as_str()
        )));
    }

    Ok(Args {
        mode,
        slug,
        pr,
        max_iter,
        status_comment,
        state_root,
        trace,
        codex_review_ceiling,
        codex_review_floor,
        codex_review_n,
        codex_review_bin,
    })
}

fn usage(msg: &str) -> Outcome {
    Outcome::usage_error(msg)
}

fn main() -> ExitCode {
    let outcome = match parse_args() {
        Ok(args) => {
            let recorder = match Recorder::open(RecorderConfig {
                slug: args.slug.clone(),
                pr: args.pr,
                mode: run_mode(args.mode),
                max_iter: args.max_iter,
                status_comment: args.status_comment,
                state_root: args.state_root.clone(),
                legacy_trace: args.trace.clone(),
            }) {
                Ok(r) => r,
                Err(e) => {
                    return finish(Outcome::binary_error(format!("recorder: {e}")), None);
                }
            };
            recorder.install_process_recorder();
            let outcome = match args.mode {
                Mode::Inspect => run_inspect(&args, &recorder),
                Mode::Loop => run_full(&args, &recorder),
            };
            return finish(outcome, Some(recorder));
        }
        Err(usage_outcome) => usage_outcome,
    };
    finish(outcome, None)
}

fn run_mode(mode: Mode) -> RunMode {
    match mode {
        Mode::Loop => RunMode::Loop,
        Mode::Inspect => RunMode::Inspect,
    }
}

fn finish(outcome: Outcome, recorder: Option<Recorder>) -> ExitCode {
    let code = outcome.exit_code();
    render_outcome(&mut std::io::stderr(), &outcome);
    if let Some(recorder) = recorder {
        let mut rendered = Vec::new();
        render_outcome(&mut rendered, &outcome);
        if let Ok(text) = String::from_utf8(rendered) {
            for line in text.lines() {
                recorder.write_trace_line(line);
            }
        }
        recorder.record_outcome(&outcome, code);
    }
    ExitCode::from(code)
}

fn run_inspect(args: &Args, recorder: &Recorder) -> Outcome {
    recorder.set_iteration(Some(1));
    recorder.record_observe_start(1);
    let obs = match fetch_all(&args.slug, args.pr) {
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
        let line = format!(
            "stack: {} → {}",
            obs.pr_view.base_ref_name, obs.stack_root_branch,
        );
        eprintln!("{line}");
        recorder.write_trace_line(&line);
    }
    // Optional codex review observation. Inspect runs one pass — we
    // only read the filesystem, never spawn — so even an enabled
    // ceiling here is read-only.
    let codex_obs = match maybe_fetch_codex(args, recorder, obs.pr_view.head_ref_oid.as_str()) {
        Ok(o) => o,
        Err(e) => return e,
    };
    let oriented = orient(&obs, codex_obs.as_ref(), None, current_timestamp());
    let candidate_actions = candidates(&oriented);
    let decision = decide_from_candidates(candidate_actions.clone(), obs.pr_view.state);
    recorder.record_iteration(1, &obs, &oriented, &candidate_actions, &decision);
    if args.status_comment {
        let rendered = comment::render::render(&oriented, &decision);
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered, recorder, Some(1));
        log_post_result("comment", true, r, Some(recorder));
    }
    let snapshot = HandoffSnapshot {
        oriented: oriented.clone(),
        head_short: obs.pr_view.head_ref_oid.as_str().chars().take(7).collect(),
        base_branch: obs.pr_view.base_ref_name.to_string(),
    };
    decorate_handoff_human(
        Outcome::from(decision),
        &args.slug,
        args.pr,
        Some(&snapshot),
    )
}

fn run_full(args: &Args, recorder: &Recorder) -> Outcome {
    let codex_act = match build_codex_act_context(args, recorder) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let cfg = LoopConfig {
        max_iterations: args.max_iter,
        codex_review: args.codex_review_ceiling.map(|ceiling| CodexReviewConfig {
            floor: args.codex_review_floor,
            ceiling,
        }),
    };
    let ctx = ActContext {
        slug: args.slug.clone(),
        pr: args.pr,
        codex: codex_act,
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
        });
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
            let r =
                comment::post::post_if_changed(&args.slug, args.pr, &rendered, recorder, Some(i));
            log_post_result(&format!("[iter {i}] comment"), false, r, Some(recorder));
        }
    };
    let outcome = match run_loop(ctx, cfg, recorder, on_state) {
        Ok(reason) => Outcome::from(reason),
        Err(e) => Outcome::from(e),
    };
    decorate_handoff_human(outcome, &args.slug, args.pr, snapshot.as_ref())
}

/// Read the codex review batch state for inspect mode. Returns
/// `Ok(None)` when the axis is disabled or the head SHA isn't
/// available yet (codex review observation depends on the PR head
/// SHA, which inspect gets from the just-completed `fetch_all`).
fn maybe_fetch_codex(
    args: &Args,
    recorder: &Recorder,
    head_sha: &str,
) -> Result<Option<observe::codex::CodexObservations>, Outcome> {
    let Some(ceiling) = args.codex_review_ceiling else {
        return Ok(None);
    };
    let codex_pr_root = recorder.pr_root().join("codex");
    match fetch_codex(
        &codex_pr_root,
        args.codex_review_floor,
        ceiling,
        args.codex_review_n,
        head_sha,
    ) {
        Ok(o) => Ok(Some(o)),
        Err(e) => Err(Outcome::binary_error(format!(
            "observe (codex review): {e}"
        ))),
    }
}

/// Build the codex-review side of the `ActContext`. Returns `Ok(None)`
/// when the axis is disabled; otherwise discovers the repo root via
/// `git rev-parse --show-toplevel`, acquires an advisory `flock` on
/// `<codex_pr_root>/.lock` (so concurrent invocations against the
/// same PR don't race on batch dirs / head_sha.txt), and bundles
/// the spawn-time data for the runner to refresh with the
/// per-iteration head SHA + base branch.
fn build_codex_act_context(
    args: &Args,
    recorder: &Recorder,
) -> Result<Option<CodexActContext>, Outcome> {
    if args.codex_review_ceiling.is_none() {
        return Ok(None);
    }
    let repo_root = match discover_repo_root() {
        Ok(p) => p,
        Err(e) => return Err(Outcome::binary_error(format!("repo root: {e}"))),
    };
    let codex_pr_root = recorder.pr_root().join("codex");
    if let Err(e) = std::fs::create_dir_all(&codex_pr_root) {
        return Err(Outcome::binary_error(format!(
            "create codex pr_root {}: {e}",
            codex_pr_root.display()
        )));
    }
    let lock_path = codex_pr_root.join(".lock");
    let lock = match std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(Outcome::binary_error(format!(
                "open codex .lock at {}: {e}",
                lock_path.display()
            )));
        }
    };
    if let Err(e) = lock.try_lock() {
        return Err(Outcome::binary_error(format!(
            "another invocation holds the codex review lock at {} ({e}); concurrent ooda-pr-codex-review runs against the same PR with codex enabled are not supported — wait for the prior run to exit, or use --state-root to isolate",
            lock_path.display()
        )));
    }
    Ok(Some(CodexActContext {
        codex_bin: args.codex_review_bin.clone(),
        repo_root,
        codex_pr_root,
        n: args.codex_review_n,
        // head_sha and base_branch are refreshed by the runner each
        // iteration; placeholders here keep the fields non-Option
        // and avoid threading Option through the spawn path. Inspect
        // mode never spawns codex so it does not need these.
        head_sha: String::new(),
        base_branch: String::new(),
        _lock: lock,
    }))
}

fn discover_repo_root() -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("spawn `git rev-parse`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git rev-parse --show-toplevel` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err("`git rev-parse --show-toplevel` returned empty".into());
    }
    Ok(PathBuf::from(s))
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
#[derive(Debug, Clone)]
struct HandoffSnapshot {
    oriented: orient::OrientedState,
    head_short: String,
    base_branch: String,
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
fn decorate_handoff_human(
    outcome: Outcome,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    snapshot: Option<&HandoffSnapshot>,
) -> Outcome {
    match outcome {
        Outcome::HandoffHuman(mut action) => {
            push_handoff_context(&mut action, slug, pr, snapshot);
            Outcome::HandoffHuman(action)
        }
        Outcome::HandoffAgent(mut action) if agent_action_needs_context(&action.kind) => {
            push_handoff_context(&mut action, slug, pr, snapshot);
            Outcome::HandoffAgent(action)
        }
        other => other,
    }
}

/// `HandoffAgent` actions whose prompts benefit from the same
/// PR / branch / CI context the `HandoffHuman` decorator appends.
/// Today: `Rebase` (returning human triages the merge state).
/// Add new variants here rather than open-coding the match at the
/// call site.
fn agent_action_needs_context(kind: &decide::action::ActionKind) -> bool {
    matches!(kind, decide::action::ActionKind::Rebase)
}

/// Append the boundary context onto the handoff prompt.
/// `HandoffAction` exposes `prompt` as a direct field, so there's
/// no inner `match` on `ActionEffect` and no `unreachable!()` —
/// the structural projection done in `classify()` carries the
/// invariant.
fn push_handoff_context(
    handoff: &mut ooda_core::HandoffAction<decide::action::ActionKind>,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    snapshot: Option<&HandoffSnapshot>,
) {
    let blocker = handoff.blocker.to_string();
    let prompt = &mut handoff.prompt;
    prompt.push_context_line("PR", format!("https://github.com/{slug}/pull/{pr}"));
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
    fn decorate_handoff_human_appends_pr_link_and_blocker() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::tag("not_approved"),
        };
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated =
            decorate_handoff_human(Outcome::HandoffHuman(Box::new(handoff)), &slug, pr, None);
        let Outcome::HandoffHuman(handoff) = decorated else {
            panic!("expected HandoffHuman");
        };
        let rendered = handoff.prompt.to_string();
        assert!(
            rendered.contains("PR: https://github.com/acme/widget/pull/42"),
            "decoration: {rendered}",
        );
        assert!(
            rendered.contains("Blocker: not_approved"),
            "decoration: {rendered}",
        );
        assert!(rendered.starts_with("Request or self-approve"));
    }

    #[test]
    fn decorate_handoff_human_passes_through_other_variants() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("1").unwrap();
        let outcome = decorate_handoff_human(Outcome::DoneSucceeded, &slug, pr, None);
        assert!(matches!(outcome, Outcome::DoneSucceeded));
        let outcome = decorate_handoff_human(Outcome::Paused, &slug, pr, None);
        assert!(matches!(outcome, Outcome::Paused));
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
}
