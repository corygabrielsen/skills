use std::path::{Path, PathBuf};
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

use dashboard::Dashboard;
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::candidates;
use decide::decision::{Decision, DecisionHalt};
use ids::{PullRequestNumber, RepoSlug};
use observe::github::{FetchOutcome, fetch_all};
use ooda_core::decide_from_candidates;
use orient::orient;
use outcome::Outcome;
use recorder::{Recorder, RecorderConfig, RunMode};
use runner::{LoopConfig, current_timestamp, run_loop};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-pr — drive a PR through observe → orient → decide → act until halt.\n\
         \n\
         Usage:\n  ooda-pr [options] <owner/repo> <pr>           run the loop (default)\n  ooda-pr inspect [options] <owner/repo> <pr>   one pass; print Outcome; exit\n\
         \n\
         Options:\n  --max-iter N        loop iteration cap (default 50, must be ≥ 1; ignored by inspect)\n  --status-comment    post a status comment on the PR each iteration (deduped)\n  --state-root PATH   write always-on harness state under PATH\n  --trace PATH        also append the compact trace to PATH\n  -h, --help          show this help and exit\n\
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
    let mut positional: Vec<String> = Vec::new();
    let mut saw_subcommand = false;
    let mut saw_max_iter = false;
    let mut saw_status_comment = false;
    let mut saw_state_root = false;
    let mut saw_trace = false;

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

    Ok(Args {
        mode,
        slug,
        pr,
        max_iter,
        status_comment,
        state_root,
        trace,
    })
}

fn usage(msg: &str) -> Outcome {
    // SingleLineString enforces the SKILL.md "header is one
    // line" invariant — the From<&str> impl flattens any \n.
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
                    return finish(&Outcome::binary_error(format!("recorder: {e}")), None);
                }
            };
            recorder.install_process_recorder();
            let outcome = match args.mode {
                Mode::Inspect => run_inspect(&args, &recorder),
                Mode::Loop => run_full(&args, &recorder),
            };
            return finish(&outcome, Some(recorder));
        }
        Err(usage_outcome) => usage_outcome,
    };
    finish(&outcome, None)
}

fn run_mode(mode: Mode) -> RunMode {
    match mode {
        Mode::Loop => RunMode::Loop,
        Mode::Inspect => RunMode::Inspect,
    }
}

fn finish(outcome: &Outcome, recorder: Option<Recorder>) -> ExitCode {
    let code = outcome.exit_code();
    let handoff_path = match (outcome, recorder.as_ref()) {
        (Outcome::HandoffAgent(h) | Outcome::HandoffHuman(h), Some(r)) => {
            r.write_handoff_md(&h.prompt.to_string())
        }
        _ => None,
    };
    render_outcome(&mut std::io::stderr(), outcome, handoff_path.as_deref());
    if let Some(recorder) = recorder {
        let mut rendered = Vec::new();
        render_outcome(&mut rendered, outcome, handoff_path.as_deref());
        let mut headline = String::new();
        if let Ok(text) = String::from_utf8(rendered) {
            headline = text.lines().next().unwrap_or("").to_string();
            for line in text.lines() {
                recorder.write_trace_line(line);
            }
        }
        recorder.record_outcome(outcome, code, &headline, handoff_path.as_deref());
    }
    ExitCode::from(code)
}

