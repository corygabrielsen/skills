#![allow(dead_code)]

//! ooda-codex-review — anti-DRY copy of /ooda-pr retargeted at
//! `codex review`. Drives n parallel reviews per reasoning level,
//! halts for AddressBatch/Retrospective handoffs to the outer
//! Claude session. See project_ooda_codex_review.md for the plan.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod act;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod text;

use act::ActContext;
use decide::action::{Action, ActionKind, Automation, ReasoningLevel, TargetEffect, Urgency};
use ids::{BlockerKey, BranchName, GitCommitSha, RepoId, ReviewTarget};
use observe::codex::fetch_all;
use outcome::Outcome;
use recorder::{LevelOutcome, Recorder, RecorderConfig};
use runner::{LoopConfig, run_loop};
use sha2::{Digest, Sha256};
use std::io::Write;

// ----- usage -----------------------------------------------------------

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-codex-review — drive `codex review` to fixed point across the reasoning ladder.\n\
         \n\
         Usage:\n  ooda-codex-review [options] <mode>\n\
         \n\
         Mode (exactly one required):\n  --uncommitted       review working-tree changes vs HEAD\n  --base BRANCH       review current branch vs BRANCH\n  --commit SHA        review a specific commit (40-hex SHA)\n  --pr NUM            review a specific PR's changes\n\
         \n\
         Options:\n  --level LVL                  reasoning level (= floor): low|medium|high|xhigh (default low)\n  --ceiling LVL                top of the ladder; all-clean here halts DoneFixedPoint (default xhigh, must be >= --level)\n  -n N                         parallel review count (default 3, must be ≥ 1)\n  --max-iter N                 loop iteration cap (default 50, must be ≥ 1)\n  --state-root PATH            directory for batch logs (default $TMPDIR/ooda-codex-review)\n  --codex-bin PATH             path to the `codex` binary (default `codex`)\n  --criteria STRING            unsupported with current `codex review` target modes; always UsageError\n  --fresh                      ignore the latest pointer; start a new run\n  -h, --help                   show this help and exit\n\
         \n\
         Side-effect flags (skip the loop, mutate the recorder, exit immediately. Mutually exclusive):\n  --advance-level              climb one rung (Idle at ceiling)\n  --drop-level                 drop one rung, clamp at floor (Idle at floor)\n  --restart-from-floor         reset current_level to floor\n  --mark-retro-clean           record Clean outcome; advance, or DoneFixedPoint at ceiling\n  --mark-retro-changes REASON  record RetrospectiveChanges outcome; restart from floor\n  --mark-address-passed        record Addressed outcome; drop one rung\n  --mark-address-failed DETAILS  emit HandoffHuman with DETAILS as prompt\n\
         \n\
         Exit codes (Outcome variants — see SKILL.md for the full taxonomy):\n  0 DoneFixedPoint    1 StuckRepeated    2 StuckCapReached    3 HandoffHuman\n  4 WouldAdvance      5 HandoffAgent     6 BinaryError        7 Idle\n  8 DoneAborted       64 UsageError"
    );
}

// ----- args ------------------------------------------------------------

struct Args {
    target: ReviewTarget,
    level: ReasoningLevel,
    ceiling: ReasoningLevel,
    n: u32,
    max_iter: u32,
    state_root: PathBuf,
    codex_bin: PathBuf,
    fresh: bool,
    /// Side-effect requested by the orchestrator. `None` means
    /// "run the OODA loop". `Some(_)` short-circuits the loop and
    /// returns directly with the variant's documented Outcome.
    side_effect: Option<SideEffect>,
}

