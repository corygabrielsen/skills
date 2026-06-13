//! ooda-codex-review — drive `codex review` to fixed point across
//! a reasoning-level ladder.
//!
//! Each iteration spawns `n` parallel review subprocesses at the
//! current level. The loop transitions per the in-batch state
//! machine (see [`decide`]); fixed point at the configured ceiling
//! halts terminally, otherwise the loop hands off to an outer
//! orchestrator for address-batch or retrospective work.
//!
//! # State model
//!
//! Each invocation creates a fresh run under
//! `<state-root>/runs/<run-id>/` via [`ooda_state`]. The run
//! captures the per-iteration observe/orient/decide/act event
//! stream in `events.jsonl`; bulky snapshots (observations,
//! handoff prompts) are content-addressed in `blobs/`. In-flight
//! codex review subprocess logs live in a per-run scratch
//! directory; observe scans them on each iteration.

use ooda_core::MidTier;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
// Aliased to avoid collision with `ooda_core::ExitCode` (re-exported
// throughout this binary). The two types are distinct: `ExitCode`
// is the typed family-wide enum; `ProcessExitCode` is the OS-facing
// `std::process::ExitCode` that `main` returns.
use std::process::ExitCode as ProcessExitCode;
use std::time::Duration;

use ooda_core::{SpawnError, SpawnLimits, run_with_limits};

/// Local git probe deadline: `rev-parse`, `config remote.origin.url`,
/// and similar git operations touch only the working tree's `.git`
/// directory and print one line. 10s caps a wedged git without
/// pinning the CLI boundary.
const GIT_LOCAL_DEADLINE: Duration = Duration::from_secs(10);

/// Per-stream byte cap for local git probes. `rev-parse` and
/// `git config` print a single line; 4 KiB tolerates long worktree
/// paths and remote URLs while keeping a runaway probe bounded.
const GIT_LOCAL_MAX_BYTES: usize = 4 * 1024;

/// Per-call deadline for `gh pr view`. The single REST call resolves
/// in the 1-2s range on a healthy network; 60s tolerates rate-limit
/// jitter and network blips while keeping a wedged subprocess from
/// stalling startup.
const GH_DEADLINE: Duration = Duration::from_mins(1);

/// Per-stream byte cap for `gh pr view`. The `--jq .baseRefName`
/// projection yields a single-line branch name; 256 KiB tolerates
/// noisy diagnostic preamble while keeping a malicious / wedged
/// subprocess from growing memory unbounded.
const GH_MAX_BYTES: usize = 256 * 1024;

/// Build the standard per-call limits for local git probes.
fn git_local_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GIT_LOCAL_DEADLINE,
        max_stdout_bytes: GIT_LOCAL_MAX_BYTES,
        max_stderr_bytes: GIT_LOCAL_MAX_BYTES,
    }
}

/// Build the standard per-call limits for `gh pr view`.
fn gh_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GH_DEADLINE,
        max_stdout_bytes: GH_MAX_BYTES,
        max_stderr_bytes: GH_MAX_BYTES,
    }
}

/// Render a [`SpawnError`] into a single-line diagnostic prefixed
/// with `context` (e.g., `"spawn `git rev-parse`"`). Used at every
/// top-level CLI subprocess site so timeout / pipe / wait failures
/// surface a typed deadline marker rather than a bare io error.
fn format_spawn_error(context: &str, err: SpawnError) -> String {
    match err {
        SpawnError::Spawn(io) => format!("{context}: {io}"),
        SpawnError::Timeout { deadline, killed } => format!(
            "{context} timed out after {}s ({})",
            deadline.as_secs(),
            if killed { "killed" } else { "kill failed" }
        ),
        SpawnError::OutputTooLarge {
            stream,
            limit,
            killed,
        } => format!(
            "{context} {stream} exceeded {limit}-byte cap ({})",
            if killed { "killed" } else { "kill failed" }
        ),
        SpawnError::Read(io) => format!("{context} (read output pipe): {io}"),
        SpawnError::Wait(io) => format!("{context} (wait on subprocess): {io}"),
    }
}

mod act;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod runner;
mod signal;

