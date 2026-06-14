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

use dashboard::Dashboard;
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::decision::{Decision, DecisionHalt};
use ids::{PullRequestNumber, RepoSlug};
use observe::github::{FetchOutcome, fetch_all};
use ooda_core::{SpawnError, SpawnLimits, decide_from_candidates, run_with_limits};
use ooda_state::ObserveOutcome;
use orient::orient;
use outcome::Outcome;
use recorder::{Recorder, RecorderConfig, RunMode};
use runner::{LoopConfig, LoopExit, current_timestamp, run_loop};

fn print_usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "ooda-pr — drive a PR through observe → orient → decide → act until halt.\n\
         \n\
         Usage:\n  ooda-pr [options] <owner/repo> <pr>           run the loop (default)\n  ooda-pr inspect [options] <owner/repo> <pr>   one pass; print Outcome; exit\n\
         \n\
         Options:\n  --max-iter N        loop iteration cap (default 50, must be ≥ 1; ignored by inspect)\n  --status-comment    post a status comment on the PR each iteration (deduped)\n  --state-root PATH   write always-on harness state under PATH\n  --repo-root PATH    target working tree for all `gt`/`git` invocations\n                      (default: derive from CWD via `git rev-parse --show-toplevel`)\n  --trace PATH        also append the compact trace to PATH\n  -h, --help          show this help and exit\n\
         \n\
         Exit codes (stderr header — see SKILL.md for variant mapping):\n   0 DoneMerged       1 Paused             2 WouldAdvance      3 HandoffHuman\n   4 HandoffAgent     5 DoneClosed         6 StuckRepeated     7 StuckCapReached\n  64 UsageError      70 BinaryError      130 SignalInterrupted (SIGINT)\n 143 SignalInterrupted (SIGTERM)"
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
    /// CWD pin for every `gt` subprocess (sync, log-stack, version);
    /// without it, an invocation from a sibling repo can mutate the
    /// wrong stack.
    repo_root: Option<PathBuf>,
    trace: Option<PathBuf>,
}

/// Parse CLI args into `Args` or a synthetic `Outcome::UsageError`.
///
/// # Invariants
///
/// - **Totality over argv**: every reachable input yields either
///   `Ok(Args)` or `Err(Outcome::UsageError(_))`; no panic, no
///   exception path. Backed by `clap::Parser::try_get_matches_from(env::args_os())`
///   — non-UTF-8 argv is preserved as `OsString` through the parse
///   path (closes bug 3 from the F9 migration).
/// - **Help to stdout, exit 0**: `-h`/`--help` anywhere in argv
///   triggers clap's built-in help renderer and `exit 0`. clap's
///   help handling subsumes the prior pre-scan; no manual pre-scan
///   is required.
/// - **`--flag=value` form** is accepted natively by clap (closes
///   bug 1).
/// - **Flag-shaped values rejected**: a value position like
///   `--max-iter --advance-level` errors instead of swallowing the
///   next flag as a value (closes bug 2; clap rejects values that
///   start with `-`).
/// - **`--state-root` existence check**: a path that does not exist
///   is rejected at parse time via [`parse_existing_state_root`]
///   (closes bug 6 — converges on `ooda-attest`'s behavior).
fn parse_args() -> Result<Args, Outcome> {
    use clap::Parser;
    // Pre-scan for `-h`/`--help` so it short-circuits ANY parse
    // failure (the prior contract; preserved on migration to
    // clap). Without this, a malformed earlier flag would error
    // before clap reached the help arg.
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
            // Help / version: clap writes to stdout and we exit 0
            // to preserve the prior "--help → exit 0" contract.
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                // clap renders the binary's name + version banner;
                // the prior contract was a custom usage block. Use
                // `print_usage` to keep stdout output byte-stable
                // against the prior contract.
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

    Ok(Args {
        mode,
        slug,
        pr,
        max_iter: raw.max_iter,
        status_comment: raw.status_comment,
        state_root: raw.state_root,
        repo_root: raw.repo_root,
        trace: raw.trace,
    })
}

/// Render a clap error as a single-line `UsageError` payload.
///
/// Strips clap's multi-line block (which embeds usage + tip text)
/// down to the first `error:` line so the `Outcome::UsageError`
/// `SingleLineString` invariant holds.
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
/// [`parse_args`]. Order matters: `#[command(subcommand)]` field is
/// resolved before bare positionals, so `inspect <slug> <pr>` and
/// `<slug> <pr>` parse into distinct shapes.
#[derive(clap::Parser, Debug)]
#[command(
    name = "ooda-pr",
    about = "drive a PR through observe → orient → decide → act until halt",
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

    /// target working tree for all `gt`/`git` invocations
    #[arg(long)]
    repo_root: Option<PathBuf>,

    /// also append the compact trace to PATH
    #[arg(long)]
    trace: Option<PathBuf>,

    /// optional `inspect` subcommand; absent → loop mode with root positionals
    #[command(subcommand)]
    sub: Option<SubCmd>,

    /// owner/repo (loop mode only; absent under `inspect`)
    slug: Option<String>,

    /// PR number (loop mode only; absent under `inspect`)
    pr: Option<String>,
}