/// Side-effect commands. Mutually exclusive within a single
/// invocation. Each opens the recorder (resuming the prior run),
/// performs its mutation, and returns a documented Outcome
/// without running the OODA loop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SideEffect {
    /// `--advance-level` — climb one rung. Idle at ceiling.
    AdvanceLevel,
    /// `--drop-level` — drop one rung, clamp at floor. Idle at
    /// floor.
    DropLevel,
    /// `--restart-from-floor` — reset current_level to floor.
    RestartFromFloor,
    /// `--mark-retro-clean` — orchestrator reports the
    /// retrospective produced no architectural changes. Records
    /// `LevelOutcome::Clean`. At ceiling: emit `DoneFixedPoint`.
    /// Below ceiling: advance and emit `Idle`.
    MarkRetroClean,
    /// `--mark-retro-changes "<reason>"` — orchestrator reports
    /// the retrospective surfaced architectural changes. Records
    /// `LevelOutcome::RetrospectiveChanges` and restarts from
    /// floor. Emits `Idle`.
    MarkRetroChanges(String),
    /// `--mark-address-passed` — orchestrator reports the address
    /// agent fixed the batch and tests passed. Records
    /// `LevelOutcome::Addressed` (with the issue count derived
    /// from the current batch's verdicts) and drops one level
    /// (clamped at floor). Emits `Idle`.
    MarkAddressPassed,
    /// `--mark-address-failed "<details>"` — orchestrator reports
    /// post-address tests failed. No transition; emits
    /// `HandoffHuman` with the details as the prompt.
    MarkAddressFailed(String),
}

fn default_state_root() -> PathBuf {
    std::env::temp_dir().join("ooda-codex-review")
}

fn parse_level(s: &str) -> Result<ReasoningLevel, String> {
    match s {
        "low" => Ok(ReasoningLevel::Low),
        "medium" => Ok(ReasoningLevel::Medium),
        "high" => Ok(ReasoningLevel::High),
        "xhigh" => Ok(ReasoningLevel::Xhigh),
        _ => Err(format!(
            "--level: unknown value `{s}` (expected: low|medium|high|xhigh)"
        )),
    }
}

fn usage(msg: impl Into<String>) -> Outcome {
    let s = msg.into();
    let flat = if s.contains('\n') {
        s.replace('\n', " ")
    } else {
        s
    };
    Outcome::UsageError(flat)
}

/// Pull a value-arg or report `<flag> requires a value`.
fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Outcome> {
    iter.next()
        .ok_or_else(|| usage(format!("{flag} requires a value")))
}

fn parse_positive_u32(flag: &str, raw: &str) -> Result<u32, Outcome> {
    if raw.starts_with('-') {
        return Err(usage(format!(
            "{flag} must be ≥ 1; got negative value: {raw}"
        )));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| usage(format!("{flag}: not an integer: {raw}")))?;
    if n == 0 {
        return Err(usage(format!("{flag} must be ≥ 1; got 0")));
    }
    Ok(n)
}

