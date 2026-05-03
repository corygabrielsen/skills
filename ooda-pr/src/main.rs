#![allow(dead_code)]

use std::process::ExitCode;
use std::time::Duration;

mod act;
mod comment;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod runner;
mod text;

use decide::action::Automation;
use decide::decide;
use decide::decision::Decision;
use ids::{PullRequestNumber, RepoSlug};
use observe::github::fetch_all;
use orient::orient;
use outcome::Outcome;
use runner::{run_loop, LoopConfig};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-pr — drive a PR through observe → orient → decide → act until halt.\n\
         \n\
         Usage:\n  ooda-pr [options] <owner/repo> <pr>           run the loop (default)\n  ooda-pr inspect [options] <owner/repo> <pr>   one pass; print Outcome; exit\n\
         \n\
         Options:\n  --max-iter N        loop iteration cap (default 50, must be ≥ 1; ignored by inspect)\n  --status-comment    post a status comment on the PR each iteration (deduped)\n  -h, --help          show this help and exit\n\
         \n\
         Exit codes (Outcome variants — see SKILL.md for the full taxonomy):\n  0 DoneMerged    1 StuckRepeated    2 StuckCapReached    3 HandoffHuman\n  4 WouldAdvance  5 HandoffAgent     6 BinaryError        7 Paused\n  8 DoneClosed    64 UsageError"
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
    max_iter: u32,
    status_comment: bool,
}

/// Parse CLI args. On failure, returns `Outcome::UsageError(_)` so
/// the boundary always speaks Outcome — no exception path.
fn parse_args() -> Result<Args, Outcome> {
    let mut mode = Mode::Loop;
    let mut max_iter: u32 = 50;
    let mut status_comment = false;
    let mut positional: Vec<String> = Vec::new();
    let mut saw_subcommand = false;
    let mut saw_max_iter = false;
    let mut saw_status_comment = false;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                // --help short-circuits all other validation. Stdout
                // is the ONLY allowed write target for ooda-pr; it
                // is reserved for help.
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
                let Ok(n) = v.parse::<u32>() else {
                    return Err(usage(&format!("--max-iter: not a non-negative integer: {v}")));
                };
                if n == 0 {
                    return Err(usage("--max-iter must be ≥ 1"));
                }
                max_iter = n;
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

    Ok(Args { mode, slug, pr, max_iter, status_comment })
}

fn usage(msg: &str) -> Outcome {
    // Newline-strip per SKILL.md UsageError invariant.
    let flat = if msg.contains('\n') { msg.replace('\n', " ") } else { msg.to_string() };
    Outcome::UsageError(flat)
}

fn main() -> ExitCode {
    let outcome = match parse_args() {
        Ok(args) => match args.mode {
            Mode::Inspect => run_inspect(&args),
            Mode::Loop => run_full(&args),
        },
        Err(usage_outcome) => usage_outcome,
    };
    let code = outcome.exit_code();
    render_outcome(&mut std::io::stderr(), &outcome);
    ExitCode::from(code)
}

fn run_inspect(args: &Args) -> Outcome {
    let obs = match fetch_all(&args.slug, args.pr) {
        Ok(o) => o,
        Err(e) => return Outcome::BinaryError(flatten(format!("observe: {e}"))),
    };
    if obs.stack_root_branch != obs.pr_view.base_ref_name {
        eprintln!(
            "stack: {} → {} (using {} for branch rules)",
            obs.pr_view.base_ref_name, obs.stack_root_branch, obs.stack_root_branch,
        );
    }
    let oriented = orient(&obs, None);
    let decision = decide(&oriented, obs.pr_view.state);
    if args.status_comment {
        let rendered = comment::render::render(&oriented, &decision);
        let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered);
        log_post_result("comment", true, r);
    }
    Outcome::from(decision)
}

fn run_full(args: &Args) -> Outcome {
    let cfg = LoopConfig { max_iterations: args.max_iter };
    let on_state = |i: u32, oriented: &orient::OrientedState, d: &Decision| {
        match d {
            Decision::Execute(action) => {
                eprintln!("[iter {i}] {} ({})", action.kind.name(), format_automation(&action.automation));
            }
            Decision::Halt(r) => {
                eprintln!("[iter {i}] halt: {r:?}");
            }
        }
        if args.status_comment {
            let rendered = comment::render::render(oriented, d);
            let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered);
            log_post_result(&format!("[iter {i}] comment"), false, r);
        }
    };
    match run_loop(&args.slug, args.pr, cfg, on_state) {
        Ok(reason) => Outcome::from(reason),
        Err(e) => Outcome::from(e),
    }
}

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
                action.kind.name(), action.blocker
            );
        }
        Outcome::StuckCapReached(opt) => match opt {
            Some(action) => {
                let _ = writeln!(
                    out,
                    "StuckCapReached: {}:{}",
                    action.kind.name(), action.blocker
                );
            }
            None => {
                let _ = writeln!(out, "StuckCapReached:");
            }
        },
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
            format_automation(&Automation::Wait { interval: Duration::from_secs(30) }),
            "Wait(30s)"
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
    fn render_stuck_cap_reached_none() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::StuckCapReached(None));
        assert_eq!(String::from_utf8(buf).unwrap(), "StuckCapReached:\n");
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
            automation: Automation::Wait { interval: Duration::from_secs(30) },
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
}
