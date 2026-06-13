//! Branch-sync probes and the per-PR sticky head record.
//!
//! Drives the branch-sync axis: an out-of-band push on the PR's
//! branch (by a human, a sibling automation, or a sister
//! invocation) advances the remote head past what this driver
//! caused. Detection is "sticky SHA on disk != current remote
//! head"; classification picks between [`SyncGraphiteStack`] (the
//! branch is graphite-tracked and we know how to converge it) and
//! [`InvestigatePush`] (everything else — hand off to an agent).
//!
//! # Sticky schema
//!
//! Persisted at `<pr-index-path>/last_seen_head.json` as
//! [`StickyHead`]. The `pending` flag implements a
//! crash-tolerant `C9` transactional update for push-shaped
//! actions:
//!
//! - `pending = false`: this is the SHA the driver last observed
//!   or successfully landed. Compare to the live `headRefOid` to
//!   detect divergence.
//! - `pending = true`: the driver issued a push toward this SHA
//!   but has not yet confirmed it landed. On the next observe,
//!   matching SHA promotes to `pending = false`; mismatching SHA
//!   leaves the marker in place (our push has not yet propagated
//!   or someone else pushed; either way we have already mutated,
//!   so divergence is suppressed for one iteration).
//!
//! Writes go through [`ooda_core::atomic_io::write_atomic`] under
//! an [`ooda_core::FileLock`] on the sticky path so concurrent
//! invocations (already serialised at the act stage by
//! `.action.lock`, but observe is unlocked) cannot tear the read
//! → decide → write window.
//!
//! [`SyncGraphiteStack`]: crate::decide::action::ActionKind::SyncGraphiteStack
//! [`InvestigatePush`]: crate::decide::action::ActionKind::InvestigatePush

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use ooda_core::FileLock;
use ooda_core::atomic_io::write_atomic;
use ooda_core::{SpawnLimits, run_with_limits};

use crate::ids::{BranchName, GitCommitSha};

/// `gt --version` is a local-only probe. Even on a slow disk the
/// graphite binary's version banner returns sub-second; 10s gives
/// fork/exec headroom without letting a wedged installation pin
/// the observe-stage fan-out.
const GT_VERSION_DEADLINE: Duration = Duration::from_secs(10);

/// Per-stream byte cap for `gt --version`. Output is a single
/// version banner line; 4 KiB tolerates a verbose build identifier
/// while keeping a misbehaving probe from growing memory unbounded.
const GT_VERSION_MAX_BYTES: usize = 4 * 1024;

/// `gt log --stack <branch>` walks the local stack and can chase a
/// few commits up the tree, but does not hit the network. 30s
/// covers the longest stacks observed in practice while keeping
/// the bound tight enough that a wedged probe is detected.
const GT_LOG_STACK_DEADLINE: Duration = Duration::from_secs(30);

/// Per-stream byte cap for `gt log --stack`. The output is a
/// per-branch listing; 1 MiB covers the longest stacks observed
/// while keeping a runaway probe bounded.
const GT_LOG_STACK_MAX_BYTES: usize = 1024 * 1024;

/// Build the standard per-call limits for `gt --version`.
fn gt_version_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GT_VERSION_DEADLINE,
        max_stdout_bytes: GT_VERSION_MAX_BYTES,
        max_stderr_bytes: GT_VERSION_MAX_BYTES,
    }
}

/// Build the standard per-call limits for `gt log --stack`.
fn gt_log_stack_limits() -> SpawnLimits {
    SpawnLimits {
        deadline: GT_LOG_STACK_DEADLINE,
        max_stdout_bytes: GT_LOG_STACK_MAX_BYTES,
        max_stderr_bytes: GT_LOG_STACK_MAX_BYTES,
    }
}

/// Per-PR sticky head record. The driver writes this after every
/// successful observe and after every push-shaped action. The
/// branch-sync axis reads it to detect divergence between what we
/// last caused and what the upstream now reports.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct StickyHead {
    /// The remote head SHA the driver last observed or pushed.
    pub head_sha: String,
    /// `true` while a push-shaped action is in flight against
    /// `head_sha` and we have not yet observed it land. See
    /// module docs for the promotion rule.
    #[serde(default)]
    pub pending: bool,
    /// When the record was written. Diagnostic; not used by the
    /// divergence comparator.
    pub recorded_at: DateTime<Utc>,
}

/// Information about a divergent remote head — surfaced to decide
/// for prompt composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BranchDivergence {
    pub from_sha: String,
    pub to_sha: String,
}

