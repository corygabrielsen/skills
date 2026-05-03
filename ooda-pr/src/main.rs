#![allow(dead_code)]

use std::process::ExitCode;

mod act;
mod comment;
mod decide;
mod ids;
mod observe;
mod orient;
mod runner;

use decide::decide;
use decide::decision::{Decision, HaltReason, Terminal};
use ids::{PullRequestNumber, RepoSlug};
use observe::github::fetch_all;
use orient::orient;
use runner::{run_loop, LoopConfig, LoopOutcome};

fn usage() -> ExitCode {
    eprintln!(
        "usage: ooda-pr [--once] [--max-iter N] [--comment] <owner/repo> <pr>\n\
         \n\
         Drives a PR through observe → orient → decide → act until halt.\n\
         \n\
         Options:\n  --once          one observe/orient/decide pass; no act, no loop\n  --max-iter N    iteration cap for the loop (default: 50)\n  --comment       post a fitness comment on the PR each iteration (deduped)"
    );
    ExitCode::from(64)
}

struct Args {
    slug: RepoSlug,
    pr: PullRequestNumber,
    once: bool,
    max_iter: u32,
    comment: bool,
}

fn parse_args() -> Result<Args, ExitCode> {
    let mut once = false;
    let mut max_iter: u32 = 50;
    let mut comment = false;
    let mut positional: Vec<String> = Vec::new();

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--once" => once = true,
            "--comment" => comment = true,
            "--max-iter" => {
                let Some(v) = iter.next() else {
                    eprintln!("--max-iter requires a value");
                    return Err(usage());
                };
                let Ok(n) = v.parse::<u32>() else {
                    eprintln!("--max-iter: not a number: {v}");
                    return Err(usage());
                };
                max_iter = n;
            }
            _ if arg.starts_with("--") => {
                eprintln!("unknown flag: {arg}");
                return Err(usage());
            }
            _ => positional.push(arg),
        }
    }

    if positional.len() != 2 {
        return Err(usage());
    }
    // Argument parse failures are usage errors (exit 64), not loop
    // stalls (exit 1). Exit-code taxonomy is the documented driver
    // contract — collapsing the two would let `ooda-pr noslash 123`
    // look like a stall to outer drivers dispatching on exit codes.
    let slug = RepoSlug::parse(&positional[0]).map_err(|e| {
        eprintln!("{e}");
        ExitCode::from(64)
    })?;
    let pr = PullRequestNumber::parse(&positional[1]).map_err(|e| {
        eprintln!("{e}");
        ExitCode::from(64)
    })?;

    Ok(Args { slug, pr, once, max_iter, comment })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return code,
    };

    if args.once {
        return run_once(&args);
    }

    run_full(&args)
}

fn run_once(args: &Args) -> ExitCode {
    let obs = match fetch_all(&args.slug, args.pr) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("observe: {e}");
            // Runtime / transport failure (gh auth, network, missing
            // CLI) — exit 6, distinct from 1 (stalled). Wrappers
            // dispatching on the documented taxonomy can retry/alert
            // instead of treating the failure as a state-machine
            // stall.
            return ExitCode::from(6);
        }
    };
    if obs.stack_root_branch != obs.pr_view.base_ref_name {
        eprintln!(
            "stack: {} → {} (using {} for branch rules)",
            obs.pr_view.base_ref_name,
            obs.stack_root_branch,
            obs.stack_root_branch,
        );
    }
    let oriented = orient(&obs, None);
    let decision = decide(&oriented, obs.pr_view.state);
    print_decision(&decision);
    if args.comment {
        let rendered = comment::render::render(&oriented, &decision);
        let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered);
        log_post_result("comment", true, r);
    }
    // --once must mirror the documented exit-code contract so that
    // wrappers using it as a probe see the same Halt class as the
    // full loop. Pre-fix this always returned SUCCESS, letting
    // BLOCKED PRs look green to outer drivers.
    decision_exit_code(&decision)
}

