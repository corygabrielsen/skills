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
use decide::decision::{Decision, DecisionHalt, HaltReason, Terminal};
use ids::{PullRequestNumber, RepoSlug};
use observe::github::fetch_all;
use orient::orient;
use runner::{run_loop, LoopConfig};

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
    // The exit-code mapping lives on `Decision::exit_code` —
    // single source of truth shared with the full loop, so a probe
    // and a loop iteration return the same code for the same state.
    ExitCode::from(decision.exit_code())
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
        Ok(reason) => {
            print_halt(&reason);
            ExitCode::from(reason.exit_code())
        }
        Err(e) => {
            eprintln!("{e}");
            // gh / transport failure is exit 6, distinct from 1
            // (stalled). Drivers retry/alert on 6; 1 is reserved
            // for true state-machine stalls. This code does not
            // belong on `HaltReason` — a transport error is loop
            // *failure*, not a halt.
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
        Decision::Halt(halt) => print_decision_halt(halt),
    }
}

fn print_decision_halt(halt: &DecisionHalt) {
    match halt {
        DecisionHalt::Success => println!("Halt: Success — no advancing actions"),
        DecisionHalt::Terminal(Terminal::Merged) => println!("Halt: PR merged"),
        DecisionHalt::Terminal(Terminal::Closed) => println!("Halt: PR closed"),
        DecisionHalt::AgentNeeded(action) => {
            println!("Halt: AgentNeeded — {:?}", action.kind);
            println!("  description: {}", action.description);
        }
        DecisionHalt::HumanNeeded(action) => {
            println!("Halt: HumanNeeded — {:?}", action.kind);
            println!("  description: {}", action.description);
        }
    }
}

fn print_halt(reason: &HaltReason) {
    match reason {
        HaltReason::Decision(halt) => print_decision_halt(halt),
        HaltReason::Stalled => println!("Halt: Stalled"),
        HaltReason::CapReached { last_action } => {
            println!(
                "Halt: CapReached — last action: {:?}",
                last_action.as_ref().map(|a| &a.kind),
            );
        }
    }
}