fn parse_args() -> Result<Args, Outcome> {
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print_usage(&mut std::io::stdout());
        std::process::exit(0);
    }

    let mut target: Option<ReviewTarget> = None;
    let mut level = ReasoningLevel::Low;
    let mut ceiling = ReasoningLevel::Xhigh;
    let mut n: u32 = 3;
    let mut max_iter: u32 = 50;
    let mut state_root: Option<PathBuf> = None;
    let mut codex_bin: PathBuf = PathBuf::from("codex");
    let mut fresh = false;
    let mut side_effect: Option<SideEffect> = None;

    let set_side_effect = |slot: &mut Option<SideEffect>, new: SideEffect| -> Result<(), Outcome> {
        if slot.is_some() {
            return Err(usage(
                "side-effect flags (--advance-level / --drop-level / --restart-from-floor / --mark-*) are mutually exclusive",
            ));
        }
        *slot = Some(new);
        Ok(())
    };

    let mut set_target = |new: ReviewTarget| -> Result<(), Outcome> {
        if target.is_some() {
            return Err(usage(
                "exactly one of --uncommitted / --base / --commit / --pr is required",
            ));
        }
        target = Some(new);
        Ok(())
    };

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => unreachable!("pre-scan handles --help"),
            "--uncommitted" => set_target(ReviewTarget::Uncommitted)?,
            "--base" => {
                let v = next_value(&mut iter, "--base")?;
                let b = BranchName::parse(&v).map_err(|e| usage(e.to_string()))?;
                set_target(ReviewTarget::Base(b))?;
            }
            "--commit" => {
                let v = next_value(&mut iter, "--commit")?;
                let s = GitCommitSha::parse(&v).map_err(|e| usage(e.to_string()))?;
                set_target(ReviewTarget::Commit(s))?;
            }
            "--pr" => {
                let v = next_value(&mut iter, "--pr")?;
                let num: u64 = v
                    .parse()
                    .map_err(|_| usage(format!("--pr: not a positive integer: {v}")))?;
                if num == 0 {
                    return Err(usage("--pr must be ≥ 1; got 0"));
                }
                set_target(ReviewTarget::Pr(num))?;
            }
            "--level" => {
                let v = next_value(&mut iter, "--level")?;
                level = parse_level(&v).map_err(usage)?;
            }
            "--ceiling" => {
                let v = next_value(&mut iter, "--ceiling")?;
                ceiling = parse_level(&v).map_err(usage)?;
            }
            "-n" => {
                let v = next_value(&mut iter, "-n")?;
                n = parse_positive_u32("-n", &v)?;
            }
            "--max-iter" => {
                let v = next_value(&mut iter, "--max-iter")?;
                max_iter = parse_positive_u32("--max-iter", &v)?;
            }
            "--state-root" => {
                let v = next_value(&mut iter, "--state-root")?;
                state_root = Some(PathBuf::from(v));
            }
            "--codex-bin" => {
                let v = next_value(&mut iter, "--codex-bin")?;
                codex_bin = PathBuf::from(v);
            }
            "--criteria" => {
                let _ = next_value(&mut iter, "--criteria")?;
                return Err(usage(
                    "--criteria is not supported by the current `codex review` CLI when a target mode is used; omit it and use codex's built-in review criteria",
                ));
            }
            "--fresh" => {
                fresh = true;
            }
            "--advance-level" => set_side_effect(&mut side_effect, SideEffect::AdvanceLevel)?,
            "--drop-level" => set_side_effect(&mut side_effect, SideEffect::DropLevel)?,
            "--restart-from-floor" => {
                set_side_effect(&mut side_effect, SideEffect::RestartFromFloor)?
            }
            "--mark-retro-clean" => set_side_effect(&mut side_effect, SideEffect::MarkRetroClean)?,
            "--mark-retro-changes" => {
                let v = next_value(&mut iter, "--mark-retro-changes")?;
                set_side_effect(&mut side_effect, SideEffect::MarkRetroChanges(v))?;
            }
            "--mark-address-passed" => {
                set_side_effect(&mut side_effect, SideEffect::MarkAddressPassed)?
            }
            "--mark-address-failed" => {
                let v = next_value(&mut iter, "--mark-address-failed")?;
                set_side_effect(&mut side_effect, SideEffect::MarkAddressFailed(v))?;
            }
            other => return Err(usage(format!("unknown argument: {other}"))),
        }
    }

    let target = target.ok_or_else(|| {
        usage("exactly one of --uncommitted / --base / --commit / --pr is required")
    })?;

    if ceiling < level {
        return Err(usage(format!(
            "--ceiling ({}) must be >= --level ({})",
            ceiling.as_str(),
            level.as_str()
        )));
    }

    // --fresh + a side-effect has no defined semantics: starting a
    // new run and immediately mutating its empty manifest produces
    // a meaningless state (e.g. --mark-address-passed records issue
    // count 0; --mark-retro-clean records Clean against a level
    // with no review history). Reject the combination at parse
    // time to keep the surface honest.
    if fresh && side_effect.is_some() {
        return Err(usage(
            "--fresh cannot be combined with a side-effect flag (--mark-* / --advance-level / --drop-level / --restart-from-floor): the side-effect would mutate a brand-new manifest with no review history",
        ));
    }

    Ok(Args {
        target,
        level,
        ceiling,
        n,
        max_iter,
        state_root: state_root.unwrap_or_else(default_state_root),
        codex_bin,
        fresh,
        side_effect,
    })
}

// ----- repo discovery --------------------------------------------------

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

fn resolve_codex_target(target: &ReviewTarget, repo_root: &Path) -> Result<ReviewTarget, String> {
    match target {
        ReviewTarget::Pr(num) => resolve_pr_base(*num, repo_root).map(ReviewTarget::Base),
        other => Ok(other.clone()),
    }
}