use act::ActContext;
use decide::action::{ActionKind, CodexReasoningLevel, TargetEffect, Urgency};
use ids::{BlockerKey, BranchName, GitCommitSha, RepoId, ReviewTarget};
use observe::codex::fetch_all;
use ooda_state::{
    CodexReviewDomain, EventBody, OutcomeKind, RunId, RunWriter, StateRoot, terminal_event,
};
use outcome::Outcome;
use runner::{EventSink, LoopConfig, LoopExit, run_loop};
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
         Options:\n  --level LVL                  reasoning level (= floor): low|medium|high|xhigh (default low)\n  --ceiling LVL                top of the ladder; all-clean here halts DoneFixedPoint (default xhigh, must be >= --level)\n  --codex-review-n N           parallel review count (default 3, must be ≥ 1)\n  --max-iter N                 loop iteration cap (default 50, must be ≥ 1)\n  --state-root PATH            OODA state-tree root (default $OODA_STATE_HOME or $XDG_STATE_HOME/ooda or $HOME/.local/state/ooda)\n  --codex-bin PATH             path to the `codex` binary (default `codex`)\n  --criteria STRING            unsupported with current `codex review` target modes; always UsageError\n  -h, --help                   show this help and exit\n\
         \n\
         Side-effect flags (skip the loop, emit a single ladder-transition event run, exit immediately. Mutually exclusive):\n  --advance-level              climb one rung (Idle at ceiling)\n  --drop-level                 drop one rung, clamp at floor (Idle at floor)\n  --restart-from-floor         reset current_level to floor\n  --mark-retro-clean           record Clean outcome; advance, or DoneFixedPoint at ceiling\n  --mark-retro-changes REASON  record RetrospectiveChanges outcome; restart from floor\n  --mark-address-passed        record Addressed outcome; drop one rung\n  --mark-address-failed DETAILS  emit HandoffHuman with DETAILS as prompt\n\
         \n\
         Exit codes (stderr header — see SKILL.md for variant mapping):\n   0 DoneFixedPoint    1 Idle               2 WouldAdvance      3 HandoffHuman\n   4 HandoffAgent      5 DoneAborted        6 StuckRepeated     7 StuckCapReached\n  64 UsageError       70 BinaryError       (130 SIGINT, 143 SIGTERM reserved)"
    );
}

// ----- args ------------------------------------------------------------

struct Args {
    /// Loop mode requires a target; side-effect mode never reads
    /// it. Encoded as `Option` so side-effect invocations can omit
    /// the target flag without manufacturing a placeholder.
    target: Option<ReviewTarget>,
    level: CodexReasoningLevel,
    ceiling: CodexReasoningLevel,
    n: std::num::NonZeroU32,
    max_iter: std::num::NonZeroU32,
    /// Explicit state-root override; `None` defers to
    /// [`ooda_state::resolve_state_root`].
    state_root: Option<PathBuf>,
    codex_bin: PathBuf,
    /// Side-effect requested by the orchestrator. `None` runs the
    /// OODA loop; `Some(_)` skips the loop and emits a single
    /// ladder-transition event before halting.
    side_effect: Option<SideEffect>,
}

/// Out-of-band ladder transitions the orchestrator may request
/// instead of running the loop. Mutually exclusive per invocation.
/// Each opens a fresh run, emits one decision/handoff event
/// recording the requested transition, and halts with the
/// documented [`Outcome`]. No state is carried across invocations:
/// the orchestrator passes the current ladder position via
/// `--level` and reads the resulting Outcome to know what to
/// pass next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SideEffect {
    /// `--advance-level` — climb one rung; idempotent at ceiling.
    AdvanceLevel,
    /// `--drop-level` — drop one rung, clamped at floor;
    /// idempotent at floor.
    DropLevel,
    /// `--restart-from-floor` — reset `current_level` to floor.
    RestartFromFloor,
    /// `--mark-retro-clean` — retrospective produced no
    /// architectural changes. Records the clean outcome; at
    /// ceiling halts terminal, otherwise advances.
    MarkRetroClean,
    /// `--mark-retro-changes "<reason>"` — retrospective surfaced
    /// architectural changes. Records the outcome and restarts
    /// from floor.
    MarkRetroChanges(String),
    /// `--mark-address-passed` — address agent fixed the batch
    /// and tests passed. Records the addressed outcome and drops
    /// one level (clamped at floor).
    MarkAddressPassed,
    /// `--mark-address-failed "<details>"` — post-address tests
    /// failed. No state transition; emits a human handoff with
    /// the details as the prompt.
    MarkAddressFailed(String),
}

