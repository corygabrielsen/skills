//! Parallel suite spawn loop.
//!
//! `drive_suite` runs the per-PR pipeline (`drive_one_pr` in
//! `main.rs`) in parallel under `std::thread::scope`. Each PR is
//! handled by exactly one thread. The thread:
//!
//!   1. Opens a `Recorder` keyed by `(slug, pr)` and installs it as
//!      the **thread-local** tool-call sink (see
//!      `recorder::THREAD_RECORDER`).
//!   2. Runs the configured mode (`Loop` or `Inspect`).
//!   3. Renders its `Outcome` to stderr (serialized via a shared
//!      stderr mutex so per-PR variant blocks don't interleave
//!      mid-line) and records it on its own Recorder.
//!
//! Joins at scope exit. Per-PR Outcomes are collected in input order
//! and returned to `main` for `MultiOutcome::Bundle` construction.
//!
//! ## Concurrency cap
//!
//! `--concurrency K` (default = `|suite|`) bounds simultaneous
//! in-flight PRs. The implementation uses an `AtomicUsize` work
//! index: `cap` worker threads each grab the next index until the
//! suite is exhausted. This gives **rolling** concurrency — a
//! finished PR releases its slot for the next, no batching wait.
//!
//! Why not a process-pool / mpsc channel? An atomic counter is the
//! minimal mechanism here: the work set is finite and known, and
//! each task is independent. mpsc + crossbeam would add a dependency
//! and a wider API surface for no behavioral gain.
//!
//! ## Stderr serialization
//!
//! Without a mutex, per-iteration log lines from N threads would
//! interleave mid-byte on stderr (because `eprintln!` makes no
//! atomicity promises across calls). Each thread's diagnostic line
//! prefix `[<slug>#<pr>] ...` makes the log human-disentangle-able
//! once per-line atomicity is preserved.
//!
//! For now, the existing per-iteration logging in `run_inspect` /
//! `run_full` (in `main.rs`) writes via `eprintln!`. The Mutex below
//! is held briefly around `render_outcome` calls (the variant
//! block) so the final per-PR header + prompt block stays
//! contiguous. Per-iteration line interleaving is left as
//! human-tractable noise; if it becomes a problem we'll route all
//! `eprintln!`s through this mutex.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::multi_outcome::ProcessOutcome;
use crate::outcome::Outcome;