fn resolve_pr_base(num: u64, repo_root: &Path) -> Result<BranchName, String> {
    let num_s = num.to_string();
    let out = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &num_s,
            "--json",
            "baseRefName",
            "--jq",
            ".baseRefName",
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("resolve --pr {num} base branch: spawn `gh pr view`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "resolve --pr {num} base branch: `gh pr view` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        return Err(format!(
            "resolve --pr {num} base branch: `gh pr view` returned an empty baseRefName"
        ));
    }
    BranchName::parse(&branch)
        .map_err(|e| format!("resolve --pr {num} base branch `{branch}`: {e}"))
}

fn compute_repo_id(repo_root: &Path) -> Result<RepoId, String> {
    let basename = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    let url = std::process::Command::new("git")
        .args(["config", "remote.origin.url"])
        .current_dir(repo_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let suffix = match url {
        Some(u) => sha256_prefix(&u, 12),
        None => "noremote".to_string(),
    };
    RepoId::parse(format!("{basename}-{suffix}")).map_err(|e| e.to_string())
}

fn sha256_prefix(input: &str, hex_chars: usize) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(hex_chars);
    for b in digest.iter() {
        if s.len() >= hex_chars {
            break;
        }
        s.push_str(&format!("{b:02x}"));
    }
    s.truncate(hex_chars);
    s
}

// ----- orchestration ---------------------------------------------------

fn run_session(args: Args) -> Outcome {
    let repo_root = match discover_repo_root() {
        Ok(p) => p,
        Err(e) => return Outcome::BinaryError(e),
    };
    let repo_id = match compute_repo_id(&repo_root) {
        Ok(id) => id,
        Err(e) => return Outcome::BinaryError(format!("compute repo id: {e}")),
    };

    let codex_target = if args.side_effect.is_none() {
        match resolve_codex_target(&args.target, &repo_root) {
            Ok(target) => Some(target),
            Err(e) => return Outcome::BinaryError(e),
        }
    } else {
        None
    };

    let (mut recorder, _open_mode) = match Recorder::open(RecorderConfig {
        state_root: args.state_root.clone(),
        repo_id: repo_id.clone(),
        target: args.target.clone(),
        start_level: args.level,
        batch_size: args.n,
        fresh: args.fresh,
        now: None,
    }) {
        Ok(pair) => pair,
        Err(e) => return Outcome::BinaryError(format!("recorder open: {e}")),
    };

    if let Some(side_effect) = args.side_effect.clone() {
        return apply_side_effect(&mut recorder, args.ceiling, side_effect);
    }

    let batch_dir = recorder.batch_dir();
    let current_level = recorder.manifest().current_level;

    let ctx = ActContext {
        batch_dir: batch_dir.clone(),
        target: codex_target.expect("loop mode resolves codex target"),
        repo_root,
        codex_bin: args.codex_bin.clone(),
    };

    let level = current_level;
    let n = args.n;
    let observe_target = args.target.clone();
    let observe_repo_id = repo_id.clone();
    let observe = move |_r: &RepoId, _t: &ReviewTarget| {
        fetch_all(
            observe_repo_id.clone(),
            observe_target.clone(),
            &batch_dir,
            level,
            n,
        )
        .map_err(|e| e.to_string())
    };

    let result = run_loop(
        &repo_id,
        &args.target,
        LoopConfig {
            max_iterations: args.max_iter,
            ceiling: args.ceiling,
        },
        &ctx,
        observe,
        |_iter, _oriented, _decision| {},
    );

    match result {
        Ok(halt) => Outcome::from(halt),
        Err(e) => Outcome::from(e),
    }
}

