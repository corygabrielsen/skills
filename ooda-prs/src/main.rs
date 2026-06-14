use std::path::{Path, PathBuf};
// Aliased to avoid collision with `ooda_core::ExitCode` (used via
// the `MultiOutcome::exit_code()` projection below). The two types
// are distinct: `ExitCode` is the typed family-wide enum;
// `ProcessExitCode` is the OS-facing `std::process::ExitCode` that
// `main` returns.
use std::process::ExitCode as ProcessExitCode;
use std::time::Duration;

mod act;
mod axis_impls;
mod comment;
mod dashboard;
mod decide;
mod ids;
mod multi_outcome;
mod observe;
mod orient;
mod outcome;
mod recorder;
mod runner;
mod signal;
mod suite;
mod text;

use dashboard::Dashboard;
use decide::action::{ActionEffect, rate_limit_wait_action};
use decide::decision::{Decision, DecisionHalt};
use ids::{PullRequestNumber, RepoSlug};
use multi_outcome::{MultiOutcome, ProcessOutcome};
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
        "ooda-prs — drive N PRs through observe → orient → decide → act until each halts.\n\
         \n\
         Usage:\n  ooda-prs [options] <suite>            run the loop on every PR in <suite>\n  ooda-prs inspect [options] <suite>    one pass per PR; print MultiOutcome; exit\n\
         \n\
         Suite grammar:\n  <suite>      ::= <group> ( ',' <group> )*\n  <group>      ::= <owner/repo>? <pr>+\n  <owner/repo>  — explicit slug for this group; if omitted, inherit from the prior\n                  group, else infer from cwd (`gh repo view --json nameWithOwner`).\n  <pr>          — positive integer.\n  Examples:\n    ooda-prs 42 45                              # cwd-slug, two PRs\n    ooda-prs acme/widget 42 43, acme/infra 100  # multi-slug; comma separates groups\n    ooda-prs acme/widget 42, 43                 # group 2 inherits acme/widget\n\
         \n\
         Options:\n  --max-iter N         loop iteration cap per PR (default 50, must be ≥ 1; ignored by inspect)\n  --concurrency K      max in-flight PRs (default = |suite|, must be ≥ 1)\n  --status-comment     post a status comment on each PR every iteration (deduped)\n  --state-root PATH    write always-on harness state under PATH\n  --repo-root PATH     target working tree for all `gt`/`git` invocations\n                       (default: derive from CWD via `git rev-parse --show-toplevel`)\n  -h, --help           show this help and exit\n\
         \n\
         Exit codes — aggregate priority projection over per-PR Outcomes:\n   0 all DoneMerged/Paused (no further action)\n   1 (unused at suite level — Paused folds into 0)\n   2 any WouldAdvance\n   3 any HandoffHuman\n   4 any HandoffAgent\n   5 any DoneClosed (closed without merge — distinct from merged)\n   6 any StuckRepeated\n   7 any StuckCapReached\n  64 UsageError\n  70 any BinaryError\n  (130 SIGINT, 143 SIGTERM reserved)\n\
         Priority order (highest first): UsageError > BinaryError > HandoffAgent > HandoffHuman > StuckCapReached > StuckRepeated > WouldAdvance > DoneClosed > DoneMerged/Paused."
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loop,
    Inspect,
}

struct Args {
    mode: Mode,
    /// The parsed suite: a non-empty, deduplicated `Vec<(slug, pr)>`
    /// in input order. Each pair is driven by its own `run_loop`
    /// (sequentially in this stage; parallelized later).
    suite: Vec<(RepoSlug, PullRequestNumber)>,
    max_iter: std::num::NonZeroU32,
    status_comment: bool,
    state_root: Option<PathBuf>,
    /// Optional explicit override for the target working tree. When
    /// `None`, [`resolve_repo_root`] derives it from the process CWD
    /// via `git rev-parse --show-toplevel`. The resolved path is the
    /// CWD pin for every `gt` subprocess (sync, log-stack, version);
    /// without it, an invocation from a sibling repo can mutate the
    /// wrong stack. A single path covers the whole suite — `ooda-prs`
    /// drives many PRs but only one local working tree at a time.
    repo_root: Option<PathBuf>,
    /// Optional cap on concurrent in-flight PRs. `None` resolves
    /// to `|suite|` at the spawn loop (no cap). Enforced by
    /// `suite::drive_suite` via an `AtomicUsize` work index.
    concurrency: Option<u32>,
}