fn parse_level(s: &str) -> Result<CodexReasoningLevel, String> {
    match s {
        "low" => Ok(CodexReasoningLevel::Low),
        "medium" => Ok(CodexReasoningLevel::Medium),
        "high" => Ok(CodexReasoningLevel::High),
        "xhigh" => Ok(CodexReasoningLevel::Xhigh),
        _ => Err(format!(
            "--level: unknown value `{s}` (expected: low|medium|high|xhigh)"
        )),
    }
}

fn usage(msg: impl Into<String>) -> Outcome {
    Outcome::usage_error(msg.into())
}

/// Pull a value-arg or report `<flag> requires a value`.
fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Outcome> {
    iter.next()
        .ok_or_else(|| usage(format!("{flag} requires a value")))
}

fn parse_positive_u32(flag: &str, raw: &str) -> Result<std::num::NonZeroU32, Outcome> {
    if raw.starts_with('-') {
        return Err(usage(format!(
            "{flag} must be ≥ 1; got negative value: {raw}"
        )));
    }
    if raw.starts_with('+') {
        return Err(usage(format!("{flag}: leading `+` not accepted: {raw}")));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| usage(format!("{flag}: not an integer: {raw}")))?;
    std::num::NonZeroU32::new(n).ok_or_else(|| usage(format!("{flag} must be ≥ 1; got 0")))
}

// Flat per-flag table: length IS the spec; one arm per known flag
// with its parse rules and error messages inline. Splitting into
// helpers would scatter the flag contract across files.
#[allow(clippy::too_many_lines)]
fn parse_args() -> Result<Args, Outcome> {
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print_usage(&mut std::io::stdout());
        std::process::exit(0);
    }

    let mut target: Option<ReviewTarget> = None;
    let mut level = CodexReasoningLevel::Low;
    let mut ceiling = CodexReasoningLevel::Xhigh;
    let mut n: std::num::NonZeroU32 = std::num::NonZeroU32::new(3).expect("3 is non-zero");
    let mut max_iter: std::num::NonZeroU32 = std::num::NonZeroU32::new(50).expect("50 is non-zero");
    let mut state_root: Option<PathBuf> = None;
    let mut codex_bin: PathBuf = PathBuf::from("codex");
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
                if v.starts_with('+') {
                    return Err(usage(format!("--pr: leading `+` not accepted: {v}")));
                }
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
            "--codex-review-n" => {
                let v = next_value(&mut iter, "--codex-review-n")?;
                n = parse_positive_u32("--codex-review-n", &v)?;
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
            "--advance-level" => set_side_effect(&mut side_effect, SideEffect::AdvanceLevel)?,
            "--drop-level" => set_side_effect(&mut side_effect, SideEffect::DropLevel)?,
            "--restart-from-floor" => {
                set_side_effect(&mut side_effect, SideEffect::RestartFromFloor)?;
            }
            "--mark-retro-clean" => set_side_effect(&mut side_effect, SideEffect::MarkRetroClean)?,
            "--mark-retro-changes" => {
                let v = next_value(&mut iter, "--mark-retro-changes")?;
                set_side_effect(&mut side_effect, SideEffect::MarkRetroChanges(v))?;
            }
            "--mark-address-passed" => {
                set_side_effect(&mut side_effect, SideEffect::MarkAddressPassed)?;
            }
            "--mark-address-failed" => {
                let v = next_value(&mut iter, "--mark-address-failed")?;
                set_side_effect(&mut side_effect, SideEffect::MarkAddressFailed(v))?;
            }
            other => return Err(usage(format!("unknown argument: {other}"))),
        }
    }

    // Loop mode requires a target; side-effect mode never reads
    // it. Reject a missing target only in loop mode.
    if target.is_none() && side_effect.is_none() {
        return Err(usage(
            "exactly one of --uncommitted / --base / --commit / --pr is required",
        ));
    }

    if ceiling < level {
        return Err(usage(format!(
            "--ceiling ({}) must be >= --level ({})",
            ceiling.as_str(),
            level.as_str()
        )));
    }

    Ok(Args {
        target,
        level,
        ceiling,
        n,
        max_iter,
        state_root,
        codex_bin,
        side_effect,
    })
}