/// Single source of truth for Decision → ExitCode. Used by both
/// `--once` and the full loop's halt branch so the exit-code
/// contract is identical regardless of mode.
///
/// `Decision::Execute` maps to exit 4 ("in_progress"): the loop
/// would auto-run the action, but `--once` does not. From the
/// probe's perspective the PR has NOT reached the documented
/// success state — wrappers using `--once` to gate must see a
/// non-zero exit so a still-advancing PR doesn't look green.
/// Exit 4 is distinct from 1 (stalled) / 3 (human) / 5 (agent).
fn decision_exit_code(d: &Decision) -> ExitCode {
    match d {
        Decision::Execute(_) => ExitCode::from(4),
        Decision::Halt(reason) => halt_exit_code(reason),
    }
}

fn halt_exit_code(reason: &HaltReason) -> ExitCode {
    match reason {
        HaltReason::Success | HaltReason::Terminal(_) => ExitCode::SUCCESS,
        HaltReason::AgentNeeded(_) => ExitCode::from(5),
        HaltReason::HumanNeeded(_) => ExitCode::from(3),
        HaltReason::Stalled => ExitCode::from(1),
    }
}

fn run_full(args: &Args) -> ExitCode {
    let cfg = LoopConfig { max_iterations: args.max_iter };
    let on_state = |i: u32, oriented: &orient::OrientedState, d: &Decision| {
        match d {
            Decision::Execute(action) => {
                eprintln!("[iter {i}] {:?} ({:?})", action.kind, action.automation);
            }
            Decision::Halt(r) => {
                eprintln!("[iter {i}] halt: {r:?}");
            }
        }
        if args.comment {
            let rendered = comment::render::render(oriented, d);
            let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered);
            log_post_result(&format!("[iter {i}] comment"), false, r);
        }
    };
    match run_loop(&args.slug, args.pr, cfg, on_state) {
        Ok(LoopOutcome::Done(reason)) => {
            print_halt(&reason);
            ExitCode::SUCCESS
        }
        Ok(LoopOutcome::Halted(reason)) => {
            print_halt(&reason);
            // Exit code mirrors the halt class so outer drivers can
            // dispatch without parsing stdout. Single source of
            // truth: same mapping as --once.
            halt_exit_code(&reason)
        }
        Ok(LoopOutcome::CapReached { last_action }) => {
            eprintln!(
                "iteration cap reached; last action: {:?}",
                last_action.map(|a| a.kind)
            );
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e}");
            // Same as --once: gh / transport failure is exit 6, not
            // 1. Drivers retry/alert on 6; 1 is reserved for true
            // state-machine stalls.
            ExitCode::from(6)
        }
    }
}

/// Log the outcome of a comment post. `verbose_skip` controls
/// whether the unchanged/dedup case prints — verbose for --once
/// (so the user sees the dedup happened), silent for the loop
/// (where it's the common case).
fn log_post_result(
    prefix: &str,
    verbose_skip: bool,
    r: Result<bool, comment::post::PostError>,
) {
    match r {
        Ok(true) => eprintln!("{prefix}: posted"),
        Ok(false) if verbose_skip => eprintln!("{prefix}: skipped (unchanged)"),
        Ok(false) => {}
        Err(e) => eprintln!("{prefix}: {e}"),
    }
}

fn print_decision(d: &Decision) {
    match d {
        Decision::Execute(action) => {
            println!("Execute: {:?} ({:?})", action.kind, action.automation);
            println!("  blocker:     {}", action.blocker);
            println!("  description: {}", action.description);
        }
        Decision::Halt(reason) => print_halt(reason),
    }
}

fn print_halt(reason: &HaltReason) {
    match reason {
        HaltReason::Success => println!("Halt: Success — no advancing actions"),
        HaltReason::Terminal(Terminal::Merged) => println!("Halt: PR merged"),
        HaltReason::Terminal(Terminal::Closed) => println!("Halt: PR closed"),
        HaltReason::AgentNeeded(action) => {
            println!("Halt: AgentNeeded — {:?}", action.kind);
            println!("  description: {}", action.description);
        }
        HaltReason::HumanNeeded(action) => {
            println!("Halt: HumanNeeded — {:?}", action.kind);
            println!("  description: {}", action.description);
        }
        HaltReason::Stalled => println!("Halt: Stalled"),
    }
}