/// Observation feeding the branch-sync axis.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BranchSyncObservation {
    /// `Some(_)` when sticky head differs from current observed
    /// head; `None` when in sync (or first observation).
    pub divergence: Option<BranchDivergence>,
    /// `true` iff the PR branch is graphite-tracked. Probed via
    /// `gt log --stack <branch>` on the local repo.
    pub branch_graphite_tracked: bool,
    /// `true` iff `gt` is available on PATH. Cached per invocation.
    pub gt_available: bool,
}

/// Read the sticky from disk; absence yields `None` (first
/// observation), malformed JSON yields `None` (defensive: a torn
/// sticky degrades to "no prior observation" rather than aborting
/// the observe pass).
pub(crate) fn read_sticky(sticky_path: &Path) -> Option<StickyHead> {
    let bytes = std::fs::read(sticky_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the sticky atomically under a [`FileLock`] on its own path.
/// Acquiring the lock serialises any concurrent observer/actor
/// pair on the same `(slug, pr)`.
pub(crate) fn write_sticky(
    sticky_path: &Path,
    head_sha: &str,
    pending: bool,
) -> std::io::Result<()> {
    if let Some(parent) = sticky_path.parent() {
        // 0o700 on the index ancestor — see `pr_index_path` rationale
        // in `recorder.rs`. The leaf sticky-head file and sibling
        // `.lock` sidecar name the observed PR.
        ooda_core::atomic_io::secure_create_dir_all(parent)?;
    }
    let _lock = FileLock::acquire(sticky_path)?;
    let record = StickyHead {
        head_sha: head_sha.to_owned(),
        pending,
        recorded_at: Utc::now(),
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_atomic(sticky_path, &bytes)
}

/// Compare the sticky to the current remote head. Promotes the
/// `pending` marker when an in-flight push has landed; suppresses
/// divergence detection when our own push is still in flight.
///
/// Returns the divergence to surface, or `None` if in sync.
pub(crate) fn classify_divergence(
    sticky: Option<&StickyHead>,
    current_head: &GitCommitSha,
) -> Option<BranchDivergence> {
    let sticky = sticky?;
    if sticky.head_sha == current_head.as_str() {
        // SHAs match: either steady-state or our push just landed.
        // Either way no divergence; caller normalises the pending
        // bit on the post-observe write.
        return None;
    }
    if sticky.pending {
        // Our own push is in flight against `sticky.head_sha` and
        // the remote shows a different SHA — push has not yet
        // propagated (or the upstream rewrote it). Either path,
        // the next observe will resolve; suppress divergence one
        // iteration.
        return None;
    }
    Some(BranchDivergence {
        from_sha: sticky.head_sha.clone(),
        to_sha: current_head.as_str().to_owned(),
    })
}

/// Probe `gt --version` inside `repo_root`. Probes are scoped to
/// the target repo so a graphite installation that lives elsewhere
/// on `$PATH` is still detected, but the spawn never inherits the
/// caller's CWD — that would otherwise let a graphite-tracked
/// sibling repo answer for the target.
///
/// Result is NOT cached across calls. Caching by binary presence
/// alone would discard the `repo_root` parameter, defeating the
/// CWD-scoping guarantee the rest of this module depends on.
pub(crate) fn gt_available(repo_root: &Path) -> bool {
    // Best-effort probe: any failure (spawn error, timeout, non-zero
    // exit) degrades to "not available". A timed-out `gt --version`
    // still routes the next iteration to `InvestigatePush` rather
    // than wedging the observe pass.
    run_with_limits(
        &mut build_gt_version_command(repo_root),
        gt_version_limits(),
    )
    .is_ok_and(|o| o.status.success())
}

/// Probe `gt log --stack <branch>`. Exit code 0 ⇒ branch is
/// graphite-tracked; anything else ⇒ untracked (or `gt` failed).
/// Failures are silent — graphite-status is informational; a
/// failed probe degrades to "not tracked" so the axis routes to
/// `InvestigatePush` instead of trying to drive `gt sync`.
///
/// `repo_root` pins the probe to the resolved target repo; without
/// it, a sibling repo on disk whose checkout happens to share the
/// branch name would answer `true` and route the next iteration to
/// `gt sync` against the wrong stack.
pub(crate) fn branch_graphite_tracked(branch: &BranchName, repo_root: &Path) -> bool {
    if !gt_available(repo_root) {
        return false;
    }
    // Same best-effort discipline as `gt_available`: timeout or any
    // other failure degrades to "not tracked".
    run_with_limits(
        &mut build_gt_log_stack_command(branch, repo_root),
        gt_log_stack_limits(),
    )
    .is_ok_and(|o| o.status.success())
}

/// Construct the `gt --version` command pinned to `repo_root`. Split
/// from [`gt_available`] so the `current_dir` invariant has a
/// dedicated test target without spawning a real `gt` binary.
fn build_gt_version_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new("gt");
    cmd.current_dir(repo_root).arg("--version");
    cmd
}

/// Construct the `gt log --stack <branch>` command pinned to
/// `repo_root`. Split from [`branch_graphite_tracked`] for the
/// same reason as [`build_gt_version_command`].
fn build_gt_log_stack_command(branch: &BranchName, repo_root: &Path) -> Command {
    let mut cmd = Command::new("gt");
    cmd.current_dir(repo_root)
        .args(["log", "--stack", branch.as_str()]);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn sha(s: &str) -> GitCommitSha {
        GitCommitSha::parse(s).unwrap()
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("last_seen_head.json");
        write_sticky(&path, SHA_A, false).unwrap();
        let s = read_sticky(&path).unwrap();
        assert_eq!(s.head_sha, SHA_A);
        assert!(!s.pending);
    }

    #[test]
    fn read_missing_sticky_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(read_sticky(&path).is_none());
    }

    #[test]
    fn read_malformed_sticky_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("torn.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_sticky(&path).is_none());
    }

    #[test]
    fn classify_none_sticky_yields_no_divergence() {
        assert!(classify_divergence(None, &sha(SHA_A)).is_none());
    }

    #[test]
    fn classify_equal_shas_yields_no_divergence() {
        let s = StickyHead {
            head_sha: SHA_A.to_owned(),
            pending: false,
            recorded_at: Utc::now(),
        };
        assert!(classify_divergence(Some(&s), &sha(SHA_A)).is_none());
    }

    #[test]
    fn classify_distinct_shas_yields_divergence() {
        let s = StickyHead {
            head_sha: SHA_A.to_owned(),
            pending: false,
            recorded_at: Utc::now(),
        };
        let d = classify_divergence(Some(&s), &sha(SHA_B)).unwrap();
        assert_eq!(d.from_sha, SHA_A);
        assert_eq!(d.to_sha, SHA_B);
    }

    #[test]
    fn classify_pending_in_flight_suppresses_divergence() {
        // Our own push is mid-flight (pending=true) and the remote
        // shows a different SHA — the suppression rule prevents a
        // false self-divergence.
        let s = StickyHead {
            head_sha: SHA_A.to_owned(),
            pending: true,
            recorded_at: Utc::now(),
        };
        assert!(classify_divergence(Some(&s), &sha(SHA_B)).is_none());
    }

    #[test]
    fn classify_pending_landed_promotes_to_no_divergence() {
        // Our push landed: pending sticky's SHA now matches the
        // current remote head. No divergence; caller clears
        // pending on the post-observe write.
        let s = StickyHead {
            head_sha: SHA_A.to_owned(),
            pending: true,
            recorded_at: Utc::now(),
        };
        assert!(classify_divergence(Some(&s), &sha(SHA_A)).is_none());
    }

    #[test]
    fn write_promotes_pending_bit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("last_seen_head.json");
        write_sticky(&path, SHA_A, true).unwrap();
        assert!(read_sticky(&path).unwrap().pending);
        write_sticky(&path, SHA_A, false).unwrap();
        assert!(!read_sticky(&path).unwrap().pending);
    }

    /// `Command`'s `Debug` impl prefixes the rendered argv with
    /// `cd "<path>" && ` when `current_dir` is set. We assert on
    /// that rendering to pin the CWD-scoping invariant without
    /// requiring a real `gt` on the test host.
    #[test]
    fn gt_version_command_targets_repo_root() {
        let dir = tempdir().unwrap();
        let cmd = build_gt_version_command(dir.path());
        let rendered = format!("{cmd:?}");
        let needle = format!("cd {:?}", dir.path());
        assert!(
            rendered.contains(&needle),
            "expected {needle:?} in {rendered:?}",
        );
    }

    #[test]
    fn gt_log_stack_command_targets_repo_root_and_branch() {
        let dir = tempdir().unwrap();
        let branch = BranchName::parse("feature/x").unwrap();
        let cmd = build_gt_log_stack_command(&branch, dir.path());
        let rendered = format!("{cmd:?}");
        let needle_dir = format!("cd {:?}", dir.path());
        assert!(
            rendered.contains(&needle_dir),
            "expected {needle_dir:?} in {rendered:?}",
        );
        assert!(
            rendered.contains("\"feature/x\""),
            "expected branch in {rendered:?}",
        );
    }
}