/// Apply one side-effect against the recorder and return the
/// documented Outcome. Each command writes a human-readable line
/// to stdout describing what changed so the orchestrator can log
/// it.
///
/// Outcome map:
///   AdvanceLevel / DropLevel / RestartFromFloor    -> Idle
///   MarkRetroClean (below ceiling)                 -> Idle
///   MarkRetroClean (at ceiling)                    -> DoneFixedPoint
///   MarkRetroChanges                               -> Idle
///   MarkAddressPassed                              -> Idle
///   MarkAddressFailed                              -> HandoffHuman
fn apply_side_effect(
    recorder: &mut Recorder,
    ceiling: ReasoningLevel,
    side_effect: SideEffect,
) -> Outcome {
    let from = recorder.manifest().current_level;
    match side_effect {
        SideEffect::AdvanceLevel => match recorder.advance_level() {
            Ok(Some(to)) => log_idle(format!(
                "advanced level: {} -> {}",
                from.as_str(),
                to.as_str()
            )),
            Ok(None) => log_idle(format!("at ladder edge ({}); no advance", from.as_str())),
            Err(e) => Outcome::BinaryError(format!("recorder advance: {e}")),
        },
        SideEffect::DropLevel => match recorder.drop_level() {
            Ok(Some(to)) => log_idle(format!(
                "dropped level: {} -> {}",
                from.as_str(),
                to.as_str()
            )),
            Ok(None) => log_idle(format!("at floor ({}); no drop", from.as_str())),
            Err(e) => Outcome::BinaryError(format!("recorder drop: {e}")),
        },
        SideEffect::RestartFromFloor => match recorder.restart_from_floor() {
            Ok(to) => log_idle(format!(
                "restarted from floor: {} -> {}",
                from.as_str(),
                to.as_str()
            )),
            Err(e) => Outcome::BinaryError(format!("recorder restart: {e}")),
        },
        SideEffect::MarkRetroClean => apply_mark_retro_clean(recorder, ceiling, from),
        SideEffect::MarkRetroChanges(reason) => apply_mark_retro_changes(recorder, from, reason),
        SideEffect::MarkAddressPassed => apply_mark_address_passed(recorder, from),
        SideEffect::MarkAddressFailed(details) => mk_handoff_human_test_failed(from, details),
    }
}

fn log_idle(msg: String) -> Outcome {
    let _ = writeln!(std::io::stdout(), "{msg}");
    Outcome::Idle
}

fn apply_mark_retro_clean(
    recorder: &mut Recorder,
    ceiling: ReasoningLevel,
    current: ReasoningLevel,
) -> Outcome {
    if let Err(e) = recorder.record_outcome(LevelOutcome::Clean { level: current }) {
        return Outcome::BinaryError(format!("recorder record-outcome: {e}"));
    }
    if current == ceiling {
        let _ = writeln!(
            std::io::stdout(),
            "retrospective clean at ceiling ({}); fixed point reached",
            current.as_str()
        );
        return Outcome::DoneFixedPoint;
    }
    match recorder.advance_level() {
        Ok(Some(to)) => log_idle(format!(
            "retrospective clean at {}; advanced to {}",
            current.as_str(),
            to.as_str()
        )),
        Ok(None) => log_idle(format!(
            "retrospective clean at {}; ladder edge xhigh reached, no advance",
            current.as_str()
        )),
        Err(e) => Outcome::BinaryError(format!("recorder advance: {e}")),
    }
}

fn apply_mark_retro_changes(
    recorder: &mut Recorder,
    current: ReasoningLevel,
    reason: String,
) -> Outcome {
    if let Err(e) = recorder.record_outcome(LevelOutcome::RetrospectiveChanges {
        level: current,
        reason: reason.clone(),
    }) {
        return Outcome::BinaryError(format!("recorder record-outcome: {e}"));
    }
    match recorder.restart_from_floor() {
        Ok(to) => log_idle(format!(
            "retrospective surfaced changes at {} (\"{}\"); restarted from floor {}",
            current.as_str(),
            reason,
            to.as_str()
        )),
        Err(e) => Outcome::BinaryError(format!("recorder restart: {e}")),
    }
}

