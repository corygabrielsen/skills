use std::path::{Path, PathBuf};
// Aliased to avoid collision with `ooda_core::ExitCode` (used via
// the `Outcome::exit_code()` projection below). The two types are
// distinct: `ExitCode` is the typed family-wide enum; `ProcessExitCode`
// is the OS-facing `std::process::ExitCode` that `main` returns.
use std::process::ExitCode as ProcessExitCode;
use std::time::Duration;

mod act;
mod axis_impls;
mod comment;
mod dashboard;
mod decide;
mod ids;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod signal;
mod text;

use act::{ActContext, CodexActContext};
use dashboard::Dashboard;
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::decision::{Decision, DecisionHalt};
use ids::{CodexReasoningLevel, PullRequestNumber, RepoSlug};
use observe::codex::fetch_all as fetch_codex;
use observe::github::{FetchOutcome, fetch_all};
use ooda_core::{SpawnError, SpawnLimits, decide_from_candidates, run_with_limits};
use ooda_state::ObserveOutcome;
use orient::orient;
use outcome::Outcome;
use recorder::{CodexReviewSnapshot, Recorder, RecorderConfig, RunMode};
use runner::{CodexReviewConfig, LoopConfig, LoopExit, current_timestamp, run_loop};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-pr-codex-review — drive a PR through observe → orient → decide → act until halt, optionally with local codex review.\n\
         \n\
         Usage:\n  ooda-pr-codex-review [options] <owner/repo> <pr>           run the loop (default)\n  ooda-pr-codex-review inspect [options] <owner/repo> <pr>   one pass; print Outcome; exit\n\
         \n\
         Options:\n  --max-iter N                  loop iteration cap (default 50, must be ≥ 1; ignored by inspect)\n  --status-comment              post a status comment on the PR each iteration (deduped)\n  --state-root PATH             write always-on harness state under PATH\n  --repo-root PATH              target working tree for all `gt`/`git`/codex invocations\n                                (default: derive from CWD via `git rev-parse --show-toplevel`)\n  --codex-review-ceiling LVL    enable codex review with ceiling LVL: off|low|medium|high|xhigh (default off — codex review disabled)\n  --codex-review-floor LVL      codex review starting rung: low|medium|high|xhigh (default low; must be ≤ ceiling)\n  --codex-review-n N            codex review parallel reviewers per level (default 3, must be ≥ 1)\n  --codex-review-bin PATH       path to the codex binary (default codex, PATH lookup)\n  -h, --help                    show this help and exit\n\
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
    /// Optional explicit override for the target working tree. When
    /// `None`, [`resolve_repo_root`] derives it from the process CWD
    /// via `git rev-parse --show-toplevel`. The resolved path is the
    /// CWD pin for every `gt` subprocess (sync, log-stack, version)
    /// AND for the codex subprocess; without it, an invocation from
    /// a sibling repo can mutate the wrong stack or diff against the
    /// wrong base.
    repo_root: Option<PathBuf>,
    /// Discriminant for the codex-review axis. `None` disables it
    /// (axis-disabled behavior is observationally identical to the
    /// no-codex sibling binary); `Some(level)` enables with that
    /// level as the reasoning-ladder upper bound.
    codex_review_ceiling: Option<CodexReasoningLevel>,
    /// Reasoning-ladder lower bound. Invariant: `floor ≤ ceiling`
    /// when the axis is enabled (validated in `parse_args`).
    codex_review_floor: CodexReasoningLevel,
    /// Per-batch parallelism. Invariant: ≥ 1.
    codex_review_n: u32,
    /// Codex binary location. Defaults to PATH lookup.
    codex_review_bin: PathBuf,
}

// `parse_ceiling` / `parse_level` superseded by the clap
// `ValueEnum` derives on `CeilingArg` / `FloorArg` below (F9).

/// Parse CLI args into `Args` or a synthetic `Outcome::UsageError`.
///
/// Backed by clap; see `ooda-pr::parse_args` for the F9 migration
/// rationale and the seven bugs it closes.
fn parse_args() -> Result<Args, Outcome> {
    use clap::Parser;
    if std::env::args_os().skip(1).any(|a| {
        let s = a.to_string_lossy();
        s == "-h" || s == "--help"
    }) {
        print_usage(&mut std::io::stdout());
        std::process::exit(0);
    }
    let raw = match CliRaw::try_parse_from(std::env::args_os()) {
        Ok(r) => r,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                print_usage(&mut std::io::stdout());
                std::process::exit(0);
            }
            return Err(usage(&format_clap_error(&e)));
        }
    };

    let (mode, slug_raw, pr_raw) = match raw.sub {
        Some(SubCmd::Inspect { slug, pr }) => (Mode::Inspect, slug, pr),
        None => match (raw.slug, raw.pr) {
            (Some(s), Some(p)) => (Mode::Loop, s, p),
            _ => {
                return Err(usage(
                    "expected exactly 2 positionals (owner/repo, pr); got fewer",
                ));
            }
        },
    };

    let slug = RepoSlug::parse(&slug_raw).map_err(|e| usage(&e.to_string()))?;
    let pr = PullRequestNumber::parse(&pr_raw).map_err(|e| usage(&e.to_string()))?;

    // Disjunctive parse: ceiling / floor / n carry None when their
    // own flag is unset, so the "axis disabled" branch can detect
    // the no-ceiling case without inheriting silent defaults.
    let codex_review_ceiling = raw.codex_review_ceiling.and_then(CeilingArg::into_level);
    let saw_bin = raw.codex_review_bin.is_some();
    let saw_floor = raw.codex_review_floor.is_some();
    let saw_n = raw.codex_review_n.is_some();
    let codex_review_floor = raw
        .codex_review_floor
        .map_or(CodexReasoningLevel::Low, FloorArg::into_level);
    let codex_review_n = raw.codex_review_n.unwrap_or(3);
    let codex_review_bin = raw
        .codex_review_bin
        .unwrap_or_else(|| PathBuf::from("codex"));

    if let Some(ceiling) = codex_review_ceiling
        && codex_review_floor > ceiling
    {
        return Err(usage(&format!(
            "--codex-review-floor ({}) must be ≤ --codex-review-ceiling ({})",
            codex_review_floor.as_str(),
            ceiling.as_str()
        )));
    }
    // Cross-flag dependency: --codex-review-{bin,floor,n} all tune
    // the codex-review axis. With --codex-review-ceiling unset, the
    // axis is disabled and the tuning flags would be silently
    // ignored — reject the inconsistent invocation at the boundary.
    if codex_review_ceiling.is_none() && (saw_bin || saw_floor || saw_n) {
        return Err(usage(
            "--codex-review-{bin|floor|n} requires --codex-review-ceiling",
        ));
    }

    Ok(Args {
        mode,
        slug,
        pr,
        max_iter: raw.max_iter,
        status_comment: raw.status_comment,
        state_root: raw.state_root,
        repo_root: raw.repo_root,
        codex_review_ceiling,
        codex_review_floor,
        codex_review_n,
        codex_review_bin,
    })
}