// ----- repo discovery --------------------------------------------------

fn discover_repo_root() -> Result<PathBuf, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-parse", "--show-toplevel"]);
    let out = run_with_limits(&mut cmd, git_local_limits())
        .map_err(|e| format_spawn_error("spawn `git rev-parse`", e))?;
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
    let mut cmd = std::process::Command::new("gh");
    cmd.args([
        "pr",
        "view",
        &num_s,
        "--json",
        "baseRefName",
        "--jq",
        ".baseRefName",
    ])
    .current_dir(repo_root);
    let out = run_with_limits(&mut cmd, gh_limits()).map_err(|e| {
        format_spawn_error(
            &format!("resolve --pr {num} base branch: spawn `gh pr view`"),
            e,
        )
    })?;
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

    let url = {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["config", "remote.origin.url"])
            .current_dir(repo_root);
        // Best-effort probe: a missing / timed-out / errored
        // `git config` degrades to "no remote URL" so the repo
        // id falls back to the noremote@... key. Surfacing the
        // failure here would block a perfectly usable detached
        // worktree from getting an id at all.
        run_with_limits(&mut cmd, git_local_limits())
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    // Mix the canonical worktree path into the hash so parallel
    // worktrees of one repo hash distinct; same-worktree sequential
    // invocations stay stable because the toplevel path is
    // symlink-resolved and stable.
    let toplevel = repo_root.display().to_string();
    let key = match url {
        Some(u) => format!("{u}@{toplevel}"),
        None => format!("noremote@{toplevel}"),
    };
    let suffix = sha256_prefix(&key, 12);
    RepoId::parse(format!("{basename}-{suffix}")).map_err(|e| e.to_string())
}

fn sha256_prefix(input: &str, hex_chars: usize) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(hex_chars);
    for b in &digest {
        if s.len() >= hex_chars {
            break;
        }
        write!(s, "{b:02x}").expect("writing to String never fails");
    }
    s.truncate(hex_chars);
    s
}

// ----- run-started target payload --------------------------------------

/// Build the `target` payload for the `run_started` event. The
/// shape carries this binary's domain identity (review mode +
/// value, ladder bounds) without leaking PR-domain concepts like
/// repo slugs or PR numbers — those live in the orchestrator's
/// own event stream when needed.
///
/// Side-effect invocations omit `--uncommitted`/`--base`/`--commit`/`--pr`;
/// the payload renders `mode = "side-effect"` and `value = null`.
fn build_target_payload(
    target: Option<&ReviewTarget>,
    floor: CodexReasoningLevel,
    ceiling: CodexReasoningLevel,
) -> serde_json::Value {
    let (mode, value) = match target {
        None => ("side-effect", None),
        Some(ReviewTarget::Uncommitted) => ("uncommitted", None),
        Some(ReviewTarget::Base(b)) => ("base", Some(b.as_str().to_string())),
        Some(ReviewTarget::Commit(s)) => ("commit", Some(s.as_str().to_string())),
        Some(ReviewTarget::Pr(n)) => ("pr", Some(n.to_string())),
    };
    serde_json::json!({
        "mode": mode,
        "value": value,
        "floor": floor.as_str(),
        "ceiling": ceiling.as_str(),
    })
}

// ----- orchestration ---------------------------------------------------