/// Parse CLI args into `Args` or a `SingleLineString` diagnostic.
///
/// # Invariants
///
/// - **Totality over argv**: every reachable input yields either
///   `Ok(Args)` or `Err(SingleLineString)`. Backed by clap's
///   `try_get_matches_from(env::args_os())` — non-UTF-8 argv flows
///   through `OsString` (closes F9 bug 3).
/// - **`--help` short-circuits**: pre-scan + clap built-in both
///   route to `print_usage` + `exit 0`, even past a malformed flag.
/// - **`--flag=value`** form accepted natively (closes F9 bug 1).
/// - **Flag-shaped values rejected** (closes F9 bug 2).
/// - **`--state-root` existence check** at parse time (closes F9
///   bug 6).
fn parse_args() -> Result<Args, ooda_core::SingleLineString> {
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

    let (mode, positional_strs) = match raw.sub {
        Some(SubCmd::Inspect { positionals }) => (Mode::Inspect, positionals),
        None => (Mode::Loop, raw.positionals),
    };
    let suite = parse_suite(&positional_strs)?;

    Ok(Args {
        mode,
        suite,
        max_iter: raw.max_iter,
        status_comment: raw.status_comment,
        state_root: raw.state_root,
        repo_root: raw.repo_root,
        concurrency: raw.concurrency,
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
/// [`parse_args`].
#[derive(clap::Parser, Debug)]
#[command(
    name = "ooda-prs",
    about = "drive N PRs through observe → orient → decide → act in parallel",
    disable_help_flag = false,
    disable_version_flag = true,
    arg_required_else_help = false
)]
struct CliRaw {
    /// loop iteration cap per PR (must be ≥ 1; ignored by inspect)
    #[arg(long, value_parser = parse_max_iter_value, default_value_t = std::num::NonZeroU32::new(50).expect("50 is non-zero"))]
    max_iter: std::num::NonZeroU32,

    /// max in-flight PRs (must be ≥ 1; default = |suite|)
    #[arg(long, value_parser = parse_concurrency_value)]
    concurrency: Option<u32>,

    /// post a status comment on each PR every iteration (deduped)
    #[arg(long)]
    status_comment: bool,

    /// always-on harness state root
    #[arg(long, value_parser = parse_existing_state_root)]
    state_root: Option<PathBuf>,

    /// target working tree for all `gt`/`git` invocations
    #[arg(long)]
    repo_root: Option<PathBuf>,

    /// optional `inspect` subcommand
    #[command(subcommand)]
    sub: Option<SubCmd>,

    /// suite positionals (loop mode)
    #[arg(trailing_var_arg = true, allow_hyphen_values = false)]
    positionals: Vec<String>,
}

#[derive(clap::Subcommand, Debug)]
enum SubCmd {
    /// run one observe/orient/decide pass per PR
    Inspect {
        #[arg(trailing_var_arg = true, allow_hyphen_values = false)]
        positionals: Vec<String>,
    },
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

fn parse_concurrency_value(raw: &str) -> Result<u32, String> {
    if raw.starts_with('-') {
        return Err(format!(
            "--concurrency must be ≥ 1; got negative value: {raw}"
        ));
    }
    if raw.starts_with('+') {
        return Err(format!("--concurrency: leading `+` not accepted: {raw}"));
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| format!("--concurrency: not an integer: {raw}"))?;
    if n == 0 {
        return Err("--concurrency must be ≥ 1; got 0".to_string());
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

/// Parse positional tokens into a non-empty, deduplicated suite.
///
/// # Grammar
///
/// ```text
/// suite ::= group ( ',' group )*
/// group ::= slug? pr+
/// slug  ::= token containing '/'
/// pr    ::= token without '/' (parsed as positive integer)
/// ```
///
/// # Slug resolution
///
/// Each group's slug is the first defined of: its own token, the
/// previously resolved group's slug, or the cwd repository
/// inferred via `gh repo view`. Implicit-slug inheritance is
/// left-to-right within one invocation.
///
/// # Invariant
///
/// Total over argv: every error path maps to a single-line
/// diagnostic — no parser path can panic.
fn parse_suite(
    positional: &[String],
) -> Result<Vec<(RepoSlug, PullRequestNumber)>, ooda_core::SingleLineString> {
    if positional.is_empty() {
        return Err(usage(
            "no PRs specified; expected <owner/repo>? <pr>+ (',' <owner/repo>? <pr>+)*",
        ));
    }

    // Comma-separation is normalized in two phases: join argv on
    // spaces, then split on ','. This collapses every surface form
    // — `a, b`, `a ,b`, `a,b`, and shell-split `a,` then `b` —
    // onto one grammar, so downstream tokenization sees a single
    // canonical shape.
    let joined: String = positional.join(" ");
    let group_strs: Vec<&str> = joined.split(',').map(str::trim).collect();

    let mut suite: Vec<(RepoSlug, PullRequestNumber)> = Vec::new();
    let mut last_slug: Option<RepoSlug> = None;

    for (idx, group_str) in group_strs.iter().enumerate() {
        if group_str.is_empty() {
            return Err(usage(&format!(
                "empty group at position {} (commas must separate non-empty groups)",
                idx + 1
            )));
        }
        let tokens: Vec<&str> = group_str.split_whitespace().collect();
        // The empty-after-trim guard above discharges the
        // non-empty-tokens precondition for the indexing below.

        let (slug, pr_tokens) = if tokens[0].contains('/') {
            let slug = RepoSlug::parse(tokens[0]).map_err(|e| usage(&e.to_string()))?;
            (slug, &tokens[1..])
        } else {
            let slug = match &last_slug {
                Some(s) => s.clone(),
                None => infer_cwd_slug().map_err(|e| usage(&e))?,
            };
            (slug, &tokens[..])
        };

        if pr_tokens.is_empty() {
            return Err(usage(&format!(
                "group {} has slug {slug} but no PR numbers",
                idx + 1
            )));
        }
        for pr_token in pr_tokens {
            let pr = PullRequestNumber::parse(pr_token).map_err(|e| usage(&e.to_string()))?;
            if suite.iter().any(|(s, p)| s == &slug && *p == pr) {
                return Err(usage(&format!("duplicate PR: {slug}#{pr}")));
            }
            suite.push((slug.clone(), pr));
        }
        last_slug = Some(slug);
    }

    Ok(suite)
}

/// Infer the cwd's repository slug from the forge CLI.
///
/// Fallback path for the first suite group when no explicit slug
/// is supplied. Every failure mode (CLI absent, cwd not a repo,
/// non-UTF-8 stdout, malformed slug) flattens to a single-line
/// diagnostic, preserving the `UsageError` newline-free invariant.
fn infer_cwd_slug() -> Result<RepoSlug, String> {
    let mut cmd = std::process::Command::new("gh");
    cmd.args([
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ]);
    let out = run_with_limits(&mut cmd, gh_repo_view_limits()).map_err(|e| match e {
        SpawnError::Spawn(io) => format!("cwd slug inference: spawn `gh` failed: {io}"),
        SpawnError::Timeout { deadline, .. } => format!(
            "cwd slug inference: `gh repo view` timed out after {}s",
            deadline.as_secs()
        ),
        SpawnError::OutputTooLarge { stream, limit, .. } => {
            format!("cwd slug inference: `gh repo view` {stream} exceeded {limit}-byte cap")
        }
        SpawnError::Read(io) => format!("cwd slug inference: read `gh` output pipe: {io}"),
        SpawnError::Wait(io) => format!("cwd slug inference: wait on `gh` subprocess: {io}"),
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out
            .status
            .code()
            .map_or_else(|| "?".into(), |c| c.to_string());
        return Err(format!(
            "cwd is not a github repo; specify <owner/repo> explicitly (gh exit {code}: {})",
            stderr.trim().replace('\n', " ")
        ));
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| format!("cwd slug inference: stdout not UTF-8: {e}"))?;
    let trimmed = stdout.trim();
    RepoSlug::parse(trimmed).map_err(|e| format!("cwd slug parse from gh stdout {trimmed:?}: {e}"))
}

/// Construct a parser-stage diagnostic at the parser's natural
/// type. Invariant: `parse_args` returns the bare diagnostic, not
/// an `Outcome`, so the call site can lift one value into both
/// `Outcome::UsageError` (stderr framing) and
/// `MultiOutcome::UsageError` (exit-code framing) by direct
/// construction — eliminating the runtime narrowing match that
/// would otherwise carry an `unreachable!()` arm.
fn usage(msg: &str) -> ooda_core::SingleLineString {
    msg.into()
}

/// Typed failures from [`resolve_repo_root`]. Every variant flattens
/// to a single-line `UsageError` diagnostic at the boundary.
///
/// The boundary deliberately stops short of remote-URL verification
/// against the suite's slugs — `origin` vs `upstream`, HTTPS vs SSH,
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
                    "{} is not inside a git working tree; invoke ooda-prs from the target repo's checkout or pass --repo-root <PATH>{suffix}",
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

/// Per-call deadline for `gh repo view` during cwd-slug inference.
/// 30s tolerates the single-call network round-trip while keeping
/// a wedged subprocess from stalling the boundary indefinitely.
const GH_REPO_VIEW_DEADLINE: Duration = Duration::from_secs(30);

/// Per-stream byte cap for `gh repo view` during cwd-slug inference.
/// Output is a single-line slug; 256 KiB tolerates noisy diagnostic
/// preamble on stderr while keeping a malicious / wedged subprocess
/// from growing memory unbounded.
const GH_REPO_VIEW_MAX_BYTES: usize = 256 * 1024;

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

/// Build the standard per-call limits for `gh repo view`.
fn gh_repo_view_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GH_REPO_VIEW_DEADLINE,
        max_stdout_bytes: GH_REPO_VIEW_MAX_BYTES,
        max_stderr_bytes: GH_REPO_VIEW_MAX_BYTES,
    }
}

/// Build the standard per-call limits for local git probes.
fn git_local_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GIT_REV_PARSE_DEADLINE,
        max_stdout_bytes: GIT_LOCAL_MAX_BYTES,
        max_stderr_bytes: GIT_LOCAL_MAX_BYTES,
    }
}

/// Resolve the target working tree. See the per-binary policy in
/// [`Args::repo_root`]: a single path covers the whole suite.
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
    // Install signal handlers before any worker spawns: every
    // per-PR loop polls the same atomic, so a `SIGTERM` lands on
    // each worker's next iteration boundary uniformly. Failure to
    // install is reported as a UsageError-grade bundle error
    // rather than silently dropping the graceful-shutdown
    // contract.
    if let Err(e) = signal::install_signal_handlers() {
        let outcome: Outcome = Outcome::binary_error(format!("install signal handlers: {e}"));
        render_outcome(&mut std::io::stderr(), &outcome, None);
        return ProcessExitCode::from(outcome.exit_code());
    }
    let multi = match parse_args() {
        Ok(args) => {
            // Resolve repo_root before fanning out workers so a
            // misconfigured invocation surfaces as `UsageError`
            // exactly once at the boundary instead of once per
            // worker.
            let repo_root = match resolve_repo_root(args.repo_root.clone()) {
                Ok(p) => p,
                Err(e) => {
                    let msg = usage(&e.to_string());
                    let outcome: Outcome = Outcome::UsageError(msg.clone());
                    render_outcome(&mut std::io::stderr(), &outcome, None);
                    return ProcessExitCode::from(MultiOutcome::UsageError(msg).exit_code());
                }
            };
            // Parallel per-PR dispatch under `thread::scope`. Each
            // worker drives one PR through the full pipeline; the
            // aggregate exit code is the typed priority projection
            // on `MultiOutcome`.
            //
            // # Cross-thread isolation invariants
            //
            // - **Tool-call sink**: thread-local, so worker i's
            //   tool calls cannot land in worker j's ledger.
            // - **Per-PR recorder**: `Arc<Mutex<_>>` with a single
            //   owning thread; internal mutation is serialized
            //   without contention. Each worker writes a distinct
            //   `runs/<run-id>/` directory under the shared state
            //   root; there is no cross-worker shared subtree.
            // - **Stall detection**: state lives on the worker
            //   stack frame; no shared cell.
            let process_outcomes = suite::drive_suite(&args.suite, args.concurrency, |slug, pr| {
                drive_one_pull_request(slug, pr, &args, &repo_root)
            });
            let multi = MultiOutcome::Bundle(process_outcomes);
            // Output-channel partitioning: stdout carries the
            // structured agent-harness contract (one JSONL record
            // per PR in input order); stderr carries human-readable
            // framing; `$?` carries the coarse dispatch signal.
            // Each channel is independently consumable.
            render_multi_jsonl(&mut std::io::stdout(), &multi);
            multi
        }
        Err(usage_msg) => {
            // Dual-lift: one diagnostic, two framings.
            // `parse_args` returns the bare typed message; we
            // construct both `Outcome::UsageError` (stderr) and
            // `MultiOutcome::UsageError` (exit code) directly,
            // discharging the narrowing-match-with-unreachable!
            // pattern at the type system instead of at runtime.
            let outcome: Outcome = Outcome::UsageError(usage_msg.clone());
            render_outcome(&mut std::io::stderr(), &outcome, None);
            MultiOutcome::UsageError(usage_msg)
        }
    };
    ProcessExitCode::from(multi.exit_code())
}