fn format_clap_error(e: &clap::Error) -> String {
    let raw = e.to_string();
    let mut first = raw
        .lines()
        .find(|line| line.starts_with("error:"))
        .unwrap_or_else(|| raw.lines().next().unwrap_or(""))
        .trim_start_matches("error:")
        .trim()
        .to_string();
    if first.is_empty() {
        first = raw.lines().next().unwrap_or("").to_string();
    }
    first
}

/// clap-facing surface. Maps onto [`Args`] via the small adapter in
/// [`parse_args`]. Sub-enums for `--codex-review-ceiling` /
/// `--codex-review-floor` exist so the parser carries
/// `Option<EnumVariant>` — used by the disabled-axis detection.
#[derive(clap::Parser, Debug)]
#[command(
    name = "ooda-pr-codex-review",
    about = "drive a PR through observe → orient → decide → act with optional codex review",
    disable_help_flag = false,
    disable_version_flag = true,
    arg_required_else_help = false
)]
struct CliRaw {
    /// loop iteration cap (must be ≥ 1; ignored by inspect)
    #[arg(long, value_parser = parse_max_iter_value, default_value_t = std::num::NonZeroU32::new(50).expect("50 is non-zero"))]
    max_iter: std::num::NonZeroU32,

    /// post a status comment on the PR each iteration (deduped)
    #[arg(long)]
    status_comment: bool,

    /// always-on harness state root
    #[arg(long, value_parser = parse_existing_state_root)]
    state_root: Option<PathBuf>,

    /// target working tree for all `gt`/`git`/codex invocations
    #[arg(long)]
    repo_root: Option<PathBuf>,

    /// codex review ceiling: off|low|medium|high|xhigh
    #[arg(long, value_enum)]
    codex_review_ceiling: Option<CeilingArg>,

    /// codex review starting rung: low|medium|high|xhigh
    #[arg(long, value_enum)]
    codex_review_floor: Option<FloorArg>,

    /// codex review parallel reviewers per level
    #[arg(long, value_parser = parse_codex_review_n)]
    codex_review_n: Option<u32>,

    /// path to the codex binary (default `codex`)
    #[arg(long)]
    codex_review_bin: Option<PathBuf>,

    /// optional `inspect` subcommand; absent → loop mode
    #[command(subcommand)]
    sub: Option<SubCmd>,

    /// owner/repo (loop mode only)
    slug: Option<String>,