fn run_session(args: &Args) -> Outcome {
    let state_root_path = ooda_state::resolve_state_root(args.state_root.as_deref());
    let state = match StateRoot::new(state_root_path) {
        Ok(s) => s,
        Err(e) => return Outcome::binary_error(format!("state root open: {e}")),
    };
    // Best-effort: reclaim disk for live markers left behind by
    // crashed prior runs (PID-derived liveness).
    let _ = state.sweep_dead_markers();
    let run_id = RunId::generate();
    let mut writer = match state.create_run(run_id.clone()) {
        Ok(w) => w,
        Err(e) => return Outcome::binary_error(format!("create run: {e}")),
    };
    if let Err(e) = writer.start(EventBody::RunStarted {
        domain: "codex-review".into(),
        target: build_target_payload(args.target.as_ref(), args.level, args.ceiling),
    }) {
        return Outcome::binary_error(format!("emit run_started: {e}"));
    }

    if let Some(side_effect) = args.side_effect.clone() {
        // Side-effect mode reads neither the git working tree nor
        // the repo identity; gating the git-shells off keeps a
        // non-repo invocation (CI dispatch, test harness) from
        // failing with `BinaryError(70)` for a property the side-
        // effect branch never observes.
        let outcome = apply_side_effect(&mut writer, args.level, args.ceiling, side_effect);
        finalize(&mut writer, &outcome);
        return outcome;
    }

    // Loop mode requires the git toplevel + repo identity. Resolve
    // them only on this branch so the side-effect mode above stays
    // side-effect-free with respect to git.
    let repo_root = match discover_repo_root() {
        Ok(p) => p,
        Err(e) => return Outcome::binary_error(e),
    };
    let repo_id = match compute_repo_id(&repo_root) {
        Ok(id) => id,
        Err(e) => return Outcome::binary_error(format!("compute repo id: {e}")),
    };

    let loop_target = args
        .target
        .clone()
        .expect("loop mode rejects missing --target at parse time");
    let codex_target = match resolve_codex_target(&loop_target, &repo_root) {
        Ok(target) => target,
        Err(e) => return Outcome::binary_error(e),
    };

    // Loop mode: spawn codex review subprocesses into a per-run
    // scratch dir. The dir lives beside `events.jsonl` and `blobs/`
    // inside this run; if the invocation halts with subprocesses
    // still writing, the scratch artifacts remain on disk attached
    // to the dead run (no cross-run sharing).
    let batch_dir = state
        .path()
        .join("runs")
        .join(run_id.as_str())
        .join("scratch");

    let ctx = ActContext {
        batch_dir: batch_dir.clone(),
        target: codex_target,
        repo_root,
        codex_bin: args.codex_bin.clone(),
    };

    let level = args.level;
    let n = args.n.get();
    let observe_target = loop_target.clone();
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

    let mut sink = EventSink::new(&mut writer);
    let result = run_loop(
        &repo_id,
        &loop_target,
        LoopConfig {
            max_iterations: args.max_iter,
            ceiling: args.ceiling,
        },
        &ctx,
        observe,
        &mut sink,
    );

    let outcome = match result {
        Ok(LoopExit::Halted(halt)) => Outcome::from(halt),
        Ok(LoopExit::SignalInterrupted { exit_code }) => Outcome::SignalInterrupted { exit_code },
        Err(e) => Outcome::from(e),
    };
    finalize(&mut writer, &outcome);
    outcome
}

/// Emit the terminal event for this run and release the live
/// marker. Errors are swallowed: the outcome has already been
/// computed and the caller needs it back regardless of whether
/// the audit-trail close succeeded.
fn finalize(writer: &mut RunWriter, outcome: &Outcome) {
    let kind = outcome_kind(outcome);
    let last_action = match outcome {
        Outcome::StuckRepeated(action) | Outcome::StuckCapReached(action) => {
            Some(action.kind.name().to_string())
        }
        _ => None,
    };
    let body = terminal_event(
        &CodexReviewDomain,
        kind,
        i32::from(outcome.exit_code().as_u8()),
        last_action.as_deref(),
    );
    let _ = writer.halt(body);
}