/// Drive a single PR end-to-end on one worker.
///
/// # Sequenced steps
///
/// 1. Open a per-PR `Recorder` keyed on `(slug, pr)`. Each
///    recorder writes a distinct `runs/<run-id>/` directory under
///    the shared state root; no cross-worker subtree exists.
/// 2. Install it as the thread-local tool-call sink (so observed
///    tool calls are attributed to this worker's ledger).
/// 3. Run the mode-selected pipeline (`Loop` or `Inspect`).
/// 4. Render the terminal `Outcome` to stderr and persist it.
///
/// Returns the per-PR [`ProcessOutcome`] carrying the worker's
/// `run_id` so the suite-level JSONL projection can join back to
/// the per-run audit trail.
fn drive_one_pull_request(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    args: &Args,
    repo_root: &Path,
) -> ProcessOutcome {
    let recorder = match Recorder::open(RecorderConfig {
        slug: slug.clone(),
        pr,
        mode: run_mode(args.mode),
        max_iter: args.max_iter,
        status_comment: args.status_comment,
        state_root: args.state_root.clone(),
        legacy_trace: None,
    }) {
        Ok(r) => r,
        Err(e) => {
            // Recorder unavailable for this PR: still surface a
            // `BinaryError` so the aggregate priority projection
            // observes the failure. Stderr framing is the only
            // channel available without a recorder.
            let outcome = Outcome::binary_error(format!("recorder: {e}"));
            render_outcome(&mut std::io::stderr(), &outcome, None);
            return ProcessOutcome {
                slug: slug.clone(),
                pr,
                run_id: String::new(),
                outcome,
            };
        }
    };
    recorder.install_process_recorder();
    let run_id = recorder.run_id();
    let outcome = match args.mode {
        Mode::Inspect => run_inspect(slug, pr, args, repo_root, &recorder),
        Mode::Loop => run_full(slug, pr, args, repo_root, &recorder),
    };
    let code = outcome.exit_code();
    // `write_handoff_md` propagates the IterationHandoff append
    // failure rather than swallowing it; here we're already in the
    // terminal halt path, so the most useful response is to log the
    // failure to stderr (audit trail will be incomplete) and
    // continue with `None` so `render_outcome` still emits the
    // prompt inline.
    let handoff_path = match &outcome {
        Outcome::HandoffAgent(h) => recorder
            .write_handoff_md(
                &h.prompt.to_string(),
                ooda_state::OutcomeKind::HandoffAgent,
                ooda_core::ActionKindName::name(&h.kind),
            )
            .map_err(|e| {
                eprintln!("ooda-prs: handoff audit-trail write failed: {e}");
            })
            .ok(),
        Outcome::HandoffHuman(h) => recorder
            .write_handoff_md(
                &h.prompt.to_string(),
                ooda_state::OutcomeKind::HandoffHuman,
                ooda_core::ActionKindName::name(&h.kind),
            )
            .map_err(|e| {
                eprintln!("ooda-prs: handoff audit-trail write failed: {e}");
            })
            .ok(),
        _ => None,
    };
    render_outcome(&mut std::io::stderr(), &outcome, handoff_path.as_deref());
    let mut rendered = Vec::new();
    render_outcome(&mut rendered, &outcome, handoff_path.as_deref());
    let mut headline = String::new();
    if let Ok(text) = String::from_utf8(rendered) {
        headline = text.lines().next().unwrap_or("").to_string();
        for line in text.lines() {
            recorder.write_trace_line(line);
        }
    }
    recorder.record_outcome(&outcome, code, &headline, handoff_path.as_deref());
    ProcessOutcome {
        slug: slug.clone(),
        pr,
        run_id,
        outcome,
    }
}