fn apply_mark_address_passed(recorder: &mut Recorder, current: ReasoningLevel) -> Outcome {
    let issue_count = count_current_batch_issues(recorder, current);
    if let Err(e) = recorder.record_outcome(LevelOutcome::Addressed {
        level: current,
        issue_count,
    }) {
        return Outcome::BinaryError(format!("recorder record-outcome: {e}"));
    }
    match recorder.drop_level() {
        Ok(Some(to)) => log_idle(format!(
            "address passed at {} ({} review(s) with issues); dropped to {}",
            current.as_str(),
            issue_count,
            to.as_str()
        )),
        Ok(None) => match recorder.start_next_batch_at_current_level() {
            Ok(batch) => log_idle(format!(
                "address passed at floor {} ({} review(s) with issues); no drop; advanced to batch {}",
                current.as_str(),
                issue_count,
                batch
            )),
            Err(e) => Outcome::BinaryError(format!("recorder next-batch: {e}")),
        },
        Err(e) => Outcome::BinaryError(format!("recorder drop: {e}")),
    }
}

/// Walk the verdicts in the current batch dir and return how many
/// reviewers flagged issues. Best-effort — returns 0 on read
/// errors so the side-effect path stays robust to filesystem
/// transients (the count is observational, not load-bearing).
fn count_current_batch_issues(recorder: &Recorder, level: ReasoningLevel) -> u32 {
    let batch_size = recorder.manifest().batch_size;
    match observe::codex::batch::scan_batch(&recorder.batch_dir(), level, batch_size) {
        Ok(observe::codex::batch::BatchState::Complete { verdicts }) => verdicts
            .iter()
            .filter(|v| matches!(v.class, observe::codex::VerdictClass::HasIssues))
            .count() as u32,
        _ => 0,
    }
}

fn mk_handoff_human_test_failed(level: ReasoningLevel, details: String) -> Outcome {
    let action = Action {
        kind: ActionKind::TestsFailedTriage,
        automation: Automation::Human,
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingHuman,
        description: format!(
            "Tests failed after addressing review batch at level {}. \
             Surface to a human for triage. Details: {}",
            level.as_str(),
            details
        ),
        blocker: BlockerKey::tag("address-failed"),
    };
    Outcome::HandoffHuman(action)
}

fn main() -> ExitCode {
    let outcome = match parse_args() {
        Ok(args) => run_session(args),
        Err(e) => e,
    };
    let code = outcome.exit_code();
    render_outcome(&mut std::io::stderr(), &outcome);
    ExitCode::from(code)
}

/// Render `Outcome` to stderr per the SKILL contract: single-line
/// header, optionally followed by a prompt block for `Handoff*`
/// variants.
fn render_outcome(out: &mut dyn std::io::Write, oc: &Outcome) {
    match oc {
        Outcome::DoneFixedPoint => {
            let _ = writeln!(out, "DoneFixedPoint");
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
            let _ = writeln!(out, "  prompt: {}", action.description);
        }
        Outcome::WouldAdvance(action) => {
            let _ = writeln!(out, "WouldAdvance: {}", action.kind.name());
        }
        Outcome::HandoffAgent(action) => {
            let _ = writeln!(out, "HandoffAgent: {}", action.kind.name());
            let _ = writeln!(out, "  prompt: {}", action.description);
        }
        Outcome::BinaryError(msg) => {
            let _ = writeln!(out, "BinaryError: {msg}");
        }
        Outcome::Idle => {
            let _ = writeln!(out, "Idle");
        }
        Outcome::DoneAborted => {
            let _ = writeln!(out, "DoneAborted");
        }
        Outcome::UsageError(msg) => {
            let _ = writeln!(out, "UsageError: {msg}");
            print_usage(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parses_all_four() {
        assert_eq!(parse_level("low").unwrap(), ReasoningLevel::Low);
        assert_eq!(parse_level("medium").unwrap(), ReasoningLevel::Medium);
        assert_eq!(parse_level("high").unwrap(), ReasoningLevel::High);
        assert_eq!(parse_level("xhigh").unwrap(), ReasoningLevel::Xhigh);
        assert!(parse_level("LOW").is_err());
        assert!(parse_level("max").is_err());
    }

    #[test]
    fn sha256_prefix_truncates() {
        let p = sha256_prefix("hello", 12);
        assert_eq!(p.len(), 12);
        assert!(p.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