/// Project an [`Outcome`] onto its [`OutcomeKind`] discriminant —
/// see the analogous helper in each PR-side recorder. Strips the
/// payload so `ooda-state` can pick the wire token without
/// depending on `ooda-core`.
fn outcome_kind(outcome: &Outcome) -> OutcomeKind {
    match outcome {
        Outcome::DoneSucceeded => OutcomeKind::DoneSucceeded,
        Outcome::DoneAborted => OutcomeKind::DoneAborted,
        Outcome::Paused => OutcomeKind::Paused,
        Outcome::WouldAdvance(_) => OutcomeKind::WouldAdvance,
        Outcome::HandoffHuman(_) => OutcomeKind::HandoffHuman,
        Outcome::HandoffAgent(_) => OutcomeKind::HandoffAgent,
        Outcome::StuckRepeated(_) => OutcomeKind::StuckRepeated,
        Outcome::StuckCapReached(_) => OutcomeKind::StuckCapReached,
        Outcome::UsageError(_) => OutcomeKind::UsageError,
        Outcome::BinaryError(_) => OutcomeKind::BinaryError,
        Outcome::SignalInterrupted { .. } => OutcomeKind::SignalInterrupted,
    }
}

/// Emit the side-effect's transition event(s) and return the
/// documented Outcome. Each side-effect maps to one
/// `iteration_decided` event recording the requested transition;
/// `MarkAddressFailed` additionally writes the handoff prompt
/// blob and emits `iteration_handoff`.
///
/// `floor` is the bottom of the ladder (= `--level`). The
/// `RestartFromFloor` / `MarkRetroChanges` log lines render the
/// truthful "to floor: {floor}" transition; the prior `current`
/// rung is the orchestrator's state, not this binary's, so it is
/// not part of the rendered text.
///
/// Outcome map (see [`SideEffect`] for the per-variant rationale):
///
/// | SideEffect | Outcome |
/// |------------|---------|
/// | `AdvanceLevel` / `DropLevel` / `RestartFromFloor` | `Paused` |
/// | `MarkRetroClean` below ceiling | `Paused` |
/// | `MarkRetroClean` at ceiling | `DoneSucceeded` |
/// | `MarkRetroChanges` | `Paused` |
/// | `MarkAddressPassed` | `Paused` |
/// | `MarkAddressFailed` | `HandoffHuman` |
fn apply_side_effect(
    writer: &mut RunWriter,
    floor: CodexReasoningLevel,
    ceiling: CodexReasoningLevel,
    side_effect: SideEffect,
) -> Outcome {
    match side_effect {
        SideEffect::AdvanceLevel => match floor.higher() {
            Some(to) => emit_transition(
                writer,
                "AdvanceLevel",
                &format!("advanced level: {} -> {}", floor.as_str(), to.as_str()),
            ),
            None => emit_transition(
                writer,
                "AdvanceLevel",
                &format!("at ladder edge ({}); no advance", floor.as_str()),
            ),
        },
        SideEffect::DropLevel => match floor.lower() {
            Some(to) => emit_transition(
                writer,
                "DropLevel",
                &format!("dropped level: {} -> {}", floor.as_str(), to.as_str()),
            ),
            None => emit_transition(
                writer,
                "DropLevel",
                &format!("at floor ({}); no drop", floor.as_str()),
            ),
        },
        SideEffect::RestartFromFloor => emit_transition(
            writer,
            "RestartFromFloor",
            &format!("restarted to floor: {}", floor.as_str()),
        ),
        SideEffect::MarkRetroClean => apply_mark_retro_clean(writer, ceiling, floor),
        SideEffect::MarkRetroChanges(reason) => apply_mark_retro_changes(writer, floor, &reason),
        SideEffect::MarkAddressPassed => apply_mark_address_passed(writer, floor),
        SideEffect::MarkAddressFailed(details) => {
            apply_mark_address_failed(writer, floor, &details)
        }
    }
}

fn emit_transition(writer: &mut RunWriter, decision_kind: &str, log_line: &str) -> Outcome {
    let _ = writeln!(std::io::stdout(), "{log_line}");
    if let Err(e) = writer.append(EventBody::IterationDecided {
        iteration: 1,
        decision_kind: decision_kind.to_string(),
    }) {
        return Outcome::binary_error(format!("emit iteration_decided: {e}"));
    }
    Outcome::Paused
}