fn run_inspect(args: &Args, recorder: &Recorder) -> Outcome {
    recorder.set_iteration(Some(1));
    recorder.record_observe_start(1);
    let obs = match fetch_all(&args.slug, args.pr, args.state_root.as_deref()) {
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
    if obs.stack_root_branch != obs.pull_request_view.base_ref_name {
        // Diagnostic note when the PR's immediate base differs
        // from the stack root used for branch-rule lookups. The
        // suffix repeated `<root>` was redundant; dropped to make
        // the line match the documented `stack: <base> → <root>`
        // format exactly.
        let line = format!(
            "stack: {} → {}",
            obs.pull_request_view.base_ref_name, obs.stack_root_branch,
        );
        eprintln!("{line}");
        recorder.write_trace_line(&line);
    }
    let oriented = orient(&obs, None, current_timestamp());
    let candidate_actions = candidates(&oriented, args.pr);
    let decision = decide_from_candidates(candidate_actions.clone(), obs.pull_request_view.state);
    recorder.record_iteration(1, &obs, &oriented, &candidate_actions, &decision);
    if args.status_comment {
        let rendered = comment::render::render(
            &args.slug,
            args.pr,
            Some(1),
            &oriented,
            &candidate_actions,
            &decision,
        );
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered, recorder, Some(1));
        log_post_result("comment", true, r, Some(recorder));
    }
    let snapshot = HandoffSnapshot {
        oriented: oriented.clone(),
        head_short: obs
            .pull_request_view
            .head_ref_oid
            .as_str()
            .chars()
            .take(7)
            .collect(),
        base_branch: obs.pull_request_view.base_ref_name.to_string(),
        dashboard: Dashboard::from_iteration(&oriented, &candidate_actions, &decision),
    };
    decorate_handoff_human(
        Outcome::from(decision),
        &args.slug,
        args.pr,
        Some(&snapshot),
    )
}

fn run_full(args: &Args, recorder: &Recorder) -> Outcome {
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
            head_short: obs
                .pull_request_view
                .head_ref_oid
                .as_str()
                .chars()
                .take(7)
                .collect(),
            base_branch: obs.pull_request_view.base_ref_name.to_string(),
            dashboard: Dashboard::from_iteration(oriented, candidate_actions, d),
        });
        recorder.set_iteration(Some(i));
        recorder.record_iteration(i, obs, oriented, candidate_actions, d);
        let line = iteration_line(i, d);
        eprintln!("{line}");
        recorder.write_trace_line(&line);
        if args.status_comment {
            let rendered = comment::render::render(
                &args.slug,
                args.pr,
                Some(i),
                oriented,
                candidate_actions,
                d,
            );
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
    let outcome = match run_loop(
        &args.slug,
        args.pr,
        args.state_root.as_deref(),
        cfg,
        recorder,
        on_state,
    ) {
        Ok(reason) => Outcome::from(reason),
        Err(e) => Outcome::from(e),
    };
    decorate_handoff_human(outcome, &args.slug, args.pr, snapshot.as_ref())
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
            // description: "..." }) into the action payload, which
            // breaks the one-line-per-iteration invariant.
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
        push_closeout_context_line(prompt, &snap.oriented);
    }
}

/// Append a `Closeout: attested at <ts> (sha <short>)` line when the
/// closeout axis is `Synced` at current HEAD AND the attestation file
/// is readable. No line otherwise — the absence is itself a signal
/// (Closeout never fires past convergence; absence on a `HandoffHuman`
/// path implies the loop bailed out before reaching the gate).
fn push_closeout_context_line(
    prompt: &mut ooda_core::HandoffPrompt,
    oriented: &orient::OrientedState,
) {
    if !matches!(oriented.closeout, orient::closeout::Closeout::Synced) {
        return;
    }
    let Some(path) = oriented.closeout_attest_path.as_deref() else {
        return;
    };
    let Ok(Some(att)) = ooda_core::attest::read_closeout(path) else {
        return;
    };
    prompt.push_context_line(
        "Closeout",
        format!(
            "attested at {} (sha {})",
            att.attested_at.to_rfc3339(),
            &att.attested_sha[..7],
        ),
    );
}