    /// PR number (loop mode only)
    pr: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
enum SubCmd {
    /// run one observe/orient/decide pass and print Outcome
    Inspect {
        /// owner/repo
        slug: String,
        /// PR number
        pr: String,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CeilingArg {
    Off,
    Low,
    Medium,
    High,
    Xhigh,
}

impl CeilingArg {
    fn into_level(self) -> Option<CodexReasoningLevel> {
        match self {
            Self::Off => None,
            Self::Low => Some(CodexReasoningLevel::Low),
            Self::Medium => Some(CodexReasoningLevel::Medium),
            Self::High => Some(CodexReasoningLevel::High),
            Self::Xhigh => Some(CodexReasoningLevel::Xhigh),
        }
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum FloorArg {
    Low,
    Medium,
    High,
    Xhigh,
}

impl FloorArg {
    fn into_level(self) -> CodexReasoningLevel {
        match self {
            Self::Low => CodexReasoningLevel::Low,
            Self::Medium => CodexReasoningLevel::Medium,
            Self::High => CodexReasoningLevel::High,
            Self::Xhigh => CodexReasoningLevel::Xhigh,
        }
    }
}

fn parse_max_iter_value(raw: &str) -> Result<std::num::NonZeroU32, String> {
    if raw.starts_with('-') {
        return Err(format!("--max-iter must be ≥ 1; got negative value: {raw}"));
    }
    if raw.starts_with('+') {
        return Err(format!("--max-iter: leading `+` not accepted: {raw}"));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| format!("--max-iter: not an integer: {raw}"))?;
    std::num::NonZeroU32::new(n).ok_or_else(|| "--max-iter must be ≥ 1; got 0".to_string())
}

fn parse_codex_review_n(raw: &str) -> Result<u32, String> {
    if raw.starts_with('-') {
        return Err(format!(
            "--codex-review-n must be ≥ 1; got negative value: {raw}"
        ));
    }
    if raw.starts_with('+') {
        return Err(format!("--codex-review-n: leading `+` not accepted: {raw}"));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| format!("--codex-review-n: not an integer: {raw}"))?;
    if n == 0 {
        return Err("--codex-review-n must be ≥ 1; got 0".to_string());
    }
    Ok(n)
}

fn parse_existing_state_root(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.exists() {
        return Err(format!("--state-root does not exist: {raw}"));
    }
    if !path.is_dir() {
        return Err(format!("--state-root is not a directory: {raw}"));
    }
    Ok(path)
}

fn usage(msg: &str) -> Outcome {
    Outcome::usage_error(msg)
}

/// Typed failures from [`resolve_repo_root`]. Every variant flattens
/// to a single-line `UsageError` diagnostic at the boundary.
///
/// The boundary deliberately stops short of remote-URL verification
/// against the supplied slug — `origin` vs `upstream`, HTTPS vs SSH,
/// fork remotes, and mirror clones all read as legitimate
/// configurations and a slug-mismatch heuristic would block them.
/// The user is trusted to invoke the binary from the right working
/// tree; this layer only guarantees the resolved path IS a working
/// tree (or an explicit override that canonicalizes).
#[derive(Debug)]
enum RepoRootError {
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    CwdUnavailable(std::io::Error),
    NotInGitTree {
        cwd: PathBuf,
        stderr: String,
    },
    GitSpawn(std::io::Error),
    /// `git rev-parse --show-toplevel` did not exit within the
    /// per-call deadline. The helper `SIGKILL`ed and reaped the
    /// child; surfacing the timeout as a distinct variant lets the
    /// boundary diagnostic name the deadline rather than collapse
    /// into a generic spawn failure.
    GitTimeout,
    /// `git rev-parse --show-toplevel` emitted more bytes on one
    /// pipe than the per-stream cap. The helper `SIGKILL`ed and
    /// reaped the child.
    GitOutputTooLarge {
        stream: &'static str,
        limit: usize,
    },
    /// `wait` / `try_wait` on the `git` subprocess reported an OS
    /// error.
    GitWait(std::io::Error),
    /// Reading the `git` subprocess's stdout / stderr pipe failed.
    GitPipe(std::io::Error),
}

impl std::fmt::Display for RepoRootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonicalize { path, source } => {
                write!(f, "--repo-root {}: {source}", path.display())
            }
            Self::CwdUnavailable(e) => write!(f, "current working directory unavailable: {e}"),
            Self::NotInGitTree { cwd, stderr } => {
                let stderr = stderr.replace('\n', " ");
                let suffix = if stderr.is_empty() {
                    String::new()
                } else {
                    format!(" ({stderr})")
                };
                write!(
                    f,
                    "{} is not inside a git working tree; invoke ooda-pr-codex-review from the target repo's checkout or pass --repo-root <PATH>{suffix}",
                    cwd.display(),
                )
            }
            Self::GitSpawn(e) => write!(
                f,
                "spawn `git rev-parse --show-toplevel`: {e}; install git or pass --repo-root <PATH>",
            ),
            Self::GitTimeout => write!(
                f,
                "`git rev-parse --show-toplevel` timed out after {}s",
                GIT_REV_PARSE_DEADLINE.as_secs()
            ),
            Self::GitOutputTooLarge { stream, limit } => write!(
                f,
                "`git rev-parse --show-toplevel` {stream} exceeded {limit}-byte cap",
            ),
            Self::GitWait(e) => {
                write!(f, "wait on `git rev-parse --show-toplevel` subprocess: {e}")
            }
            Self::GitPipe(e) => write!(f, "read `git rev-parse --show-toplevel` output pipe: {e}"),
        }
    }
}

/// Local git probe: `rev-parse --show-toplevel` does no I/O beyond
/// touching the working tree's `.git` directory and prints one
/// line. 10s is a generous cap that still surfaces a wedged git
/// instead of letting it pin the boundary.
const GIT_REV_PARSE_DEADLINE: Duration = Duration::from_secs(10);

/// Per-stream byte cap for local git probes. `rev-parse`,
/// `--show-toplevel`, and friends print a single line; 4 KiB
/// tolerates long worktree paths while keeping a runaway probe
/// bounded.
const GIT_LOCAL_MAX_BYTES: usize = 4 * 1024;

/// Build the standard per-call limits for local git probes.
fn git_local_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GIT_REV_PARSE_DEADLINE,
        max_stdout_bytes: GIT_LOCAL_MAX_BYTES,
        max_stderr_bytes: GIT_LOCAL_MAX_BYTES,
    }
}

/// Resolve the target working tree. Used by both the `gt`-pinning
/// threading and the codex subprocess `current_dir`.
fn resolve_repo_root(flag: Option<PathBuf>) -> Result<PathBuf, RepoRootError> {
    let cwd = std::env::current_dir().map_err(RepoRootError::CwdUnavailable)?;
    resolve_repo_root_with_cwd(flag, &cwd)
}