fn apply_mark_retro_clean(
    writer: &mut RunWriter,
    ceiling: CodexReasoningLevel,
    current: CodexReasoningLevel,
) -> Outcome {
    if current == ceiling {
        let _ = writeln!(
            std::io::stdout(),
            "retrospective clean at ceiling ({}); fixed point reached",
            current.as_str()
        );
        if let Err(e) = writer.append(EventBody::IterationDecided {
            iteration: 1,
            decision_kind: "RetroClean::Terminal".into(),
        }) {
            return Outcome::binary_error(format!("emit iteration_decided: {e}"));
        }
        return Outcome::DoneSucceeded;
    }
    let log_line = match current.higher() {
        Some(to) => format!(
            "retrospective clean at {}; advanced to {}",
            current.as_str(),
            to.as_str()
        ),
        None => format!(
            "retrospective clean at {}; ladder edge xhigh reached, no advance",
            current.as_str()
        ),
    };
    emit_transition(writer, "RetroClean::Advance", &log_line)
}

fn apply_mark_retro_changes(
    writer: &mut RunWriter,
    floor: CodexReasoningLevel,
    reason: &str,
) -> Outcome {
    // Log renders the truthful transition: "restart to floor".
    // The prior `current` rung is the orchestrator's state, not
    // this binary's, so it does not appear here.
    let log_line = format!(
        "retrospective surfaced changes (\"{}\"); restarted to floor: {}",
        reason,
        floor.as_str()
    );
    emit_transition(writer, "RetroChanges::RestartFromFloor", &log_line)
}

fn apply_mark_address_passed(writer: &mut RunWriter, current: CodexReasoningLevel) -> Outcome {
    let log_line = match current.lower() {
        Some(to) => format!(
            "address passed at {}; dropped to {}",
            current.as_str(),
            to.as_str()
        ),
        None => format!("address passed at floor {}; no drop", current.as_str()),
    };
    emit_transition(writer, "AddressPassed", &log_line)
}

fn apply_mark_address_failed(
    writer: &mut RunWriter,
    level: CodexReasoningLevel,
    details: &str,
) -> Outcome {
    let handoff = ooda_core::HandoffAction {
        kind: ActionKind::TestsFailedTriage,
        prompt: ooda_core::HandoffPrompt::new(format!(
            "Tests failed after addressing review batch at level {}. \
             Surface to a human for triage. Details: {}",
            level.as_str(),
            details
        )),
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("address-failed"),
    };
    // Stash the prompt body as a blob so the audit trail captures
    // the verbatim handoff text without inlining it on the event.
    let prompt_bytes = handoff.prompt.to_string().into_bytes();
    match writer.write_blob(&prompt_bytes, "md") {
        Ok(blob) => {
            if let Err(e) = writer.append(EventBody::IterationHandoff {
                iteration: 1,
                variant: OutcomeKind::HandoffHuman.variant_name().to_string(),
                action_kind: handoff.kind.name().to_string(),
                blob,
            }) {
                return Outcome::binary_error(format!("emit iteration_handoff: {e}"));
            }
        }
        Err(e) => return Outcome::binary_error(format!("write handoff blob: {e}")),
    }
    Outcome::HandoffHuman(Box::new(handoff))
}

fn main() -> ProcessExitCode {
    // Install signal handlers before any loop work: a `SIGTERM`
    // arriving during args-parse should be picked up on the first
    // iteration boundary instead of killing the process uncleanly.
    // Failure to install is reported as a binary error rather than
    // silently dropping the graceful-shutdown contract.
    if let Err(e) = signal::install_signal_handlers() {
        let outcome: Outcome = Outcome::binary_error(format!("install signal handlers: {e}"));
        let code = outcome.exit_code();
        render_outcome(&mut std::io::stderr(), &outcome);
        return ProcessExitCode::from(code);
    }
    let outcome = match parse_args() {
        Ok(args) => run_session(&args),
        Err(e) => e,
    };
    let code = outcome.exit_code();
    render_outcome(&mut std::io::stderr(), &outcome);
    ProcessExitCode::from(code)
}