fn run_mode(mode: Mode) -> RunMode {
    match mode {
        Mode::Loop => RunMode::Loop,
        Mode::Inspect => RunMode::Inspect,
    }
}

#[allow(clippy::too_many_lines)]
fn run_inspect(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    args: &Args,
    repo_root: &Path,
    recorder: &Recorder,
) -> Outcome {
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
        slug,
        pr,
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
            eprintln!("{} {line}", loop_prefix(slug, pr, Some(&run_id)));
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
        eprintln!("{} {line}", loop_prefix(slug, pr, Some(&run_id)));
        recorder.write_trace_line(&line);
    }
    let oriented = orient(&obs, None, current_timestamp());
    let candidate_actions = runner::drive(&oriented, pr);
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
            slug,
            pr,
            Some(1),
            &comment::render::RenderInputs::from(&oriented),
            &candidate_actions,
            &decision,
        );
        recorder.record_status_comment_rendered(Some(1), &rendered, "inspect comment rendered");
        let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(1));
        log_post_result(slug, pr, "comment", true, r, Some(recorder));
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
    decorate_handoff_human(Outcome::from(decision), slug, pr, Some(&snapshot))
}

fn run_full(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    args: &Args,
    repo_root: &Path,
    recorder: &Recorder,
) -> Outcome {
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
        eprintln!("{} {line}", loop_prefix(slug, pr, Some(&run_id)));
        recorder.write_trace_line(&line);
        if args.status_comment {
            let rendered = comment::render::render(
                slug,
                pr,
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
            let r = comment::post::post_if_changed(slug, pr, &rendered, recorder, Some(i));
            log_post_result(
                slug,
                pr,
                &format!("[iter {i}] comment"),
                false,
                r,
                Some(recorder),
            );
        }
    };
    let outcome = match run_loop(
        slug,
        pr,
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
    decorate_handoff_human(outcome, slug, pr, snapshot.as_ref())
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
        Some(rid) if !rid.is_empty() => format!("[ooda-prs {slug}#{pr} run={rid}]"),
        _ => format!("[ooda-prs {slug}#{pr}]"),
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

fn pull_request_url(slug: &RepoSlug, pr: PullRequestNumber) -> String {
    format!("https://github.com/{slug}/pull/{pr}")
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
    prompt.push_context_line("PR", pull_request_url(slug, pr));
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

/// Project the suite-level `MultiOutcome` onto JSONL stdout.
///
/// # Output contract
///
/// - **Bundle case**: one record per PR, emitted in suite input
///   order. Each record carries the constant fields
///   `slug`/`pr`/`pr_url`/`outcome`/`exit`, plus variant-specific
///   fields folded in by `per_pr_jsonl_record`.
/// - **`UsageError` case**: empty stdout; the `$? = 64` exit code
///   and the stderr usage block together fully discharge the
///   diagnostic.
fn render_multi_jsonl(out: &mut dyn std::io::Write, multi: &MultiOutcome) {
    let MultiOutcome::Bundle(prs) = multi else {
        return;
    };
    for po in prs {
        let _ = writeln!(out, "{}", per_pr_jsonl_record(po));
    }
}

fn per_pr_jsonl_record(po: &ProcessOutcome) -> String {
    use serde_json::{Map, Value, json};
    let mut obj: Map<String, Value> = Map::new();
    obj.insert("slug".into(), json!(po.slug.to_string()));
    obj.insert("pr".into(), json!(po.pr.get()));
    // Deep link inclusion is invariant — consumers index `pr_url`
    // directly rather than reconstruct it per record.
    obj.insert("pr_url".into(), json!(pull_request_url(&po.slug, po.pr)));
    // Run identifier (opaque, generated by `ooda-state`). Joins
    // this per-PR JSONL record back to the on-disk
    // `runs/<run-id>/` audit trail.
    obj.insert("run_id".into(), json!(po.run_id));
    obj.insert("outcome".into(), json!(outcome_variant_name(&po.outcome)));
    obj.insert("exit".into(), json!(po.outcome.exit_code()));
    match &po.outcome {
        Outcome::StuckRepeated(a) | Outcome::StuckCapReached(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
        }
        Outcome::HandoffHuman(h) | Outcome::HandoffAgent(h) => {
            obj.insert("action".into(), json!(h.kind.name()));
            obj.insert("blocker".into(), json!(h.blocker.to_string()));
            obj.insert("prompt".into(), json!(h.prompt.to_string()));
        }
        Outcome::WouldAdvance(a) => {
            obj.insert("action".into(), json!(a.kind.name()));
            obj.insert("blocker".into(), json!(a.blocker.to_string()));
            obj.insert("effect".into(), json!(format_effect(&a.effect)));
        }
        Outcome::BinaryError(s) | Outcome::UsageError(s) => {
            obj.insert("msg".into(), json!(s));
        }
        Outcome::DoneSucceeded | Outcome::DoneAborted | Outcome::Paused => {
            // Terminal-no-payload variants: the constant fields
            // fully describe them.
        }
        Outcome::SignalInterrupted { exit_code } => {
            // The wrapped numeric is also surfaced via `exit`;
            // duplicating it on a typed field keeps the record
            // self-describing for consumers that pivot on outcome
            // tokens rather than exit codes.
            obj.insert("signal_exit_code".into(), json!(exit_code));
        }
    }
    Value::Object(obj).to_string()
}

fn outcome_variant_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::DoneSucceeded => "DoneMerged",
        Outcome::StuckRepeated(_) => "StuckRepeated",
        Outcome::StuckCapReached(_) => "StuckCapReached",
        Outcome::HandoffHuman(_) => "HandoffHuman",
        Outcome::WouldAdvance(_) => "WouldAdvance",
        Outcome::HandoffAgent(_) => "HandoffAgent",
        Outcome::BinaryError(_) => "BinaryError",
        Outcome::Paused => "Paused",
        Outcome::DoneAborted => "DoneClosed",
        Outcome::UsageError(_) => "UsageError",
        Outcome::SignalInterrupted { .. } => "SignalInterrupted",
    }
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
    fn loop_prefix_carries_binary_slug_pr_and_run_id() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        assert_eq!(
            loop_prefix(&slug, pr, Some("abc123")),
            "[ooda-prs acme/widget#42 run=abc123]"
        );
    }

    #[test]
    fn loop_prefix_omits_run_id_when_absent() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        assert_eq!(loop_prefix(&slug, pr, None), "[ooda-prs acme/widget#42]");
        // Empty run-id collapses to the same shape as None: keeps
        // the prefix grammar regular so a stderr grep can pivot on
        // the presence of `run=`.
        assert_eq!(
            loop_prefix(&slug, pr, Some("")),
            "[ooda-prs acme/widget#42]"
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

    // ─── per-PR JSONL records (suite stdout contract) ───────────────

    fn po(slug: &str, pr_num: u64, outcome: Outcome) -> ProcessOutcome {
        ProcessOutcome {
            slug: RepoSlug::parse(slug).unwrap(),
            pr: PullRequestNumber::new(pr_num).unwrap(),
            run_id: "test-run-id".to_string(),
            outcome,
        }
    }

    fn parse_record(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("JSONL record must parse as JSON")
    }

    // ─── per-PR JSONL schema goldens ────────────────────────────────
    //
    // Schema goldens for `per_pr_jsonl_record`'s output. The field
    // names are an external contract — downstream tools index them
    // directly — so renames surface here as test failures.
    //
    // Exhaustiveness is layered:
    //   structural   — `pull_request_jsonl_golden`'s match on `Outcome`
    //                  denies a non-exhaustive arm at compile time.
    //   runtime      — the length sentinel in the test pins sample
    //                  coverage to the variant count.

    /// Canonical schema for `per_pr_jsonl_record`'s output:
    /// constant outer fields plus a variant-specific tail.
    fn pull_request_jsonl_golden(outcome: &Outcome) -> serde_json::Value {
        use serde_json::json;
        // Outer object is invariant across `Outcome` variants;
        // the per-variant arms below extend it with the
        // variant-specific tail.
        let mut o = serde_json::Map::new();
        o.insert("slug".into(), json!("acme/widget"));
        o.insert("pr".into(), json!(42));
        o.insert(
            "pr_url".into(),
            json!("https://github.com/acme/widget/pull/42"),
        );
        o.insert("run_id".into(), json!("test-run-id"));
        match outcome {
            Outcome::DoneSucceeded => {
                o.insert("outcome".into(), json!("DoneMerged"));
                o.insert("exit".into(), json!(0));
            }
            Outcome::Paused => {
                o.insert("outcome".into(), json!("Paused"));
                o.insert("exit".into(), json!(1));
            }
            Outcome::WouldAdvance(a) => {
                o.insert("outcome".into(), json!("WouldAdvance"));
                o.insert("exit".into(), json!(2));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
                o.insert("effect".into(), json!(format_effect(&a.effect)));
            }
            Outcome::HandoffHuman(h) => {
                o.insert("outcome".into(), json!("HandoffHuman"));
                o.insert("exit".into(), json!(3));
                o.insert("action".into(), json!(h.kind.name()));
                o.insert("blocker".into(), json!(h.blocker.to_string()));
                o.insert("prompt".into(), json!(h.prompt.to_string()));
            }
            Outcome::HandoffAgent(h) => {
                o.insert("outcome".into(), json!("HandoffAgent"));
                o.insert("exit".into(), json!(4));
                o.insert("action".into(), json!(h.kind.name()));
                o.insert("blocker".into(), json!(h.blocker.to_string()));
                o.insert("prompt".into(), json!(h.prompt.to_string()));
            }
            Outcome::DoneAborted => {
                o.insert("outcome".into(), json!("DoneClosed"));
                o.insert("exit".into(), json!(5));
            }
            Outcome::StuckRepeated(a) => {
                o.insert("outcome".into(), json!("StuckRepeated"));
                o.insert("exit".into(), json!(6));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
            }
            Outcome::StuckCapReached(a) => {
                o.insert("outcome".into(), json!("StuckCapReached"));
                o.insert("exit".into(), json!(7));
                o.insert("action".into(), json!(a.kind.name()));
                o.insert("blocker".into(), json!(a.blocker.to_string()));
            }
            Outcome::UsageError(msg) => {
                o.insert("outcome".into(), json!("UsageError"));
                o.insert("exit".into(), json!(64));
                o.insert("msg".into(), json!(msg.as_str()));
            }
            Outcome::BinaryError(msg) => {
                o.insert("outcome".into(), json!("BinaryError"));
                o.insert("exit".into(), json!(70));
                o.insert("msg".into(), json!(msg.as_str()));
            }
            Outcome::SignalInterrupted { exit_code } => {
                o.insert("outcome".into(), json!("SignalInterrupted"));
                o.insert("exit".into(), json!(exit_code));
                o.insert("signal_exit_code".into(), json!(exit_code));
            }
        }
        serde_json::Value::Object(o)
    }

    /// Sample coverage over `Outcome`: one inhabitant per variant.
    /// Hand-maintained; the length sentinel in
    /// `jsonl_schema_goldens_exhaustive` guards drift. Variants
    /// carrying payloads use distinct kinds / blockers so the
    /// golden distinguishes them by shape, not by chance.
    fn pull_request_jsonl_sample_outcomes() -> Vec<Outcome> {
        let stuck_action = action("rebase-needed");
        let mut would_advance_action = action("ci_pending: build");
        would_advance_action.effect = ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(60),
            log: "Wait for 2 pending checks".into(),
        };
        // Handoff variants carry `HandoffAction` (typed projection
        // with a direct `prompt` field). Construct directly — the
        // structural narrowing eliminates the prior `Action` +
        // `effect` mutation path.
        let handoff_agent_action = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Address 2 unresolved review threads."),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingFix),
            blocker: ids::BlockerKey::from_static("unresolved_threads"),
        };
        let handoff_human_action = ooda_core::HandoffAction {
            kind: decide::action::ActionKind::Rebase,
            prompt: ooda_core::HandoffPrompt::new("Approve the PR."),
            target_effect: decide::action::TargetEffect::Blocks,
            urgency: decide::action::Urgency::Mid(MidTier::BlockingHuman),
            blocker: ids::BlockerKey::from_static("pending_human_review: alice"),
        };
        vec![
            Outcome::DoneSucceeded,
            Outcome::Paused,
            Outcome::WouldAdvance(Box::new(would_advance_action)),
            Outcome::HandoffHuman(Box::new(handoff_human_action)),
            Outcome::HandoffAgent(Box::new(handoff_agent_action)),
            Outcome::DoneAborted,
            Outcome::StuckRepeated(Box::new(stuck_action.clone())),
            Outcome::StuckCapReached(Box::new(stuck_action)),
            Outcome::UsageError("bad --concurrency".into()),
            Outcome::BinaryError("observe: gh: connection refused".into()),
            Outcome::SignalInterrupted { exit_code: 143 },
        ]
    }

    /// Variant-wise golden assertions for `per_pr_jsonl_record`'s
    /// schema. Compile-time exhaustiveness over `Outcome` is
    /// supplied by `pull_request_jsonl_golden`'s match; runtime
    /// exhaustiveness over the sample list is supplied by the
    /// length-sentinel.
    #[test]
    fn jsonl_schema_goldens_exhaustive() {
        let samples = pull_request_jsonl_sample_outcomes();
        assert_eq!(
            samples.len(),
            11,
            "`pull_request_jsonl_sample_outcomes` must include one sample per `Outcome` variant; \
             adding a new variant requires adding both a golden arm in `pull_request_jsonl_golden` \
             AND a sample here.",
        );
        for outcome in samples {
            let outcome_name = outcome_variant_name(&outcome);
            let po = po("acme/widget", 42, outcome);
            let actual = parse_record(&per_pr_jsonl_record(&po));
            let expected = pull_request_jsonl_golden(&po.outcome);
            assert_eq!(
                actual, expected,
                "schema mismatch for variant {outcome_name}"
            );
        }
    }

    #[test]
    fn decorate_handoff_human_appends_pull_request_link_and_blocker() {
        use crate::decide::action::{ActionKind, TargetEffect, Urgency};
        use crate::ids::BlockerKey;
        let h = ooda_core::HandoffAction {
            kind: ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("Request or self-approve"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("not_approved"),
        };
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("42").unwrap();
        let decorated = decorate_handoff_human(Outcome::HandoffHuman(Box::new(h)), &slug, pr, None);
        let Outcome::HandoffHuman(h) = decorated else {
            panic!("expected HandoffHuman");
        };
        let rendered = h.prompt.to_string();
        assert!(
            rendered.contains("**PR:** https://github.com/acme/widget/pull/42"),
            "decoration: {rendered}",
        );
        assert!(rendered.contains("**Blocker:** not_approved"));
        assert!(rendered.starts_with("# Request or self-approve"));
    }

    #[test]
    fn decorate_handoff_human_passes_through_other_variants() {
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let pr = PullRequestNumber::parse("1").unwrap();
        assert!(matches!(
            decorate_handoff_human(Outcome::DoneSucceeded, &slug, pr, None),
            Outcome::DoneSucceeded
        ));
        assert!(matches!(
            decorate_handoff_human(Outcome::Paused, &slug, pr, None),
            Outcome::Paused
        ));
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

    // Per-variant shape assertions are subsumed by
    // `jsonl_schema_goldens_exhaustive`'s golden arms — adding
    // sibling per-variant tests here would duplicate the contract.

    #[test]
    fn render_multi_jsonl_emits_one_line_per_pull_request_in_order() {
        let multi = MultiOutcome::Bundle(vec![
            po("a/b", 1, Outcome::DoneSucceeded),
            po(
                "a/b",
                2,
                Outcome::HandoffAgent(Box::new(handoff("unresolved"))),
            ),
            po("c/d", 9, Outcome::Paused),
        ]);
        let mut buf = Vec::new();
        render_multi_jsonl(&mut buf, &multi);
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        let first = parse_record(lines[0]);
        let second = parse_record(lines[1]);
        let third = parse_record(lines[2]);
        assert_eq!(
            (first["slug"].as_str(), first["pr"].as_u64()),
            (Some("a/b"), Some(1))
        );
        assert_eq!(
            (second["slug"].as_str(), second["pr"].as_u64()),
            (Some("a/b"), Some(2))
        );
        assert_eq!(
            (third["slug"].as_str(), third["pr"].as_u64()),
            (Some("c/d"), Some(9))
        );
        assert_eq!(second["outcome"], "HandoffAgent");
    }

    #[test]
    fn render_multi_jsonl_usage_error_emits_nothing() {
        let multi = MultiOutcome::UsageError("bad invocation".into());
        let mut buf = Vec::new();
        render_multi_jsonl(&mut buf, &multi);
        assert!(buf.is_empty(), "UsageError must not write to stdout");
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
        let bogus = std::env::temp_dir().join("ooda-prs-resolve-nonexistent-XYZZY");
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