/// Test-facing variant of [`resolve_repo_root`] with the CWD injected
/// so cases (2) and (3) of the resolver's test matrix are reachable
/// without mutating process state.
fn resolve_repo_root_with_cwd(flag: Option<PathBuf>, cwd: &Path) -> Result<PathBuf, RepoRootError> {
    if let Some(p) = flag {
        return p
            .canonicalize()
            .map_err(|source| RepoRootError::Canonicalize { path: p, source });
    }
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd).args(["rev-parse", "--show-toplevel"]);
    let out = run_with_limits(&mut cmd, git_local_limits()).map_err(|e| match e {
        SpawnError::Spawn(io) => RepoRootError::GitSpawn(io),
        SpawnError::Timeout { .. } => RepoRootError::GitTimeout,
        SpawnError::OutputTooLarge { stream, limit, .. } => RepoRootError::GitOutputTooLarge {
            stream: stream.name(),
            limit,
        },
        SpawnError::Wait(io) => RepoRootError::GitWait(io),
        SpawnError::Read(io) => RepoRootError::GitPipe(io),
    })?;
    if !out.status.success() {
        return Err(RepoRootError::NotInGitTree {
            cwd: cwd.to_path_buf(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(RepoRootError::NotInGitTree {
            cwd: cwd.to_path_buf(),
            stderr: "`git rev-parse --show-toplevel` returned empty stdout".into(),
        });
    }
    Ok(PathBuf::from(s))
}

fn main() -> ProcessExitCode {
    // Install signal handlers before any loop work: a `SIGTERM`
    // arriving during args-parse should be picked up on the first
    // iteration boundary instead of killing the process uncleanly.
    // Failure to install is reported as a binary error rather than
    // silently dropping the graceful-shutdown contract.
    if let Err(e) = signal::install_signal_handlers() {
        return finish(
            &Outcome::binary_error(format!("install signal handlers: {e}")),
            None,
        );
    }
    let outcome = match parse_args() {
        Ok(args) => {
            // Resolve repo_root before opening the recorder so a
            // misconfigured invocation surfaces as `UsageError`
            // rather than booking a run that would mutate the
            // wrong working tree.
            let repo_root = match resolve_repo_root(args.repo_root.clone()) {
                Ok(p) => p,
                Err(e) => return finish(&usage(&e.to_string()), None),
            };
            let recorder = match Recorder::open(RecorderConfig {
                slug: args.slug.clone(),
                pr: args.pr,
                mode: run_mode(args.mode),
                max_iter: args.max_iter,
                status_comment: args.status_comment,
                state_root: args.state_root.clone(),
                legacy_trace: None,
            }) {
                Ok(r) => r,
                Err(e) => {
                    return finish(&Outcome::binary_error(format!("recorder: {e}")), None);
                }
            };
            recorder.install_process_recorder();
            let codex_review_snapshot =
                args.codex_review_ceiling
                    .map(|ceiling| CodexReviewSnapshot {
                        floor: args.codex_review_floor.as_str().to_string(),
                        ceiling: ceiling.as_str().to_string(),
                        n: args.codex_review_n,
                    });
            recorder.record_codex_review_config(codex_review_snapshot.as_ref());
            let outcome = match args.mode {
                Mode::Inspect => run_inspect(&args, &repo_root, &recorder),
                Mode::Loop => run_full(&args, &repo_root, &recorder),
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

fn finish(outcome: &Outcome, recorder: Option<Recorder>) -> ProcessExitCode {
    let code = outcome.exit_code();
    // `write_handoff_md` propagates the IterationHandoff append
    // failure rather than swallowing it; here we're already in the
    // terminal halt path, so the most useful response is to log the
    // failure to stderr (audit trail will be incomplete) and
    // continue with `None` so `render_outcome` still emits the
    // prompt inline.
    let handoff_path = match (outcome, recorder.as_ref()) {
        (Outcome::HandoffAgent(h), Some(r)) => r
            .write_handoff_md(
                &h.prompt.to_string(),
                ooda_state::OutcomeKind::HandoffAgent,
                ooda_core::ActionKindName::name(&h.kind),
            )
            .map_err(|e| {
                eprintln!("ooda-pr-codex-review: handoff audit-trail write failed: {e}");
            })
            .ok(),
        (Outcome::HandoffHuman(h), Some(r)) => r
            .write_handoff_md(
                &h.prompt.to_string(),
                ooda_state::OutcomeKind::HandoffHuman,
                ooda_core::ActionKindName::name(&h.kind),
            )
            .map_err(|e| {
                eprintln!("ooda-pr-codex-review: handoff audit-trail write failed: {e}");
            })
            .ok(),
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
    ProcessExitCode::from(code)
}

/// Post-observe sticky-head write site. Inspect and the iterated
/// loop both call this after a successful observe so the divergence
/// comparator's baseline tracks the most recent observed head.
/// Best-effort: a sticky write failure leaves the signal stale for
/// one iteration, never bricks the caller.
fn record_observed_head(sticky_path: &std::path::Path, obs: &observe::github::GitHubObservations) {
    let head = obs.pull_request_view.head_ref_oid.as_str();
    let _ = crate::observe::branch::write_sticky(sticky_path, head, false);
}

#[allow(clippy::too_many_lines)]
fn run_inspect(args: &Args, repo_root: &Path, recorder: &Recorder) -> Outcome {
    recorder.set_iteration(Some(1));
    recorder.record_observe_start(1);
    let sticky_path = match recorder.last_seen_head_path() {
        Ok(p) => p,
        Err(e) => {
            recorder.record_observe_end(1, ObserveOutcome::Error(e.to_string()));
            return Outcome::binary_error(format!("recorder: {e}"));
        }
    };
    let obs = match fetch_all(
        &args.slug,
        args.pr,
        args.state_root.as_deref(),
        Some(&sticky_path),
        repo_root,
    ) {
        Ok(FetchOutcome::Observations(o)) => {
            recorder.record_observe_end(1, ObserveOutcome::Ok);
            record_observed_head(&sticky_path, &o);
            *o
        }
        Ok(FetchOutcome::RateLimited(hit)) => {
            // Rate-limit shortcircuit: with no observations,
            // orient/decide are undefined. Inject a synthetic
            // wait-action and project through the same
            // `Outcome::from(Decision::Execute(_))` pipeline as any
            // ordinary iteration — invariant: inspect's exit-code
            // distribution is a subset of loop's.
            let line = format!(
                "rate-limited on {}; would wait {}s",
                hit.scope.name(),
                hit.retry_after.as_duration().as_secs(),
            );
            eprintln!("{line}");
            recorder.write_trace_line(&line);
            recorder.record_observe_end(
                1,
                ObserveOutcome::RateLimited {
                    scope: hit.scope.name().to_string(),
                    retry_after_secs: hit.retry_after.as_duration().as_secs(),
                },
            );
            let action = rate_limit_wait_action(hit);
            return Outcome::from(Decision::Execute(action));
        }
        Err(e) => {
            recorder.record_observe_end(1, ObserveOutcome::Error(e.to_string()));
            return Outcome::binary_error(format!("observe: {e}"));
        }
    };
    if obs.stack_root_branch != obs.pull_request_view.base_ref_name {
        let line = format!(
            "stack: {} → {}",
            obs.pull_request_view.base_ref_name, obs.stack_root_branch,
        );
        eprintln!("{line}");
        recorder.write_trace_line(&line);
    }
    // Inspect mode is observation-only: codex review state is
    // read from the filesystem, never spawned. The axis-enabled /
    // axis-disabled distinction collapses to "is there an artifact
    // to read?"; either way, inspect performs no mutation.
    let codex_obs =
        match maybe_fetch_codex(args, recorder, obs.pull_request_view.head_ref_oid.as_str()) {
            Ok(o) => o,
            Err(e) => return e,
        };
    let oriented = orient(&obs, codex_obs.as_ref(), None, current_timestamp());
    let candidate_actions = runner::drive(&oriented, args.pr);
    let decision = decide_from_candidates(candidate_actions.clone(), obs.pull_request_view.state);
    recorder.record_iteration(
        1,
        &obs,
        &recorder::RecorderInputs::from(&oriented),
        &candidate_actions,
        &decision,
    );
    if args.status_comment {
        let rendered = comment::render::render(
            &args.slug,
            args.pr,
            Some(1),
            &comment::render::RenderInputs::from(&oriented),
            &candidate_actions,
            &decision,
        );
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(&args.slug, args.pr, &rendered, recorder, Some(1));
        log_post_result("comment", true, r, Some(recorder));
    }
    let snapshot = HandoffSnapshot {
        ci: oriented.ci.clone(),
        reviews: oriented.reviews.clone(),
        closeout: oriented.closeout.clone(),
        closeout_attest_path: oriented.closeout_attest_path.clone(),
        head_short: obs
            .pull_request_view
            .head_ref_oid
            .as_str()
            .chars()
            .take(7)
            .collect(),
        base_branch: obs.pull_request_view.base_ref_name.to_string(),
        dashboard: Dashboard::from_iteration(
            &dashboard::DashboardInputs::from(&oriented),
            &candidate_actions,
            &decision,
        ),
    };
    decorate_handoff_human(
        Outcome::from(decision),
        &args.slug,
        args.pr,
        Some(&snapshot),
    )
}

fn run_full(args: &Args, repo_root: &Path, recorder: &Recorder) -> Outcome {
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
    let action_lock_path = match recorder.action_lock_path() {
        Ok(p) => p,
        Err(e) => return Outcome::binary_error(format!("recorder: {e}")),
    };
    let ctx = ActContext {
        slug: args.slug.clone(),
        pr: args.pr,
        action_lock_path,
        repo_root: repo_root.to_path_buf(),
        codex: codex_act,
    };
    let mut snapshot: Option<HandoffSnapshot> = None;
    let on_state = |i: u32,
                    obs: &observe::github::GitHubObservations,
                    oriented: &orient::OrientedState,
                    candidate_actions: &[decide::action::Action],
                    d: &Decision| {
        snapshot = Some(HandoffSnapshot {
            ci: oriented.ci.clone(),
            reviews: oriented.reviews.clone(),
            closeout: oriented.closeout.clone(),
            closeout_attest_path: oriented.closeout_attest_path.clone(),
            head_short: obs
                .pull_request_view
                .head_ref_oid
                .as_str()
                .chars()
                .take(7)
                .collect(),
            base_branch: obs.pull_request_view.base_ref_name.to_string(),
            dashboard: Dashboard::from_iteration(
                &dashboard::DashboardInputs::from(oriented),
                candidate_actions,
                d,
            ),
        });
        recorder.set_iteration(Some(i));
        recorder.record_iteration(
            i,
            obs,
            &recorder::RecorderInputs::from(oriented),
            candidate_actions,
            d,
        );
        let line = iteration_line(i, d);
        eprintln!("{line}");
        recorder.write_trace_line(&line);
        if args.status_comment {
            let rendered = comment::render::render(
                &args.slug,
                args.pr,
                Some(i),
                &comment::render::RenderInputs::from(oriented),
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
    let outcome = match run_loop(ctx, args.state_root.as_deref(), cfg, recorder, on_state) {
        Ok(LoopExit::Halted(reason)) => Outcome::from(reason),
        Ok(LoopExit::SignalInterrupted { exit_code }) => Outcome::SignalInterrupted { exit_code },
        Err(e) => Outcome::from(e),
    };
    decorate_handoff_human(outcome, &args.slug, args.pr, snapshot.as_ref())
}

/// Read codex-review batch state for inspect mode.
///
/// # Result discriminant
///
/// `Ok(None)` when the axis is disabled (ceiling unset) — the
/// caller treats observations-absent and axis-disabled
/// identically. `Ok(Some(_))` on a successful read; `Err(_)` on
/// a read failure that warrants surfacing as a binary error.
fn maybe_fetch_codex(
    args: &Args,
    recorder: &Recorder,
    head_sha: &str,
) -> Result<Option<observe::codex::CodexObservations>, Outcome> {
    let Some(ceiling) = args.codex_review_ceiling else {
        return Ok(None);
    };
    let codex_pr_root = recorder
        .pr_workspace_root()
        .map_err(|e| Outcome::binary_error(format!("recorder: {e}")))?
        .join("codex");
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

/// Construct the codex-review actuator context.
///
/// # Postcondition on `Ok(Some(_))`
///
/// - The codex PR-root directory exists.
/// - An advisory `flock` on `<codex_pr_root>/.lock` is held for
///   the context's lifetime, establishing the
///   single-active-invocation-per-PR invariant on shared batch
///   state.
/// - Per-iteration fields (`head_sha`, `base_branch`) hold
///   placeholders the runner refreshes per iteration.
///
/// The codex subprocess's working directory comes from
/// [`ActContext::repo_root`] (resolved by [`resolve_repo_root`] at
/// the binary entrypoint), not from this builder — the codex
/// sub-context no longer carries a separate copy.
///
/// # Postcondition on `Ok(None)`
///
/// Axis is disabled; no filesystem state was touched.
fn build_codex_act_context(
    args: &Args,
    recorder: &Recorder,
) -> Result<Option<CodexActContext>, Outcome> {
    if args.codex_review_ceiling.is_none() {
        return Ok(None);
    }
    let codex_pr_root = recorder
        .pr_workspace_root()
        .map_err(|e| Outcome::binary_error(format!("recorder: {e}")))?
        .join("codex");
    // 0o700 across the codex workspace tree — per-PR review logs,
    // exit codes, and sibling sidecars live below this root.
    if let Err(e) = ooda_core::atomic_io::secure_create_dir_all(&codex_pr_root) {
        return Err(Outcome::binary_error(format!(
            "create codex pr_root {}: {e}",
            codex_pr_root.display()
        )));
    }
    // Route through `FileLock::try_acquire` so the sidecar inherits
    // the secure-mode discipline from `ooda_core::file_lock` rather
    // than the shell's umask. Matches the `.batch.lock` →
    // `.batch.lock.lock` convention already in use at the inner
    // batch-dir guard (`observe::codex::batch`, `act::run`).
    let lock_path = codex_pr_root.join(".lock");
    let lock = match ooda_core::FileLock::try_acquire(&lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return Err(Outcome::binary_error(format!(
                "another invocation holds the codex review lock at {}; concurrent ooda-pr-codex-review runs against the same PR with codex enabled are not supported — wait for the prior run to exit, or use --state-root to isolate",
                lock_path.display()
            )));
        }
        Err(e) => {
            return Err(Outcome::binary_error(format!(
                "open codex .lock at {}: {e}",
                lock_path.display()
            )));
        }
    };
    Ok(Some(CodexActContext {
        codex_bin: args.codex_review_bin.clone(),
        codex_pr_root,
        n: args.codex_review_n,
        // Per-iteration fields use non-`Option` placeholders; the
        // runner refreshes them before each spawn. This sidesteps
        // threading `Option` through the spawn path while keeping
        // inspect mode (which never spawns) honest — it can ignore
        // these fields entirely.
        head_sha: String::new(),
        base_branch: String::new(),
        _lock: lock,
    }))
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
        // Single-line invariant on comment-event log lines:
        // `SingleLineString` flattens embedded newlines that the
        // upstream error type does not strip. Discharges the
        // one-line-per-comment-event contract at the type level.
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
            // `halt.name()` projects to a finite token set; `{:?}`
            // would expand the payload and violate the
            // one-line-bounded-length-per-iteration invariant.
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

/// Latest per-iteration context the post-loop handoff decorator
/// requires. Invariant: captured during the final `on_state`
/// callback, so decoration never re-observes — the loop's terminal
/// observations are reused verbatim.
///
/// `dashboard` carries the tier-grouped candidates / per-axis
/// signals / blockers projection derived from the same
/// `(oriented, candidates, decision)` triple the recorder
/// consumes. Constructed at the boundary so no shared mutable
/// state crosses the runner seam.
#[derive(Debug, Clone)]
struct HandoffSnapshot {
    ci: orient::ci::CiReport,
    reviews: orient::reviews::ReviewSummary,
    closeout: orient::closeout::Closeout,
    closeout_attest_path: Option<PathBuf>,
    head_short: String,
    base_branch: String,
    dashboard: Dashboard,
}

/// Decorate a handoff `Outcome` so the stderr hand-off is
/// self-contained: the recipient can act without re-querying the
/// forge.
///
/// # Decoration layers
///
/// Both layers append to the per-action body so the artifact reads
/// top-to-bottom as instructions-then-context: the per-action
/// headline + body explain *what to do*; the appended preamble
/// and context block answer *what state was observed and why this
/// halt fired*. Reordering this is a deliberate UX choice — a
/// reader scanning a long session lands on the action first.
///
/// - **Preamble (universal)**: appends a dashboard projection —
///   tier-grouped candidates, per-axis signals, blockers — to every
///   `HandoffHuman` and `HandoffAgent` outcome. Established by
///   `append_dashboard_preamble`.
/// - **Per-action context (gated)**: appends PR URL / branch / CI
///   summary / review summary to `HandoffHuman` outcomes and to
///   `HandoffAgent` outcomes whose kind passes
///   `agent_action_needs_context`. Established by
///   `push_handoff_context`.
///
/// # Invariants
///
/// - Non-handoff `Outcome` variants pass through unchanged.
/// - The handoff-agent gate is allowlist-shaped: new kinds opt in
///   by extension of `agent_action_needs_context`, not by editing
///   this decorator's match arms.
fn decorate_handoff_human(
    outcome: Outcome,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    snapshot: Option<&HandoffSnapshot>,
) -> Outcome {
    match outcome {
        Outcome::HandoffHuman(mut action) => {
            append_dashboard_preamble(&mut action, snapshot);
            push_handoff_context(&mut action, slug, pr, snapshot);
            Outcome::HandoffHuman(action)
        }
        Outcome::HandoffAgent(mut action) => {
            append_dashboard_preamble(&mut action, snapshot);
            if agent_action_needs_context(&action.kind) {
                push_handoff_context(&mut action, slug, pr, snapshot);
            }
            Outcome::HandoffAgent(action)
        }
        other => other,
    }
}

/// Append dashboard preamble sections onto the handoff prompt
/// (after the per-action body so the artifact reads
/// instructions-then-context).
///
/// Identity on either of two preconditions: snapshot absent
/// (synthetic handoff outside the iteration loop) or dashboard
/// projects no sections (terminal halts).
fn append_dashboard_preamble(
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
    handoff.prompt.sections.extend(preamble);
}

/// Allowlist predicate: which `HandoffAgent` kinds receive the
/// trailing per-action context block. Extension point — new kinds
/// opt in here; callers do not branch on `kind`.
fn agent_action_needs_context(kind: &decide::action::ActionKind) -> bool {
    matches!(kind, decide::action::ActionKind::Rebase)
}

/// Append boundary context lines (PR URL, blocker, branch, CI,
/// reviews) onto the handoff prompt.
///
/// Total over `HandoffAction`: `prompt` is a direct field, so the
/// structural projection in `classify()` discharges what would
/// otherwise be an `unreachable!()` arm over `ActionEffect`.
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
        let req = &snap.ci.summary.required;
        prompt.push_context_line(
            "CI",
            format!(
                "{} pass / {} failed / {} pending (required)",
                req.pass,
                req.fail(),
                req.pending()
            ),
        );
        let r = &snap.reviews;
        prompt.push_context_line(
            "Reviews",
            format!(
                "{} unresolved thread(s) / {} pending bot / {} pending human",
                r.threads_unresolved,
                r.pending_reviews.bots.len(),
                r.pending_reviews.humans.len()
            ),
        );
        push_closeout_context_line(prompt, &snap.closeout, snap.closeout_attest_path.as_deref());
    }
}

/// Append a closeout attestation line iff both: closeout axis is
/// `Synced` at current HEAD, AND the attestation file is readable.
///
/// Absence is a signal: closeout does not fire past convergence,
/// so an unattested handoff path implies the loop yielded before
/// reaching the gate.
fn push_closeout_context_line(
    prompt: &mut ooda_core::HandoffPrompt,
    closeout: &orient::closeout::Closeout,
    attest_path: Option<&Path>,
) {
    if !matches!(closeout, orient::closeout::Closeout::Synced) {
        return;
    }
    let Some(path) = attest_path else {
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

/// Render `Outcome` to a writer (typically stderr).
///
/// # Output contract
///
/// - **Header**: exactly one line per call, of the form
///   `<Variant>[: <suffix>]`.
///   Carries the bounded-token-set variant name plus a per-variant
///   single-line suffix.
/// - **Body** (handoff variants only): one pointer block written by
///   `write_handoff_block`, choosing path-form or inline form by
///   the `handoff_path` discriminant.
/// - **Trailer**: none, except `UsageError` which appends usage to
///   the same writer.
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
            let _ = writeln!(out, "Hand off to human: {}", handoff.prompt.headline);
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
            let _ = writeln!(out, "Hand off to agent: {}", handoff.prompt.headline);
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
        Outcome::SignalInterrupted { exit_code } => {
            let _ = writeln!(out, "Interrupted: exit code {exit_code}");
        }
    }
}

/// Write the handoff block in one of two shapes.
///
/// # Path form (`handoff_path = Some`)
///
/// Single line with leading sentinel `␣␣see:␣` followed by an
/// absolute path to a recorder-written file holding the prompt
/// body. **Invariant**: prompt size is bounded by the file's stat
/// — consumption is decoupled from the stderr stream's
/// truncation budget. Production path.
///
/// # Inline fallback (`handoff_path = None`)
///
/// Single line with leading sentinel `␣␣prompt:␣` followed by the
/// prompt body inline; continuation lines unprefixed. Used when
/// the recorder is unavailable (e.g. tests).
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

/// Project `ActionEffect` to a single-line tag suitable for the
/// `WouldAdvance` header. The Wait variant carries a duration
/// rendered in the smallest compound unit (s / m / m+s); payload
/// fields (log, prompt) are discarded — handoff-prompt rendering
/// is the responsibility of `write_handoff_block`.
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
    use ooda_core::MidTier;

    fn handoff(blocker: &str) -> ooda_core::HandoffAction<decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
            blocker: ids::BlockerKey::for_test(blocker),
        }
    }

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new(description),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: ids::BlockerKey::from_static("rebase-needed"),
        };
        Outcome::HandoffAgent(Box::new(handoff))
    }

    #[test]
    fn render_handoff_agent_fallback_inline_prompt() {
        let mut buf = Vec::new();
        render_outcome(&mut buf, &make_handoff_outcome("Rebase onto base"), None);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("Hand off to agent: Rebase onto base\n"));
        assert!(s.contains("\n  prompt: # Rebase onto base\n"));
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
        assert!(s.starts_with("Hand off to agent: Rebase onto base\n"));
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
            rendered.contains("**PR:** https://github.com/acme/widget/pull/42"),
            "decoration: {rendered}",
        );
        assert!(
            rendered.contains("**Blocker:** not_approved"),
            "decoration: {rendered}",
        );
        assert!(rendered.starts_with("# Request or self-approve"));
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
            rendered.contains("**PR:** https://github.com/acme/widget/pull/42"),
            "PR context missing from Rebase HandoffAgent: {rendered}",
        );
        assert!(
            rendered.contains("**Blocker:** behind_base"),
            "blocker context missing: {rendered}",
        );
        // Original prompt content preserved.
        assert!(rendered.starts_with("# Rebase onto the latest base branch"));
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
        assert!(!rendered.contains("**PR:** https://"));
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
                action_log: a.rendered_summary(),
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
        let o = stub_oriented();
        HandoffSnapshot {
            ci: o.ci,
            reviews: o.reviews,
            closeout: o.closeout,
            closeout_attest_path: o.closeout_attest_path,
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
                conversation_resolution_required: false,
                signatures_required: false,
                unsigned_commits: vec![],

                required_approving_review_count: None,
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
                requested_reviewers: crate::orient::reviews::RequestedReviewerSet::default(),
                latest_human_changes_requested: None,
            },
            copilot: None,
            cursor: None,
            threads: vec![],
            codex_review: None,
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
            branch_sync: crate::observe::branch::BranchSyncObservation {
                divergence: None,
                branch_graphite_tracked: false,
                gt_available: false,
            },
        }
    }

    fn rebase_action() -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Agent {
                prompt: ooda_core::HandoffPrompt::new("Rebase onto the latest base branch"),
            },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: ids::BlockerKey::from_static("behind_base"),
        }
    }

    #[test]
    fn decorate_handoff_human_prepends_dashboard_preamble() {
        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
            rendered.contains("## Recommended (blocking fix)"),
            "preamble: {rendered}",
        );
        assert!(rendered.contains("[blocker: behind_base]"), "{rendered}");
        assert!(rendered.contains("Blockers"), "{rendered}");
        // The existing per-action context block from 5bf9c7c still
        // lands at the trailing edge — preamble does not displace it.
        assert!(
            rendered.contains("**PR:** https://github.com/acme/widget/pull/42"),
            "trailing context: {rendered}",
        );
        assert!(rendered.contains("**Blocker:** not_approved"), "{rendered}");
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
            rendered.contains("## Recommended (blocking fix)"),
            "preamble missing: {rendered}",
        );
        assert!(
            rendered.contains("**PR:** https://github.com/acme/widget/pull/42"),
            "5bf9c7c context missing: {rendered}",
        );
        assert!(
            rendered.contains("**Blocker:** behind_base"),
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
            rendered.contains("## Recommended (blocking fix)"),
            "preamble must apply to non-allowlisted: {rendered}",
        );
        // Per-action context still gated — no PR / Blocker lines.
        assert!(!rendered.contains("**PR:** https://"), "{rendered}");
        assert!(!rendered.contains("**Blocker:** behind_base"), "{rendered}");
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingWait),
            blocker: ids::BlockerKey::from_static("waiting"),
        };
        let mut buf = Vec::new();
        render_outcome(&mut buf, &Outcome::WouldAdvance(Box::new(action)), None);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "WouldAdvance: Rebase:Wait(30s)\n"
        );
    }

    // ── repo_root resolver ────────────────────────────────────────

    #[test]
    fn resolve_repo_root_explicit_flag_canonicalizes() {
        let dir = tempfile::tempdir().unwrap();
        let indirect = dir.path().join(".");
        let resolved = resolve_repo_root_with_cwd(Some(indirect), Path::new("/")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_repo_root_explicit_flag_nonexistent_errors() {
        let bogus = std::env::temp_dir().join("ooda-pr-codex-review-resolve-nonexistent-XYZZY");
        let _ = std::fs::remove_dir_all(&bogus);
        let err = resolve_repo_root_with_cwd(Some(bogus.clone()), Path::new("/")).unwrap_err();
        match err {
            RepoRootError::Canonicalize { path, .. } => assert_eq!(path, bogus),
            other => panic!("expected Canonicalize, got {other:?}"),
        }
    }

    #[test]
    fn resolve_repo_root_cwd_in_git_tree_returns_toplevel() {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--quiet"])
            .output()
            .expect("spawn git init");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let resolved = resolve_repo_root_with_cwd(None, dir.path()).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap(),
        );
    }

    #[test]
    fn resolve_repo_root_cwd_outside_git_tree_errors() {
        // Defensive premise check: if the test environment's
        // `temp_dir()` happens to BE inside a git tree (some CI
        // sandboxes, dev-loop setups where a `.git` lingers under
        // TMPDIR), the assertion would misfire. Skip cleanly
        // rather than panic — the other resolver tests cover the
        // success paths.
        let dir = tempfile::tempdir().unwrap();
        let probe = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "--show-toplevel"])
            .output();
        if let Ok(out) = probe.as_ref()
            && out.status.success()
        {
            eprintln!(
                "skipping: tempdir {} is unexpectedly inside a git tree (env quirk)",
                dir.path().display(),
            );
            return;
        }
        let result = resolve_repo_root_with_cwd(None, dir.path());
        match result {
            Err(RepoRootError::NotInGitTree { .. } | RepoRootError::GitSpawn(_)) => {}
            other => panic!("expected NotInGitTree or GitSpawn, got {other:?}"),
        }
    }
}