/// Subcommand surface. Only `inspect` exists today; the variant
/// is an explicit subcommand so its positionals don't collide with
/// the root-level loop positionals.
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

/// `--max-iter` value parser preserving prior diagnostic substrings
/// (`"must be ≥ 1"`, `"not an integer"`, `"got negative value"`).
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

/// `--state-root` value parser. Rejects paths that do not exist on
/// disk — converges on `ooda-attest`'s prior validation surface
/// (F9 bug 6). The further canonicalization happens inside
/// [`Recorder::open`]; this parser keeps the boundary check at
/// argv-parse time so a typo on `--state-root` surfaces as
/// `UsageError(64)` not `BinaryError(70)`.
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
    // `Outcome::usage_error` wraps `msg` in `SingleLineString`,
    // structurally enforcing the single-line-header invariant on
    // every UsageError diagnostic.
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
    /// `--repo-root <PATH>` was supplied but the path could not be
    /// canonicalized (typically: does not exist, or no read
    /// permission on a path component).
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `std::env::current_dir()` failed before CWD-derivation could
    /// proceed. Rare — usually a deleted-CWD or permission edge
    /// case.
    CwdUnavailable(std::io::Error),
    /// `git rev-parse --show-toplevel` exited non-zero inside the
    /// resolved CWD: the directory is not part of a git working
    /// tree. Carries the CWD and `git`'s own stderr so the operator
    /// can distinguish "not a repo" from "permission denied".
    NotInGitTree { cwd: PathBuf, stderr: String },
    /// The `git` subprocess could not be spawned at all (typically:
    /// `git` is not on `$PATH`).
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
    GitOutputTooLarge { stream: &'static str, limit: usize },
    /// `wait` or `try_wait` on the `git` subprocess reported an OS
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
                    "{} is not inside a git working tree; invoke ooda-pr from the target repo's checkout or pass --repo-root <PATH>{suffix}",
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

/// Resolve the target working tree.
///
/// Policy:
///   1. `Some(path)` → canonicalize (lifts symlinks + relative paths
///      to absolute; fails if the path does not exist).
///   2. `None` → derive from `std::env::current_dir()` via
///      `git -C <cwd> rev-parse --show-toplevel`.
///
/// Deliberately does NOT verify the resolved path matches the
/// supplied `--slug` argument's remote: remote-URL comparison is
/// brittle (origin / upstream / HTTPS / SSH / fork remotes), and
/// false rejections would break valid setups. See [`RepoRootError`].
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
                legacy_trace: args.trace.clone(),
            }) {
                Ok(r) => r,
                Err(e) => {
                    return finish(&Outcome::binary_error(format!("recorder: {e}")), None);
                }
            };
            recorder.install_process_recorder();
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
                eprintln!("ooda-pr: handoff audit-trail write failed: {e}");
            })
            .ok(),
        (Outcome::HandoffHuman(h), Some(r)) => r
            .write_handoff_md(
                &h.prompt.to_string(),
                ooda_state::OutcomeKind::HandoffHuman,
                ooda_core::ActionKindName::name(&h.kind),
            )
            .map_err(|e| {
                eprintln!("ooda-pr: handoff audit-trail write failed: {e}");
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
            // Post-observe sticky update — same write site as
            // the main loop. Inspect is single-iteration, but
            // recording the SHA we read keeps subsequent loops
            // (run by other tools) consistent with the
            // divergence comparator.
            let _ = crate::observe::branch::write_sticky(
                &sticky_path,
                o.pull_request_view.head_ref_oid.as_str(),
                false,
            );
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
            let run_id = recorder.run_id();
            eprintln!("{} {line}", loop_prefix(&args.slug, args.pr, Some(&run_id)));
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
        // Stack discrepancy: the PR's immediate base differs from
        // the stack root branch-rule lookups are keyed on. Emit
        // exactly `stack: <base> → <root>` so downstream parsers
        // can match a fixed grammar.
        let line = format!(
            "stack: {} → {}",
            obs.pull_request_view.base_ref_name, obs.stack_root_branch,
        );
        let run_id = recorder.run_id();
        eprintln!("{} {line}", loop_prefix(&args.slug, args.pr, Some(&run_id)));
        recorder.write_trace_line(&line);
    }
    let oriented = orient(&obs, None, current_timestamp());
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
        log_post_result(&args.slug, args.pr, "comment", true, r, Some(recorder));
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
        let run_id = recorder.run_id();
        eprintln!("{} {line}", loop_prefix(&args.slug, args.pr, Some(&run_id)));
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
            log_post_result(
                &args.slug,
                args.pr,
                &format!("[iter {i}] comment"),
                false,
                r,
                Some(recorder),
            );
        }
    };
    let outcome = match run_loop(
        &args.slug,
        args.pr,
        args.state_root.as_deref(),
        repo_root,
        cfg,
        recorder,
        on_state,
    ) {
        Ok(LoopExit::Halted(reason)) => Outcome::from(reason),
        Ok(LoopExit::SignalInterrupted { exit_code }) => Outcome::SignalInterrupted { exit_code },
        Err(e) => Outcome::from(e),
    };
    decorate_handoff_human(outcome, &args.slug, args.pr, snapshot.as_ref())
}

