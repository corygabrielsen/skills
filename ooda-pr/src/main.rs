use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod act;
mod comment;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod text;

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
    let outcome = match run_loop(&args.slug, args.pr, cfg, recorder, on_state) {
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
#[derive(Debug, Clone)]
struct HandoffSnapshot {
    oriented: orient::OrientedState,
    head_short: String,
    base_branch: String,
}

/// Append a PR-context block to `HandoffHuman` prompts so the
/// stderr handoff is usable on its own — no tab-juggling. Pass-
/// through for every other `Outcome` variant.
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
        other => other,
    }
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
        let req = &snap.oriented.ci.required;
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

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag(blocker),
        }
    }

    fn handoff(blocker: &str) -> ooda_core::HandoffAction<decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingHuman,
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
        let action = decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Rebase onto base"),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::BlockingFix,
            blocker: ids::BlockerKey::tag("rebase-needed"),
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