/// Render `Outcome` to stderr per the wire contract: single-line
/// variant header on the first line, with a following prompt
/// block only for handoff variants.
fn render_outcome(out: &mut dyn std::io::Write, oc: &Outcome) {
    match oc {
        Outcome::DoneSucceeded => {
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
        Outcome::HandoffHuman(handoff) => {
            let _ = writeln!(out, "HandoffHuman: {}", handoff.kind.name());
            let _ = writeln!(out, "  prompt: {}", handoff.prompt);
        }
        Outcome::WouldAdvance(action) => {
            let _ = writeln!(out, "WouldAdvance: {}", action.kind.name());
        }
        Outcome::HandoffAgent(handoff) => {
            let _ = writeln!(out, "HandoffAgent: {}", handoff.kind.name());
            let _ = writeln!(out, "  prompt: {}", handoff.prompt);
        }
        Outcome::BinaryError(msg) => {
            let _ = writeln!(out, "BinaryError: {msg}");
        }
        Outcome::Paused => {
            let _ = writeln!(out, "Idle");
        }
        Outcome::DoneAborted => {
            let _ = writeln!(out, "DoneAborted");
        }
        Outcome::UsageError(msg) => {
            let _ = writeln!(out, "UsageError: {msg}");
            print_usage(out);
        }
        Outcome::SignalInterrupted { exit_code } => {
            let _ = writeln!(out, "Interrupted: exit code {exit_code}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parses_all_four() {
        assert_eq!(parse_level("low").unwrap(), CodexReasoningLevel::Low);
        assert_eq!(parse_level("medium").unwrap(), CodexReasoningLevel::Medium);
        assert_eq!(parse_level("high").unwrap(), CodexReasoningLevel::High);
        assert_eq!(parse_level("xhigh").unwrap(), CodexReasoningLevel::Xhigh);
        assert!(parse_level("LOW").is_err());
        assert!(parse_level("max").is_err());
    }

    #[test]
    fn sha256_prefix_truncates() {
        let p = sha256_prefix("hello", 12);
        assert_eq!(p.len(), 12);
        assert!(p.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Two worktrees of the same remote at different toplevel
    /// paths must hash to distinct `repo_id`s. The test reproduces
    /// the key-construction shape inline rather than invoking the
    /// git-shelling production path.
    #[test]
    fn worktree_path_disambiguates_repo_id_hash() {
        let url = "git@example.invalid:org/repo.git";
        let key_a = format!("{url}@/work/a/repo");
        let key_b = format!("{url}@/work/b/repo");
        let h_a = sha256_prefix(&key_a, 12);
        let h_b = sha256_prefix(&key_b, 12);
        assert_ne!(h_a, h_b, "different worktree paths must hash differently");
        assert_eq!(h_a, sha256_prefix(&key_a, 12), "same key, same hash");
    }

    #[test]
    fn target_payload_carries_mode_and_ladder_bounds() {
        let payload = build_target_payload(
            Some(&ReviewTarget::Uncommitted),
            CodexReasoningLevel::Low,
            CodexReasoningLevel::Xhigh,
        );
        assert_eq!(payload["mode"], "uncommitted");
        assert!(payload["value"].is_null());
        assert_eq!(payload["floor"], "low");
        assert_eq!(payload["ceiling"], "xhigh");

        let branch = BranchName::parse("master").unwrap();
        let payload = build_target_payload(
            Some(&ReviewTarget::Base(branch)),
            CodexReasoningLevel::Medium,
            CodexReasoningLevel::High,
        );
        assert_eq!(payload["mode"], "base");
        assert_eq!(payload["value"], "master");
        assert_eq!(payload["floor"], "medium");
        assert_eq!(payload["ceiling"], "high");
    }

    #[test]
    fn target_payload_side_effect_mode_when_target_absent() {
        let payload =
            build_target_payload(None, CodexReasoningLevel::Low, CodexReasoningLevel::Xhigh);
        assert_eq!(payload["mode"], "side-effect");
        assert!(payload["value"].is_null());
        assert_eq!(payload["floor"], "low");
        assert_eq!(payload["ceiling"], "xhigh");
    }
}
