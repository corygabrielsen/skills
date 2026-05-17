//! CLI entry point: parse argv, install signal handlers, run the loop.
//!
//! Zero domain knowledge. The fitness command is everything after `--`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ooda_core::ExitCode;

use crate::r#loop::{ConvergeOpts, converge};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_MAX_ITER: u32 = 20;

fn usage() -> ! {
    eprintln!(
        "Usage: converge [options] -- <command> [args...]

Observe→decide→act loop. Runs <command> repeatedly, reads a JSON
fitness report from its stdout, and dispatches prescribed actions
until the target score is reached or the iteration cap is hit.

Options:
  -n, --max-iter N     Iteration ceiling (default {DEFAULT_MAX_ITER})
  -s, --session ID     Session identifier (default: hash of command)
  --hook CMD           Coprocess for progress events (receives JSONL on stdin)
  -v, --verbose        Verbose trace to stderr
  -h, --help           This message
  --version            Version

Exit codes (see ooda-core::ExitCode for the family-wide table):
   0 DoneSucceeded       target reached
   3 HandoffHuman        human action required
   4 HandoffAgent        agent task required
   5 DoneAborted         subject reached terminal state
   6 StuckRepeated       no advancing actions
   7 StuckCapReached     iteration cap hit
  64 UsageError          CLI parse / validation failure
  70 BinaryError         caught runtime failure (incl. fitness unavailable, SIGINT trap)
 130 reserved            SIGINT (synthesized by shell when untrapped)
 143 reserved            SIGTERM (synthesized by shell when untrapped)
"
    );
    std::process::exit(0);
}

fn die(msg: &str) -> ! {
    eprintln!("converge: {msg}");
    std::process::exit(ExitCode::UsageError.as_u8().into());
}

/// djb2 hash for session-id derivation.
fn djb2(s: &str) -> String {
    let mut h: u32 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(u32::from(b));
    }
    format!("{h:x}")
}

struct ParsedArgs {
    fitness_argv: Vec<String>,
    max_iter: u32,
    session_id: Option<String>,
    hook_cmd: Option<String>,
    verbose: bool,
}

fn parse_args() -> ParsedArgs {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut max_iter = DEFAULT_MAX_ITER;
    let mut session_id = None;
    let mut hook_cmd = None;
    let mut verbose = false;
    let mut fitness_argv = Vec::new();
    let mut seen_separator = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if seen_separator {
            fitness_argv.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "--" {
            seen_separator = true;
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-h" | "--help" => usage(),
            "--version" => {
                println!("converge {VERSION}");
                std::process::exit(0);
            }
            "-v" | "--verbose" => verbose = true,
            "-n" | "--max-iter" => {
                i += 1;
                let val = args.get(i).unwrap_or_else(|| die("-n requires a value"));
                max_iter = val
                    .parse()
                    .unwrap_or_else(|_| die(&format!("invalid -n: {val}")));
            }
            "-s" | "--session" => {
                i += 1;
                let val = args.get(i).unwrap_or_else(|| die("-s requires a value"));
                session_id = Some(val.clone());
            }
            "--hook" => {
                i += 1;
                let val = args
                    .get(i)
                    .unwrap_or_else(|| die("--hook requires a value"));
                hook_cmd = Some(val.clone());
            }
            _ if arg.starts_with("--max-iter=") => {
                let val = &arg["--max-iter=".len()..];
                max_iter = val
                    .parse()
                    .unwrap_or_else(|_| die(&format!("invalid --max-iter: {val}")));
            }
            _ if arg.starts_with("--session=") => {
                session_id = Some(arg["--session=".len()..].to_string());
            }
            _ if arg.starts_with("--hook=") => {
                hook_cmd = Some(arg["--hook=".len()..].to_string());
            }
            _ if arg.starts_with('-') => {
                die(&format!("unknown option: {arg}"));
            }
            _ => {
                die(&format!("unexpected argument before --: {arg}"));
            }
        }
        i += 1;
    }

    if fitness_argv.is_empty() {
        die("missing fitness command after --");
    }

    ParsedArgs {
        fitness_argv,
        max_iter,
        session_id,
        hook_cmd,
        verbose,
    }
}

pub(crate) fn run() -> ExitCode {
    let parsed = parse_args();

    let session_id = parsed.session_id.unwrap_or_else(|| {
        let cmd_str = parsed.fitness_argv.join(" ");
        format!("s-{}", djb2(&cmd_str))
    });

    // Resume command is not converge's concern — the caller provides it
    // via the fitness skill wrapper. We store an empty one; the SKILL.md
    // tells the agent to re-run the same command.
    let resume_cmd: Vec<String> = Vec::new();

    eprintln!("session: /tmp/converge/{session_id}/");

    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let c = Arc::clone(&cancelled);
        ctrlc::set_handler(move || {
            c.store(true, Ordering::SeqCst);
        })
        .unwrap_or_else(|e| die(&format!("cannot install signal handler: {e}")));
    }

    let opts = ConvergeOpts {
        fitness_argv: parsed.fitness_argv,
        max_iter: parsed.max_iter,
        session_id,
        resume_cmd,
        hook_cmd: parsed.hook_cmd,
        verbose: parsed.verbose,
    };

    match converge(&opts, &cancelled) {
        Ok(halt) => ExitCode::from(halt.status),
        Err(msg) => {
            eprintln!("converge: {msg}");
            ExitCode::BinaryError
        }
    }
}