/// Loop-identity prefix for advisory stderr lines.
///
/// With ≥2 concurrent OODA invocations live (the common case in
/// this user's workflow), a bare `eprintln!("{line}")` cannot be
/// attributed to a specific loop. The prefix carries
/// binary+slug+PR+run-id so a stderr grep disambiguates every
/// warning line. Run-id is omitted (rather than printed empty)
/// when the recorder is not yet open or the mutex is poisoned.
fn loop_prefix(slug: &RepoSlug, pr: PullRequestNumber, run_id: Option<&str>) -> String {
    match run_id {
        Some(rid) if !rid.is_empty() => format!("[ooda-pr {slug}#{pr} run={rid}]"),
        _ => format!("[ooda-pr {slug}#{pr}]"),
    }
}

fn log_post_result(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    prefix: &str,
    verbose_skip: bool,
    r: Result<bool, comment::post::PostError>,
    recorder: Option<&Recorder>,
) {
    let line = post_result_line(prefix, verbose_skip, r);
    if let Some(line) = line {
        let run_id = recorder.map(Recorder::run_id);
        eprintln!("{} {line}", loop_prefix(slug, pr, run_id.as_deref()));
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

    fn action(blocker: &str) -> decide::action::Action {
        decide::action::Action {
            kind: decide::action::ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: ids::BlockerKey::for_test(blocker),
        }
    }

    fn handoff(blocker: &str) -> ooda_core::HandoffAction<decide::action::ActionKind> {
        ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
    fn loop_prefix_carries_binary_slug_pr_and_run_id() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        assert_eq!(
            loop_prefix(&slug, pr, Some("abc123")),
            "[ooda-pr acme/widget#42 run=abc123]"
        );
    }

    #[test]
    fn loop_prefix_omits_run_id_when_absent() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        assert_eq!(loop_prefix(&slug, pr, None), "[ooda-pr acme/widget#42]");
        // Empty run-id collapses to the same shape as None: keeps
        // the prefix grammar regular so a stderr grep can pivot on
        // the presence of `run=`.
        assert_eq!(loop_prefix(&slug, pr, Some("")), "[ooda-pr acme/widget#42]");
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
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
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
        // Original prompt content is preserved.
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
    fn decorate_handoff_human_surfaces_closeout_attestation_line_when_synced() {
        use ooda_core::attest::write_closeout_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("42").join("closeout_attest.json");
        let sha = "0123456789abcdef0123456789abcdef01234567";
        write_closeout_atomic(&path, sha.to_string()).unwrap();

        let mut snap = snapshot_with_dashboard(&[rebase_action()]);
        snap.closeout = orient::closeout::Closeout::Synced;
        snap.closeout_attest_path = Some(path);

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
            rendered.contains("**Closeout:** attested at "),
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
        snap.closeout = orient::closeout::Closeout::NeverAttested;
        snap.closeout_attest_path = None;

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
        snap.closeout = orient::closeout::Closeout::Synced;
        snap.closeout_attest_path = None;

        let handoff = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
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
        // Pass the path through a `./` indirection to prove
        // canonicalize is doing the work. `/some/path/./` resolves
        // to `/some/path` only after canonicalize touches it.
        let indirect = dir.path().join(".");
        let resolved = resolve_repo_root_with_cwd(
            Some(indirect),
            // CWD irrelevant when the flag is supplied; pass `/`
            // to prove the flag branch never falls back to git.
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_repo_root_explicit_flag_nonexistent_errors() {
        let bogus = std::env::temp_dir().join("ooda-pr-resolve-nonexistent-XYZZY");
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
        // `git rev-parse --show-toplevel` returns the canonicalized
        // working tree, so compare against the canonicalized
        // tempdir.
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap(),
        );
    }

    #[test]
    fn resolve_repo_root_cwd_outside_git_tree_errors() {
        // tempdir under the system temp_dir is not normally inside
        // a git working tree; `git rev-parse` should exit non-zero.
        // `GitSpawn` is also acceptable: when the test env lacks
        // `git`, the resolver still surfaces a typed error rather
        // than panicking.
        //
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