/// Drive every PR in `suite` in parallel. `drive_one` is invoked
/// once per `(slug, pr)` on its own thread; the closure is
/// responsible for opening its per-PR recorder, running the
/// pipeline, rendering stderr, and recording the outcome — see
/// `main::drive_one_pr`.
///
/// `cap` is the maximum number of simultaneously-active PRs.
/// `cap = 0` is clamped to `1`; `cap > suite.len()` is clamped to
/// `suite.len()`. `None` means no cap (= `suite.len()`).
///
/// Returns per-PR `ProcessOutcome`s in **input order**, so the
/// caller's `MultiOutcome::Bundle` retains the operator's intent.
pub fn drive_suite<F>(
    suite: &[(RepoSlug, PullRequestNumber)],
    concurrency: Option<u32>,
    drive_one: F,
) -> Vec<ProcessOutcome>
where
    F: Fn(&RepoSlug, PullRequestNumber) -> Outcome + Sync,
{
    let n = suite.len();
    if n == 0 {
        return Vec::new();
    }
    let cap = concurrency.map(|c| c as usize).unwrap_or(n).max(1).min(n);

    // Per-index result slot. Each worker writes exactly its own
    // slot, so the Mutex is never contended; we use it only because
    // `Vec<T>` doesn't permit per-element interior mutability without
    // either `Mutex<T>` or `UnsafeCell`. The Mutex form is also the
    // honest signal that "thread X owns slot i" without unsafe.
    let results: Vec<Mutex<Option<Outcome>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..cap {
            let drive_one = &drive_one;
            let results = &results;
            let next = &next;
            scope.spawn(move || {
                loop {
                    // SeqCst because the work index is the cross-
                    // thread synchronization point. Relaxed would
                    // suffice for correctness on x86, but SeqCst
                    // keeps the model portable and the perf cost
                    // is negligible on a counter incremented
                    // O(|suite|) times total.
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let (slug, pr) = &suite[i];
                    let outcome = drive_one(slug, *pr);
                    let mut slot = results[i].lock().expect("result slot mutex poisoned");
                    *slot = Some(outcome);
                }
            });
        }
    });

    // After scope exit all worker threads have joined. Drain the
    // result slots in input order; every slot must be Some(_) by
    // construction (atomic counter visits each i exactly once and
    // each visit assigns).
    suite
        .iter()
        .zip(results)
        .map(|((slug, pr), slot)| {
            let outcome = slot
                .into_inner()
                .expect("result slot mutex poisoned")
                .expect("result slot not set — atomic counter invariant violated");
            ProcessOutcome {
                slug: slug.clone(),
                pr: *pr,
                outcome,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering as O;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::parse(s).unwrap()
    }

    fn pr(n: u64) -> PullRequestNumber {
        PullRequestNumber::new(n).unwrap()
    }

    #[test]
    fn empty_suite_returns_empty() {
        let out = drive_suite(&[], None, |_, _| Outcome::DoneMerged);
        assert!(out.is_empty());
    }

    #[test]
    fn preserves_input_order() {
        let s = [
            (slug("a/b"), pr(10)),
            (slug("a/b"), pr(20)),
            (slug("c/d"), pr(30)),
        ];
        let out = drive_suite(&s, None, |_, p| {
            // Map each PR to a unique BinaryError carrying its
            // number, so we can assert order independently of which
            // thread ran first.
            Outcome::BinaryError(format!("pr={p}"))
        });
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].pr.get(), 10);
        assert_eq!(out[1].pr.get(), 20);
        assert_eq!(out[2].pr.get(), 30);
        // Slug + outcome carry through.
        assert_eq!(out[0].slug.to_string(), "a/b");
        assert_eq!(out[2].slug.to_string(), "c/d");
        match &out[1].outcome {
            Outcome::BinaryError(s) => assert_eq!(s, "pr=20"),
            _ => panic!("expected BinaryError"),
        }
    }

    #[test]
    fn drives_each_pr_exactly_once() {
        // Verify the atomic counter visits each index exactly once.
        let s: Vec<_> = (1..=10).map(|n| (slug("x/y"), pr(n))).collect();
        let counter = AtomicU32::new(0);
        let _ = drive_suite(&s, None, |_, _| {
            counter.fetch_add(1, O::SeqCst);
            Outcome::Paused
        });
        assert_eq!(counter.load(O::SeqCst), 10);
    }

    #[test]
    fn concurrency_cap_clamps_at_one() {
        // cap=0 should clamp to 1 (single worker). All PRs still
        // run; just sequentially.
        let s: Vec<_> = (1..=5).map(|n| (slug("x/y"), pr(n))).collect();
        let counter = AtomicU32::new(0);
        let out = drive_suite(&s, Some(0), |_, _| {
            counter.fetch_add(1, O::SeqCst);
            Outcome::DoneMerged
        });
        assert_eq!(counter.load(O::SeqCst), 5);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn concurrency_cap_above_n_clamps_to_n() {
        // cap=100 with |suite|=3 — extra workers are wasteful but
        // not incorrect. We just verify all PRs run and outcomes
        // are collected in order.
        let s: Vec<_> = (1..=3).map(|n| (slug("x/y"), pr(n))).collect();
        let out = drive_suite(&s, Some(100), |_, p| Outcome::BinaryError(format!("{p}")));
        assert_eq!(out.len(), 3);
        for (i, po) in out.iter().enumerate() {
            assert_eq!(po.pr.get(), (i + 1) as u64);
        }
    }
}