/// Render `Outcome` to a writer (typically stderr) per the SKILL
/// contract: single-line header, optionally followed by a pointer
/// block (`Handoff*` variants). No trailing content.
///
/// `handoff_path` is the absolute path of the per-iteration
/// `handoff.md` file the recorder wrote for this outcome
/// (`runs/<run-id>/iterations/<NNNN>/handoff.md`). When `Some`, the
/// emitted block is `  see: <path>` (a 7-byte sentinel + absolute
/// path on one line); the agent reads the prompt body from the
/// file. When `None` (recorder unavailable, or tests), the
/// fallback is the legacy `  prompt: <body>` inline block.
fn render_outcome(out: &mut dyn std::io::Write, oc: &Outcome, handoff_path: Option<&Path>) {
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
            write_handoff_block(out, &handoff.prompt.to_string(), handoff_path);
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
            write_handoff_block(out, &handoff.prompt.to_string(), handoff_path);
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

/// Write the handoff block. Two shapes:
///
/// - **Path form** (`handoff_path = Some`): one line beginning with
///   the literal 7-byte sequence `␣␣see:␣` (two spaces, "see",
///   colon, space) followed by the absolute path of the
///   per-iteration `handoff.md` file holding the prompt body
///   (`runs/<run-id>/iterations/<NNNN>/handoff.md`). The agent
///   reads that file to obtain the prompt; the file's size is
///   observable via `stat` before commit, so consumption has no
///   truncation pressure. This is the production path.
///
/// - **Inline fallback** (`handoff_path = None`): one line beginning
///   with the legacy 10-byte sequence `␣␣prompt:␣` followed by the
///   description content. Continuation lines carry no prefix; the
///   block ends at the last byte of content. Used by tests and as a
///   defensive fallback when the recorder is unavailable.
fn write_handoff_block(
    out: &mut dyn std::io::Write,
    description: &str,
    handoff_path: Option<&Path>,
) {
    if let Some(path) = handoff_path {
        let _ = writeln!(out, "  see: {}", path.display());
    } else {
        let _ = writeln!(out, "  prompt: {description}");
    }
}

/// Format `ActionEffect` for the `WouldAdvance` stderr render.
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

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::for_test(blocker),
        }
    }

    fn handoff(blocker: &str) -> ooda_core::HandoffAction<decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::for_test(blocker),
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
        assert_eq!(format_duration(Duration::from_mins(1)), "1m");
        assert_eq!(format_duration(Duration::from_mins(2)), "2m");
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
        render_outcome(&mut buf, &Outcome::DoneSucceeded, None);
        assert_eq!(String::from_utf8(buf).unwrap(), "DoneMerged\n");
    }

    #[test]
    fn render_paused() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::Paused, None);
        assert_eq!(String::from_utf8(buf).unwrap(), "Paused\n");
    }

    #[test]
    fn render_done_closed() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::DoneAborted, None);
        assert_eq!(String::from_utf8(buf).unwrap(), "DoneClosed\n");
    }

    #[test]
    fn render_stuck_cap_reached_carries_action() {
        let action = action("rebase-needed");
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::StuckCapReached(Box::new(action)), None);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "StuckCapReached: Rebase:rebase-needed\n"
        );
    }

    #[test]
    fn render_binary_error() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::BinaryError("gh: 401".into()), None);
        assert_eq!(String::from_utf8(buf).unwrap(), "BinaryError: gh: 401\n");
    }

    fn make_handoff_outcome(description: &str) -> Outcome {
        let action = decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new(description),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::from_static("rebase-needed"),
        };
        // Project the test Action down to a HandoffAction (the
        // shape Outcome::HandoffAgent carries post-refactor).
        let handoff = ooda_core::HandoffAction {
            kind: action.kind,
            prompt: match action.effect {
                ActionEffect::Agent { prompt } | ActionEffect::Human { prompt } => prompt,
                _ => panic!("test setup must produce a handoff effect"),
            },
            target_effect: action.target_effect,
            urgency: action.urgency,
            blocker: action.blocker,
        };
        Outcome::HandoffAgent(Box::new(handoff))
    }

    #[test]
    fn render_handoff_agent_fallback_inline_prompt() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &make_handoff_outcome("Rebase onto base"), None);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HandoffAgent: Rebase\n"));
        assert!(s.contains("\n  prompt: Rebase onto base\n"));
    }

    #[test]
    fn render_handoff_agent_pointer_form() {
        let mut buf = Vec::new();
        let path =
            Path::new("/state/github.com/acme/widget/prs/42/runs/r1/iterations/0001/handoff.md");
        render_outcome(
            &mut buf,
            &make_handoff_outcome("Rebase onto base"),
            Some(path),
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HandoffAgent: Rebase\n"));
        assert!(
            s.contains(
                "\n  see: /state/github.com/acme/widget/prs/42/runs/r1/iterations/0001/handoff.md\n",
            ),
            "rendered: {s}"
        );
        assert!(
            !s.contains("\n  prompt: "),
            "pointer form must not emit inline prompt block: {s}"
        );
    }

    #[test]
    fn decorate_handoff_human_appends_pull_request_link_and_blocker() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::from_static("not_approved"),
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
        // Original prompt content is preserved.
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
    fn decorate_handoff_agent_rebase_gets_pull_request_context() {
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
            blocker: ids::BlockerKey::from_static("behind_base"),
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
            blocker: ids::BlockerKey::from_static("changes_requested_summary"),
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

    // ── Phase B: dashboard preamble injection ─────────────────────

    fn snapshot_with_dashboard(candidates: &[decide::action::Action]) -> HandoffSnapshot {
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
        use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
        use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
        use crate::orient::reviews::{PendingReviews, ReviewSummary};
        use crate::orient::state::PullRequestProjection;
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
            state: PullRequestProjection {
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
                active_branch_rule_types: vec![],
                required_check_names_per_ruleset: vec![],
                missing_required_check_names_on_head: vec![],
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
            pull_request_metadata:
                orient::pull_request_metadata::PullRequestMetadata::NeverAttested,
            attest_path: None,
            doc_review: orient::doc_review::DocReview::NeverAttested,
            doc_review_attest_path: None,
            claude_review: orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
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
            blocker: ids::BlockerKey::from_static("behind_base"),
        }
    }

    #[test]
    fn decorate_handoff_human_prepends_dashboard_preamble() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::from_static("not_approved"),
        };
        let snap = snapshot_with_dashboard(&[rebase_action()]);
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
            blocker: ids::BlockerKey::from_static("behind_base"),
        };
        let snap = snapshot_with_dashboard(&[rebase_action()]);
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
            blocker: ids::BlockerKey::from_static("changes_requested_summary"),
        };
        let snap = snapshot_with_dashboard(&[rebase_action()]);
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
    fn decorate_handoff_human_surfaces_closeout_attestation_line_when_synced() {
        use ooda_core::attest::write_closeout_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("42").join("closeout_attest.json");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        write_closeout_atomic(&path, sha.to_string()).unwrap();

        let mut snap = snapshot_with_dashboard(&[rebase_action()]);
        snap.oriented.closeout = orient::closeout::Closeout::Synced;
        snap.oriented.closeout_attest_path = Some(path);

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::from_static("not_approved"),
        };
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
        assert!(
            rendered.contains("Closeout: attested at "),
            "decoration: {rendered}",
        );
        assert!(
            rendered.contains(&sha[..7]),
            "short sha must appear: {rendered}",
        );
    }

    #[test]
    fn decorate_handoff_human_omits_closeout_line_when_not_synced() {
        let mut snap = snapshot_with_dashboard(&[rebase_action()]);
        snap.oriented.closeout = orient::closeout::Closeout::NeverAttested;
        snap.oriented.closeout_attest_path = None;

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::from_static("not_approved"),
        };
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
        assert!(
            !rendered.contains("Closeout:"),
            "no closeout line expected when not Synced: {rendered}",
        );
    }

    #[test]
    fn decorate_handoff_human_omits_closeout_line_when_path_missing() {
        let mut snap = snapshot_with_dashboard(&[rebase_action()]);
        snap.oriented.closeout = orient::closeout::Closeout::Synced;
        snap.oriented.closeout_attest_path = None;

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
            blocker: ids::BlockerKey::from_static("not_approved"),
        };
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
        assert!(
            !rendered.contains("Closeout:"),
            "no closeout line expected without attest path: {rendered}",
        );
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
            blocker: ids::BlockerKey::from_static("waiting"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::WouldAdvance(Box::new(action)), None);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "WouldAdvance: Rebase:Wait(30s)\n"
        );
    }
}
