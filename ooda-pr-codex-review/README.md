# `/ooda-pr-codex-review` — Type Algebra

Single-binary OODA loop for driving a PR through observe → orient →
decide → act until merge or external resolution, optionally running
a local `codex review` axis on the same tick.

This document is the **type-level specification**. For invocation
and exit-code taxonomy see `SKILL.md`. For implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder ⊕ dashboard
    ⊕ comment ⊕ signal ⊕ outcome
        ⊕ ooda-core ⊕ ooda-state (sibling crates)
        ⊕ ooda-attest (companion binary — writes the attestation files
                       the attestation axes observe)

observe = github ⊕ branch ⊕ codex          (codex: local filesystem only)
orient  = … ⊕ codex_review
decide  = … ⊕ codex_review

run_loop : ActContext × Option⟨StateRoot⟩ × LoopConfig × Recorder × OnState
         → Result⟨LoopExit, LoopError⟩

LoopConfig = max_iterations: NonZeroU32 × codex_review: Option⟨CodexReviewConfig⟩
CodexReviewConfig = floor: CodexReasoningLevel × ceiling: CodexReasoningLevel   (floor ≤ ceiling)

ActContext = slug: RepoSlug × pr: PullRequestNumber
           × action_lock_path: PathBuf × repo_root: PathBuf
           × codex: Option⟨CodexActContext⟩
CodexActContext = codex_bin: PathBuf × codex_pr_root: PathBuf × n: u32
                × head_sha: String × base_branch: String       (refreshed every iteration from observe)
                × _lock: FileLock                              (FD-tied; held for the invocation)

LoopExit  = Halted(HaltReason) ⊕ SignalInterrupted{exit_code: u8}
LoopError = Observe(GhError) ⊕ CodexObserve(io::Error) ⊕ Act(ActError) ⊕ Recorder(RecorderError)

main : Argv → Outcome → ExitCode
ExitCode = Outcome.exit_code()       (1:1 variant → code; see Outcome)
```

The codex axis is enabled iff `--codex-review-ceiling ≠ off`.
`LoopConfig.codex_review = None ⟺ ActContext.codex = None ⟺
OrientedState.codex_review = None`; with the axis disabled the
binary is observationally identical to `ooda-pr`.
`--codex-review-floor` (default `low`), `--codex-review-n`
(default 3, ≥ 1) and `--codex-review-bin` (default `codex`) tune
the axis and are a `UsageError` without a ceiling; `floor >
ceiling` is a `UsageError`.

`recorder.rs` is a thin adapter over the shared `ooda-state` crate.
The PR-specific event vocabulary (`action_started`,
`status_comment_rendered`, `tool_call_finished`,
`codex_review_config`, …) lives here; the generic on-disk layout
(events.jsonl plus content-addressed blobs) is owned by
`ooda-state`. Invariants the underlying state model establishes:

- **Append-only causality**: events appended to `events.jsonl`
  under `PIPE_BUF` are atomic w.r.t. concurrent readers.
- **Content-addressed write-once**: every payload is written via
  `tmp+rename` to `blobs/<sha>.<ext>`; identical bytes dedup.
- **Atomic live marker**: `live/<run-id>` is created via
  `O_CREAT|O_EXCL` at run start and `unlink`-ed at halt; presence
  is the source of truth for "active".

Per-PR sidecars live under the PR workspace root:

```
pr_workspace_root = <state_root>/workspaces/pr-codex-review/<owner>/<repo>/<pr>/
    .action.lock          per-PR action lock (every Full effect)
    last_seen_head.json   sticky head for the branch-sync comparator
    codex/                codex_pr_root (0o700)
        .lock             invocation lock — try_acquire; held ⇒ BinaryError
        levels/<level>/<sha[:12]>/   batch directories (see Observe)
```

### Shared boundary types (`ooda-core` crate)

This binary depends on the sibling [`ooda-core`](../ooda-core/)
library crate for the cross-binary type spine: `Outcome`,
`Decision`, `DecisionHalt`, `HaltReason`, `Terminal`, `Action`,
`HandoffAction`, `ActionEffect`, `UpstreamConsistency`, `Urgency`,
`MidTier`, `TargetEffect`, `BlockerKey`, `GateIdentity`,
`StallKey`, `NonEmpty`, `PollingInterval`, `HandoffPrompt`,
`Witness`, `FileLock`, the rate-limit types (`RateLimitBudget`,
`RateLimitHit`, `RateLimitScope`), the `attest` schema module, and
the `ActionKindName` trait. Each generic-over-`ActionKind` type is
instantiated locally via a type alias
(`pub type Outcome = ooda_core::Outcome<ActionKind>` and similar)
so call sites stay non-generic. This binary's `ActionKind` enum is
the PR-domain enum plus four codex-review variants; its
`ActionKindName::name()` implementation stays in
`decide/action.rs`. `CodexReasoningLevel`, the codex observe /
orient / decide modules, `ActContext`, and the per-binary
`LoopError`, `runner.rs`, and `recorder.rs` are not lifted (see
`ooda-core/README.md`).

**Variant name ≠ stderr header.** The Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are neutral verbs
defined in `ooda-core`. The stderr header strings emitted by
this binary's `render_outcome` are the PR-domain vocabulary
(`DoneMerged`, `DoneClosed`, `Paused`). The mapping is shown in
the Outcome section below; callers dispatch on `$?` and read
the stderr header, not the variant name.

### Domain primitives (`ids` module)

Every identifier is a validated newtype. No `String`s representing
domain concepts cross a module boundary.

```
Owner            := { String | non-empty ∧ ¬contains '/' }
Repo             := { String | non-empty ∧ ¬contains '/' }
RepoSlug         := Owner × Repo                            (Display: "owner/repo")
PullRequestNumber:= { ℕ | > 0 }
GitCommitSha     := { String | |s| = 40 ∧ s ⊂ [0-9a-f] }    (uppercase normalized)
BranchName       := { String | git check_ref_format }       (no '..', no leading '-', no ws)
GitHubLogin      := { String | non-empty }                  (.is_bot() ⟺ ends_with("[bot]"))
TeamName         := { String | non-empty }                  (distinct namespace from logins)
Reviewer         := User(GitHubLogin) ⊕ Team(TeamName)      (symmetric sum, both arms validated)
CheckName        := { String | non-empty }
Timestamp        := chrono::DateTime⟨Utc⟩                   (Copy, Ord on instant)
BlockerKey       := { String | non-empty }                  (re-exported from ooda-core; identifies a gate, never a count)
CodexReasoningLevel := total enum { Low < Medium < High < Xhigh }
                    (as_str: "low" | "medium" | "high" | "xhigh" — the surface token at every boundary;
                     higher / lower : Option⟨Self⟩, None at the endpoints;
                     GateIdentity ⇒ usable as a BlockerKey::typed discriminator)
```

Urgency is defined in `ooda-core`, not `ids`:

```
Urgency  := Pre < Mid(MidTier) < Post                       (derived Ord — smallest sorts first)
MidTier  := Critical < Pathology < BlockingFix < BlockingWait
            < BlockingHuman < Advancing < Hygiene
```

`Pre` is reserved for first-time attestation (fires before any
other axis); `Post` is reserved for the closeout gate (fires only
when every other axis is silent). Everything else lives in
`Mid`.

### Composition law

Internal taxonomy (decide/runner/loop layer) is unchanged in shape;
the binary boundary (`Outcome`) re-encodes it as a single 1:1
variant→exit-code mapping for caller dispatch on `$?` alone.

```
Decision::exit_code()  ≡  match { Execute → 2, Halt(h) → h.exit_code() }    (internal, used by inspect)
HaltReason::exit_code()≡  match { Decision(h) → h.exit_code(),
                                  Stalled(_) → 6, CapReached(_) → 7 }      (internal, used by loop)
DecisionHalt::exit_code() ≡ match { Success | Terminal(Succeeded) → 0,
                                    Terminal(Aborted) → 5,
                                    HumanNeeded → 3, AgentNeeded → 4 }       (shared; the boundary
                                                                              re-maps Success → Paused = 1)
Outcome::exit_code()   ≡  see Outcome section below                          (boundary, 1:1)
```

Internal exit-code methods remain for unit-test ergonomics; the
binary itself dispatches via `Outcome::exit_code()` after collapsing
`LoopExit` (loop) or `Decision` (inspect) at the boundary.

---

## O — Observe

Boundary: `gh` subprocess (REST + GraphQL) plus local `git` → typed
Rust structs. Pure I/O.

```
fetch_all : RepoSlug × PullRequestNumber × Option⟨StateRoot⟩
          × Option⟨StickyPath⟩ × RepoRoot
          → Result⟨FetchOutcome, GhError⟩

FetchOutcome =
    Observations(GitHubObservations)
  ⊕ RateLimited(RateLimitHit)          ← lifted to Ok so decide sees it as data

GitHubObservations =
    pull_request_view      : PullRequestView
  × checks                 : Vec⟨PullRequestCheck⟩
  × reviews                : Vec⟨PullRequestReview⟩
  × review_threads_page    : ReviewThreadsResponse
  × issue_events           : Vec⟨IssueEvent⟩
  × issue_comments         : Vec⟨IssueComment⟩
  × requested_reviewers    : RequestedReviewers
  × branch_rules           : Vec⟨BranchRule⟩
  × branch_protection      : Option⟨BranchProtection⟩
  × stack_root_branch      : BranchName
  × copilot_config         : Option⟨CopilotCodeReviewParams⟩
  × rate_limit_budget      : RateLimitBudget
  × workflow_runs          : Vec⟨WorkflowRun⟩
  × unsigned_commits       : Vec⟨GitCommitSha⟩
  × cursor_status          : CursorStatus
  × merge_base_delta       : Option⟨MergeBaseDelta⟩
  × pull_request_metadata  : PullRequestMetadataObservation      (attestation file + HEAD)
  × doc_review             : DocReviewObservation                (attestation file + HEAD)
  × claude_review          : ClaudeReviewObservation             (attestation file + reviewer content)
  × closeout               : CloseoutObservation                 (attestation file + HEAD)
  × review_class           : ReviewClassObservation              (attestation file; threads joined in orient)
  × branch_sync            : BranchSyncObservation               (local git, not gh)
```

### Per-endpoint shapes (key fields)

```
PullRequestView     ::  state              : PullRequestState (Open ⊕ Terminal(Merged ⊕ Closed))
                    ::  is_draft           : Bool
                    ::  mergeable          : Mergeable (Mergeable ⊕ Conflicting ⊕ Unknown)
                    ::  merge_state_status : MergeStateStatus
                    ::  head_ref_oid       : GitCommitSha
                    ::  head_ref_name      : BranchName
                    ::  base_ref_name      : BranchName
                    ::  review_decision    : Option⟨ReviewDecision⟩
                    ::  labels, assignees, review_requests, commits, author
                    ::  updated_at, closed_at, merged_at
                    ::  ...

PullRequestCheck    ::  name         : CheckName
                    ::  state        : CheckState (12-variant enum + Unknown)
                    ::  completed_at : Option⟨Timestamp⟩

PullRequestReview   ::  user         : Option⟨ReviewUser{login: GitHubLogin}⟩
                    ::  state        : ReviewState
                    ::  commit_id    : GitCommitSha
                    ::  submitted_at : Option⟨Timestamp⟩

RequestedReviewer   ::  Bot{login: GitHubLogin}
                    ⊕   User{login: GitHubLogin}
                    ⊕   Team{name: TeamName}        ← validated at boundary
                    ⊕   Mannequin{login: GitHubLogin}

BranchRule          ::  rule_type : String
                    ::  parameters: Option⟨serde_json::Value⟩  (typed on demand)

BranchProtection    ::  required_status_checks           : Option⟨RequiredStatusChecks⟩
                    ::  required_conversation_resolution : Option⟨EnabledFlag⟩
                    ::  required_signatures              : Option⟨EnabledFlag⟩

WorkflowRun         ::  id : WorkflowRunId × name : String × head_sha : GitCommitSha × ...
CursorStatus        ::  suite : Option⟨CursorCheckSuite⟩ × run : Option⟨CursorCheckRun⟩

CopilotCodeReviewParams        ::  review_on_push, review_draft_pull_requests : Bool
RequiredStatusChecksParams     ::  required_status_checks : Vec⟨RequiredStatusCheck{context: CheckName, ...}⟩

<Axis>Observation   ::  attestation : Option⟨<Axis>Attestation{attested_sha, attested_at, version}⟩
                    ::  head_sha    : GitCommitSha
                    ::  attest_path : Option⟨PathBuf⟩          (None ⟺ no state root)
                    (pr-meta / doc-review additionally carry commits_behind : Option⟨ℕ⟩ — hint, not gate)

BranchSyncObservation :: divergence             : Option⟨BranchDivergence{from_sha, to_sha}⟩
                      :: branch_graphite_tracked : Bool
                      :: gt_available            : Bool

GhError =
    NotFound
  ⊕ ExitNonZero{code, stderr}
  ⊕ Json{stdout, error}
  ⊕ Spawn{io_error}
  ⊕ ...
```

**Concurrency:** fetchers fan out in parallel, joined before
return; first-error fail-fast, except rate-limit hits, which are
lifted into `FetchOutcome::RateLimited` so the runner can emit a
`WaitForRateLimit` action instead of a `BinaryError`. Terminal
short-circuit: `state ∈ {Merged, Closed}` skips auxiliary
endpoints whose base may have been deleted post-merge.

**Attestation reads never fail the pass.** A missing, malformed,
or schema-skewed attestation file collapses to absence; orient
classifies it as never-attested. Absence is a valid steady state.

### Codex review observation (axis-gated)

Boundary: local filesystem → typed Rust structs. Pure read; no
subprocess, no network. Runs after the GitHub fetch so the head
SHA and base branch it keys on are the ones just observed.

```
fetch_codex : CodexPrRoot × floor: CodexReasoningLevel × ceiling: CodexReasoningLevel
            × expected: u32 × head_sha: String
            → io::Result⟨CodexObservations⟩

CodexObservations =
    levels   : NonEmpty⟨CodexLevelObservation⟩   (ladder_slice(floor, ceiling); floor ≤ ceiling ⇒ non-empty)
  × expected : u32                               (= --codex-review-n)
  × head_sha : String
  × floor    : CodexReasoningLevel
  × ceiling  : CodexReasoningLevel

CodexLevelObservation =
    level       : CodexReasoningLevel
  × batch_state : BatchState
  × batch_dir   : PathBuf                        (= <codex_pr_root>/levels/<level>/<sha[:12]>/)

BatchState =
    NotStarted                                   (dir absent, no slot files, or identity gate fails)
  ⊕ Running { pending_slots: Vec⟨PendingSlot⟩, completed_verdicts: Vec⟨VerdictRecord⟩ }
  ⊕ Complete { verdicts: Vec⟨VerdictRecord⟩ }   (completed = expected)
  ⊕ InconsistentState { total, completed, expected: u32, reason: String }   (completed > expected)

PendingSlot   = slot: u32 × log_mtime: SystemTime × log_bytes: u64    (slot without `.exit`)
VerdictRecord = slot: u32 × body: String × class: VerdictClass
VerdictClass  = Clean ⊕ HasIssues ⊕ Indeterminate ⊕ Abandoned
                (Abandoned is never produced by classify — only by project_abandoning_pending)

ALIVE_THRESHOLD = 90s
```

Batch directory layout and the `scan_batch` reduction:

```
<batch_dir>/
    head_sha.txt          identity stamp — written LAST by the spawn path
    .batch.lock           per-batch advisory lock (shared by scan and spawn)
    <level>-<slot>.log    reviewer stdout+stderr, 1-indexed slot
    <level>-<slot>.exit   subprocess exit code; presence ⟺ subprocess terminated

scan_batch(dir, level, expected, Some(head_sha)) =
    dir absent                                   → NotStarted
    head_sha.txt absent ∨ content ≠ head_sha     → NotStarted           -- identity gate
    no <level>-*.{log,exit}                      → NotStarted
    .exit without matching .log                  → Err                  -- spawn-protocol violation
    slot with .exit: code ≠ 0                    → Err
    slot with .exit: no verdict marker ∨ empty   → Err
    slot without .exit                           → PendingSlot{mtime, bytes}
    completed < expected                         → Running
    completed = expected                         → Complete
    completed > expected                         → InconsistentState

extract_verdict : String → Option⟨String⟩       -- suffix after the LAST line that is exactly "codex"
                                                 -- Some("") ≠ None (streaming vs. not-yet-at-verdict)
classify : String → VerdictClass                 -- empty → Clean; structural issue marker → HasIssues;
                                                 -- clean-phrasing whitelist → Clean; else Indeterminate
                                                 -- structural check precedes prose check

project_abandoning_pending : CodexObservations × reason → CodexObservations
    Running{pending, completed} ↦ Complete{ completed ⊎ pending.map(Abandoned{reason}) }   (sorted by slot)
    other                      ↦ unchanged
```

**Identity gate is the whole invalidation mechanism.** A batch is
bound to the head SHA it was spawned against. A pushed fix changes
`head_ref_oid`, the per-batch stamp no longer matches, the scan
reports `NotStarted`, and the next iteration re-spawns. Prior-head
directories persist as cache and are never consulted.

---

## O — Orient

Boundary: typed observations → per-axis reports. Pure, no I/O.
The clock is read once per iteration and passed in so every axis
sees the same instant.

```
orient : GitHubObservations × Option⟨CodexObservations⟩ × Option⟨Timestamp⟩ × Timestamp → OrientedState

OrientedState =
    ci                        : CiReport                    (always-present)
  × state                     : PullRequestProjection       (always-present)
  × reviews                   : ReviewSummary               (always-present)
  × copilot                   : Option⟨CopilotReport⟩       (config-gated; None ⟺ no copilot ruleset)
  × cursor                    : Option⟨CursorReport⟩        (activity-gated; None ⟺ no rounds, no check)
  × threads                   : Vec⟨ReviewThread⟩
  × codex_review              : Option⟨CodexReviewReport⟩   (flag-gated; None ⟺ --codex-review-ceiling off)
  × merge_base_delta          : Option⟨MergeBaseDelta⟩
  × pull_request_metadata     : PullRequestMetadata         (attestation axis)
  × attest_path               : Option⟨PathBuf⟩
  × doc_review                : DocReview                   (attestation axis)
  × doc_review_attest_path    : Option⟨PathBuf⟩
  × claude_review             : ClaudeReview                (attestation axis, content-keyed)
  × claude_review_attest_path : Option⟨PathBuf⟩
  × closeout                  : Closeout                    (attestation axis, convergence gate)
  × closeout_attest_path      : Option⟨PathBuf⟩
  × review_class              : ReviewClass                 (attestation axis, content-keyed over every thread)
  × review_class_attest_path  : Option⟨PathBuf⟩
  × branch_sync               : BranchSyncObservation       (passed through untransformed)
```

**Asymmetric optionality is the soundness anchor.** Always-present
axes have empty/zero states; `Option`-gated axes structurally
distinguish _unconfigured_ from _configured-but-dormant_.
`codex_review = None` is structurally distinct from
`Some(report { status: LadderSatisfied })`.

### Per-axis algebra

```
CiReport =
    summary  : CiSummary
  × activity : CiActivity

CiSummary =
    required      : CheckBucket
  × missing_names : Vec⟨CheckName⟩
  × completed_at  : Option⟨Timestamp⟩
  × advisory      : CheckBucket

CheckBucket =  pass: ℕ × failed: Vec⟨FailedCheck⟩ × pending_names: Vec⟨CheckName⟩
FailedCheck =  name: CheckName × description: String × link: String

CiActivity  =  Idle ⊕ InFlight(Vec⟨PendingCheck⟩) ⊕ Resolved(ResolvedState)
CheckHealth =  Healthy ⊕ Degraded(Symptom) ⊕ Failed(Symptom)
               (per-check, per-HEAD rerun budget drives Degraded → Failed)

ReviewSummary =
    decision                        : Option⟨ReviewDecision⟩    (None ⟺ no policy)
  × threads_unresolved              : ℕ
  × threads_total                   : ℕ
  × bot_comments                    : ℕ
  × approvals_on_head               : ℕ
  × approvals_stale                 : ℕ
  × pending_reviews                 : PendingReviews
  × bot_reviews                     : Vec⟨BotReview⟩
  × requested_reviewers             : RequestedReviewerSet
  × latest_human_changes_requested  : Option⟨HumanReview⟩

PendingReviews = RequestedReviewerSet =
    bots   : Vec⟨GitHubLogin⟩    ← bots are always logins (structural invariant)
  × humans : Vec⟨Reviewer⟩       ← humans may be users OR teams (sum preserved)

PullRequestProjection =
    conflict           : Mergeable
  × draft              : Bool
  × wip                : Bool
  × title_len          : ℕ  × title_ok : Bool
  × body ⊕ summary ⊕ test_plan ⊕ content_label : Bool
  × assignees          : ℕ  × reviewers : ℕ
  × merge_when_ready   : Bool
  × commits            : ℕ
  × behind             : Bool
  × has_open_parent_pr : Bool
  × merge_state_status : MergeStateStatus
  × updated_at         : Timestamp
  × last_commit_at     : Option⟨Timestamp⟩

CopilotReport =
    config   : CopilotRepoConfig
  × activity : CopilotActivity
  × rounds   : Vec⟨CopilotReviewRound⟩
  × threads  : BotThreadSummary
  × tier     : CopilotTier
  × fresh    : Bool

CopilotActivity =
    Idle
  ⊕ Requested{requested_at: Timestamp, health: InFlightHealth}
  ⊕ Working{requested_at: Timestamp, ack_at: Timestamp, health: InFlightHealth}
  ⊕ Reviewed{latest: CopilotReviewRound}

CursorReport =
    activity         : CursorActivity
  × rounds           : Vec⟨CursorReviewRound⟩
  × threads          : BotThreadSummary
  × severity         : CursorSeverityBreakdown
  × tier             : CursorTier
  × fresh            : Bool
  × suite_created_at : Option⟨Timestamp⟩

CursorActivity =
    NotApplicable ⊕ Skipped(SkipReason) ⊕ InFlight(InFlightHealth) ⊕ Reviewed(ReviewedState)

CopilotTier  = Bronze ⊕ Silver ⊕ Gold ⊕ Platinum
CursorTier   = Bronze ⊕ Silver ⊕ Gold ⊕ Platinum
              (slug: &'static str — same vocab; types kept distinct
               to prevent accidental cross-bot comparison)

-- SHA-keyed attestation axes: drift is the SHA-inequality bit alone.
PullRequestMetadata = Synced ⊕ Drift{attested_sha, head_sha, commits_behind: Option⟨ℕ⟩} ⊕ NeverAttested
DocReview           = Synced ⊕ Drift{attested_sha, head_sha, commits_behind: Option⟨ℕ⟩} ⊕ NeverAttested
Closeout            = Synced ⊕ Drift{attested_sha, head_sha}                              ⊕ NeverAttested

-- Content-keyed attestation axis: the reviewer does not re-fire on
-- push, so the trigger is reviewer content newer than attested_at,
-- max'd across every surface the reviewer writes to.
ClaudeReview        = NoActivity
                    ⊕ Addressed
                    ⊕ Fresh{latest_claude_at, body_at, latest_claude_body,
                            latest_claude_url, inline_thread_count,
                            attested_at: Option⟨Timestamp⟩, head_sha}

-- Content-keyed over every review thread, all authors. Resolution
-- state is not the witness; the attestation must be newer than the
-- newest thread, and it carries the enumerated sites per class.
ReviewClass         = NoThreads
                    ⊕ Attested{attested_at, class_count}
                    ⊕ Fresh{latest_thread_at, fresh_thread_count,
                            prior: Option⟨ReviewClassAttestation⟩}

-- Codex review axis (ladder over [floor, ceiling]).
CodexReviewReport =
    status            : CodexReviewStatus
  × floor             : CodexReasoningLevel
  × ceiling           : CodexReasoningLevel
  × head_sha          : String
  × expected          : u32
  × current_batch_dir : PathBuf                  (batch_dir of current_level)
  × current_level     : CodexReasoningLevel      (first uncleared level; ceiling when satisfied)

CodexReviewStatus =
    Spawn        { level }                                          ← NotStarted
  ⊕ Await        { level, total: u32, completed: u32 }              ← Running with ≥ 1 alive slot
  ⊕ Address      { level, verdicts: Vec⟨VerdictRecord⟩ }            ← Complete with ≥ 1 non-Clean verdict
  ⊕ Inconsistent { level, reason: String }                          ← InconsistentState
  ⊕ LadderSatisfied                                                 ← every level Complete ∧ all Clean

orient_codex_review(obs) =
    current := first l ∈ obs.levels with ¬(l.batch_state = Complete{all Clean})
    case current of
        None → LadderSatisfied, anchored at obs.levels.last()
        Some(l) → status(discriminate_running(l.batch_state, now) ∨ l.batch_state)

discriminate_running(Running{pending, completed}, now) =
    if ∀s ∈ pending. now − s.log_mtime > ALIVE_THRESHOLD
        then Some(project_abandoning_pending)      -- every slot idle ⇒ synthetic Complete, pending ↦ Abandoned
        else None                                  -- any slot alive ⇒ stay Running
```

Ladder climbing is implicit: a clean `Complete` at level N makes
the next iteration's `current_level` N+1. No explicit advance
action exists. A cleared level never reverts within one head SHA.

---

## D — Decide

Boundary: `OrientedState × PullRequestState → Decision`. Pure, total.

```
decide : OrientedState × PullRequestState → Decision

Decision =
    Execute(Action)              (loop runs the action)
  ⊕ Halt(DecisionHalt)           (loop halts; outer driver consumes)

DecisionHalt ⊂ HaltReason                  ⟶ exit_code()   (internal; boundary maps Success → Paused = 1)
    Success                                 ⟶ 0
  ⊕ Terminal(Terminal)                      ⟶ 0 | 5
  ⊕ AgentNeeded(HandoffAction)              ⟶ 4
  ⊕ HumanNeeded(HandoffAction)              ⟶ 3

Terminal = Succeeded ⊕ Aborted

HandoffAction = Action with `effect` replaced by `prompt: HandoffPrompt`
                (the agent-vs-human distinction lives on the outer
                 variant; the inner shape does not carry it)
```

`DecisionHalt ⊂ HaltReason` is a strict subtype: render code matches
exhaustively over `DecisionHalt` and the compiler proves it cannot
witness loop-only halts (`Stalled`, `CapReached`).

### Action algebra

```
Action =
    kind          : ActionKind     (sum over 40 variants)
  × effect        : ActionEffect   (who runs it, fused with its payload)
  × target_effect : TargetEffect   (Blocks ⊕ Advances ⊕ Neutral)
  × urgency       : Urgency        (Pre ⊕ Mid(MidTier) ⊕ Post — total-ordered tier)
  × blocker       : BlockerKey     (stable stall-detection key — typed)

ActionEffect =
    Full{log: String, upstream: UpstreamConsistency}   (we run it)
  ⊕ Wait{interval: PollingInterval, log: String}       ("Wait without duration" unrepresentable)
  ⊕ Agent{prompt: HandoffPrompt}
  ⊕ Human{prompt: HandoffPrompt}

UpstreamConsistency =
    Sync                            (next observe sees the effect immediately)
  ⊕ Eventual(PollingInterval)       (next observe may still see pre-call state;
                                     the runner grants one propagation window
                                     before calling a repeat a stall)

StallKey = kind_name: &'static str × blocker: BlockerKey
           (payload-free — Action.stall_key() projects it)
```

### `ActionKind` taxonomy (40 variants — the funnel basins, all payloads typed)

```
                ┌─ FixCi{check_name: CheckName}
                ├─ WaitForCi{pending: NonEmpty⟨CheckName⟩}
                ├─ TriageWait{blocked_checks: NonEmpty⟨CheckName⟩}
        CI ─────┼─ ReRunWorkflow{checks: NonEmpty⟨DegradedCheck⟩}
                ├─ EscalateCiFailed{checks: NonEmpty⟨FailedCheckHandle⟩}
                ├─ EscalateCiStuck{stuck_runs: NonEmpty⟨WorkflowRunId⟩}
                └─ EscalateSigningRequired{unsigned_commits: NonEmpty⟨GitCommitSha⟩}

                ┌─ AddressThreads{threads: NonEmpty⟨ReviewThread⟩}
   Reviews ─────┼─ AddressChangeRequest
                └─ RequestApproval

   Mech.   ┌─ Rebase
   merge ──┼─ MarkReady       ─┬─ ShortenTitle{current_len: ℕ}
   block.  └─ RemoveWipLabel   └─ WaitForMergeability  ⊕ ResolveMergePolicy

   Hygiene ─── AddContentLabel ⊕ AddAssignee ⊕ AddDescription
              (computed, NOT emitted — domain-purity invariant)

   Bot tier ┌─ RerequestCopilot{symptom: Option⟨Symptom⟩}  ⊕ EscalateCopilotFailed{symptom: Symptom}
   advance  ├─ WaitForCopilotAck  ⊕ WaitForCopilotReview  ⊕ AddressCopilotSuppressed{count: ℕ}
            └─ WaitForCursorReview  ⊕ EscalateCursorStalled

   Pending  ┌─ WaitForBotReview{reviewers: NonEmpty⟨GitHubLogin⟩}    ← bots only
   review.  └─ WaitForHumanReview{reviewers: NonEmpty⟨Reviewer⟩}     ← user|team sum

   Rate     ─── WaitForRateLimit{scope: RateLimitScope}
   limit

   Codex    ┌─ RunCodexReviewBatch{level: CodexReasoningLevel, n: u32}            Full{Eventual(60s)}, Critical,     Advances
   review   ├─ AwaitCodexReviewBatch{level: CodexReasoningLevel, pending: u32}    Wait{30s},          BlockingWait, Blocks
   axis     ├─ AddressCodexReviewBatch{level: CodexReasoningLevel, count: u32}    Agent,              BlockingFix,  Blocks
            └─ CodexReviewBatchInconsistent{level: CodexReasoningLevel, reason}   Human,              Pathology,    Blocks

   Attest.  ┌─ SyncPullRequestMetadata{attest_path: PathBuf}   (SHA-keyed)
            ├─ ReviewDocs{attest_path: PathBuf}                (SHA-keyed)
            ├─ AddressClaudeReview{attest_path: PathBuf}       (content-keyed)
            ├─ AttestReviewClass{attest_path: PathBuf}         (content-keyed — sweep witness with enumerated sites)
            └─ Closeout{attest_path: PathBuf}                  (convergence gate — Urgency::Post)

   Branch   ┌─ SyncGraphiteStack{from_sha: String, to_sha: String}   (Full — `gt sync`)
   sync     └─ InvestigatePush{from_sha: String, to_sha: String}     (Agent handoff)
```

### Codex review axis: status → candidate

```
codex_review::candidates : CodexReviewReport → Vec⟨Action⟩       (|result| ≤ 1)
    LadderSatisfied            ↦ []
    Spawn{level}               ↦ [RunCodexReviewBatch{level, n: expected}]
    Await{level, total, done}  ↦ [AwaitCodexReviewBatch{level, pending: total − done}]
    Address{level, verdicts}   ↦ [AddressCodexReviewBatch{level, count: |issues|}]
                                  issues = verdicts.filter(class ∈ {HasIssues, Indeterminate, Abandoned})
                                  prompt.witnesses = issues.map(slot ↦ body)      (≥ 1 by precondition)
    Inconsistent{level, reason}↦ [CodexReviewBatchInconsistent{level, reason}]

blocker = BlockerKey::typed("codex_review_{runbatch|await|address|inconsistent}", level)
```

`AddressCodexReviewBatch` shares `BlockingFix` with
`AddressThreads` / `FixCi`; `AwaitCodexReviewBatch` shares
`BlockingWait` with `WaitForCi` / `WaitForCopilotReview`;
`RunCodexReviewBatch` is `Critical` and preempts every PR-side
wait. The `Address` prompt carries each flagged slot's verdict body
as a witness, so the dispatched agent needs no second filesystem
read.

### The `decide` predicate

Candidate assembly is per-axis through the `ooda_core::Axis` trait
(`runner.rs::drive`), with the codex axis called directly on the
`Option`-gated report; the halt predicate is the shared
`ooda_core::decide_from_candidates`.

```
drive : OrientedState × PullRequestNumber → Vec⟨Action⟩
  = StateAxis ⊎ CiAxis ⊎ ReviewsAxis ⊎ CopilotAxis ⊎ CursorAxis
    ⊎ codex_review::candidates(codex_review?)        -- ∅ when codex_review = None
    ⊎ PullRequestMetadataAxis ⊎ DocReviewAxis ⊎ ClaudeReviewAxis
    ⊎ CloseoutAxis ⊎ BranchSyncAxis
    ⊎ merge_eligibility_candidates ⊎ signing_eligibility_candidates
    |> yield_policy_to_actionable        -- drop `merge_blocked_policy` when any
                                         -- other candidate names a concrete gate
    |> sort by Urgency (stable)

decide(o, lifecycle) =
    case lifecycle of
        Terminal(Merged) → Halt(Terminal(Succeeded))
        Terminal(Closed) → Halt(Terminal(Aborted))
        Open → case drive(o) of
            []        → Halt(Success)
            top :: _  → classify(top)

classify(a) =
    case a.effect of
        Full | Wait → Execute(a)
        Agent       → Halt(AgentNeeded(a as HandoffAction))
        Human       → Halt(HumanNeeded(a as HandoffAction))
```

**Halt-as-predicate, not scalar.** No `score ≥ target` anywhere.
Empty candidate set ⟺ Success. With the codex axis enabled,
`Success` requires `LadderSatisfied` — the axis emits a candidate
in every other status.

**Verdict-by-absence yields to any concrete gate.** The
`merge_blocked_policy` fallback (GitHub reports `BLOCKED` and no
modeled axis explains it) is dropped whenever another candidate
exists, independent of tier. It survives only when it is the sole
signal.

---

## A — Act

Boundary: `Action × ActContext → Result⟨(), ActError⟩`. Side-effecting.

```
act : Action × ActContext → Result⟨(), ActError⟩

ActError =
    UnsupportedAutomation        (Agent / Human reached act, or Full kind with no handler — programmer error)
  ⊕ Gh(GhError)                  (subprocess failure on a Full action)
  ⊕ CodexDisabled                (codex kind reached act with ActContext.codex = None — programmer error)
  ⊕ CodexSpawn{slot: u32, source: io::Error}   (batch dir, lock, log, or spawn failure; slot 0 = batch-level)
  ⊕ Lock(io::Error)              (per-PR action lock could not be acquired)
  ⊕ GraphiteSync(String)         (`gt sync` exited non-zero / timed out / overflowed)

act(a, ctx) =
    case a.effect of
        Full{..}         → with FileLock(ctx.action_lock_path): run_full(a.kind, ctx)
        Wait{interval,..}→ thread::sleep(interval); Ok(())
        Agent | Human    → Err(UnsupportedAutomation)

run_full : ActionKind × ActContext → Result⟨(), ActError⟩
    MarkReady                     → gh pr ready
    RemoveWipLabel                → gh pr edit --remove-label
    RerequestCopilot              → gh api .../requested_reviewers POST
    ReRunWorkflow{checks}         → ∀c ∈ checks: gh api .../actions/runs/<c.run_id>/rerun   (fail-fast)
    RunCodexReviewBatch{level, n} → spawn_codex_review_batch(ctx.codex?, ctx.repo_root, level, n)
    SyncGraphiteStack             → gt sync   (cwd pinned to RepoRoot)
    _                             → Err(UnsupportedAutomation)   (no Full handler)
```

**Class invariant:** `decide` guarantees only `Full | Wait` reach
`act`; the `Agent | Human` arms are dead-by-construction (modulo
programmer error). The `UnsupportedAutomation` variant exists for
that bug class, not for runtime behavior.

**Action lock.** Every `Full` effect runs under an advisory
`FileLock` on the per-PR `.action.lock` sidecar so concurrent OODA
invocations against the same PR serialise their side effects.

**Repo-root pinning.** Every `gt` / `git` / `codex` subprocess runs
with `current_dir = RepoRoot`, resolved once at startup
(`--repo-root`, else `git rev-parse --show-toplevel` from CWD). A
caller invoking the binary from a sibling checkout cannot have
`gt sync` rewrite that sibling's stack or `codex review` diff the
wrong tree.

### Codex batch spawn

```
spawn_codex_review_batch(codex, repo_root, level, n) =
    dir := batch_dir(codex.codex_pr_root, level, codex.head_sha)
    secure_create_dir_all(dir)                                  -- 0o700
    with FileLock(dir/.batch.lock):                             -- excludes a concurrent scan
        preflight: codex_bin path-shaped ∧ ¬exists → Err(CodexSpawn{0})
        for slot in 1..=n:
            open_secure_truncate(dir/<level>-<slot>.log)        -- 0o600
            remove(dir/<level>-<slot>.exit) if present
            spawn /bin/sh -c 'umask 077; "$@" > $LOG 2>&1; code=$?; printf "%s\n" $code > $EXIT; exit $code'
                  -- <codex_bin> review --base <base_branch> -c model_reasoning_effort="<level>"
                  cwd = repo_root; stdio = null; detached reaper thread calls wait()
            on any Err: cleanup_partial_batch(dir, level, n); Err(CodexSpawn{slot})
        write dir/head_sha.txt := codex.head_sha                -- LAST
```

**`head_sha.txt` is written last.** `head_sha.txt present ⇒ every
slot's spawn returned Ok`. A partial spawn leaves the stamp absent,
the identity gate reports `NotStarted`, and the next iteration
re-emits `RunCodexReviewBatch`; truncate-on-entry makes the retry
idempotent. Completion is signalled by the `.exit` file the shell
wrapper writes after the child exits — never pre-created, so
`.exit present ⟺ subprocess terminated` holds for concurrent
scans.

---

## Runner / Loop

```
LoopConfig = max_iterations: NonZeroU32 × codex_review: Option⟨CodexReviewConfig⟩
IterStep   = Halt(HaltReason) ⊕ Executed(Action)

HaltReason =                                ⟶ exit_code()
    Decision(DecisionHalt)                  ⟶ delegate
  ⊕ Stalled(Action)                         ⟶ 6
  ⊕ CapReached(Action)                      ⟶ 7

run_iter(ctx, i, last_non_wait_key, auto_wait_used_for) =
    case fetch_all(...) of
        Err(e)              → Err(Observe(e))
        Ok(RateLimited(hit))→ act(WaitForRateLimit{hit.scope}, ctx);   Executed(that Wait)
        Ok(Observations(o)) →
            codex_obs := case (cfg.codex_review, ctx.codex) of
                (Some(cfg), Some(cx)) →
                    cx.head_sha    := o.pull_request_view.head_ref_oid     -- refresh before scan
                    cx.base_branch := o.pull_request_view.base_ref_name
                    Some(fetch_codex(cx.codex_pr_root, cfg.floor, cfg.ceiling, cx.n, cx.head_sha)?)
                                                                            -- Err → LoopError::CodexObserve
                _ → None
            oriented := orient(o, codex_obs, None, now())
            cands    := drive(oriented, pr)
            decision := decide_from_candidates(cands, o.pull_request_view.state)
            on_state(i, o, oriented, cands, decision)
            case decision of
                Halt(h)    → Halt(Decision(h))
                Execute(a) → case apply_stall_check(a, last_non_wait_key, auto_wait_used_for) of
                                 Halt(r)    → Halt(r)
                                 Proceed(a')→ act(a', ctx)?;  Executed(a')

apply_stall_check(a, prev, auto_waited) =
    k := a.stall_key()
    if prev ≠ Some(k)                    → Proceed(a)                       -- no repeat
    else if auto_waited = Some(k)        → Halt(Stalled(a))                 -- window already granted
    else if a.effect = Full{Eventual(d)} → Proceed(a with effect := Wait{d}) -- synthetic Wait
    else                                 → Halt(Stalled(a))                 -- Sync repeat is a stall

run_loop(ctx, cfg) =
    -- iteration 1 is unrolled: NonZeroU32 guarantees it runs and no comparator exists yet
    last_attempted     := (run_iter(ctx, 1, None, None) as Executed) or return Halted
    last_non_wait_key  := if last_attempted.effect.is_wait() then None else Some(key)
    auto_wait_used_for := None
    for i in 2..=cfg.max_iterations:
        if SHUTDOWN_SIGNAL set → return SignalInterrupted{130 | 143}
        case run_iter(ctx, i, last_non_wait_key, auto_wait_used_for) of
            Halt(r)     → return Halted(r)
            Executed(a) →
                k := a.stall_key()
                if a.is_wait ∧ last_non_wait_key = Some(k):      -- synthetic-Wait conversion seen
                    auto_wait_used_for := Some(k)
                else if ¬a.is_wait:
                    if auto_wait_used_for ≠ Some(k): auto_wait_used_for := None   -- real progress elsewhere
                    last_non_wait_key := Some(k)
                last_attempted := a
    -- cap-trip projection (codex axis only)
    if last_attempted.kind = AwaitCodexReviewBatch ∧ ctx.codex = Some(cx) ∧ cfg.codex_review = Some(cfg):
        obs'   := project_abandoning_pending(fetch_codex(cx, cfg), "iteration cap reached …")
        report := orient_codex_review(obs')
        case codex_review::candidates(report) of
            [a] → case classify(a) of
                      Halt(h)    → return Halted(Decision(h))          -- Address ⇒ HandoffAgent with partial verdicts
                      Execute(a) → return Halted(CapReached(a))
            []  → fall through
    return Halted(CapReached(last_attempted))
```

The loop is a Kleene iteration of `(observe ∘ orient ∘ decide ∘ act)*`
until `decide` halts, stall/cap fires, or a trapped signal is
observed at an iteration boundary.

**Stall detection compares `StallKey = (kind_name, blocker)`**, a
payload-free projection. Two `AddressThreads` actions with
different thread lists but the same blocker compare equal; a
mutating payload is not a second stability axis. Natural `Wait`
actions never seed the comparator — polling is _expected_ to
repeat — and a `Wait` whose key matches the pending non-`Wait` key
can only be the runner's own synthetic conversion.

**Eventual-consistency window.** A `Full` effect that declares
`Eventual(d)` is granted exactly one propagation window on its
first repeat: the runner converts it to a synthetic `Wait{d}`
instead of halting. A second repeat of the same key after that
window is a genuine stall. `Sync` effects get no window — a repeat
means orient re-derived the blocker after the effect should have
resolved it. `RunCodexReviewBatch` declares `Eventual(60s)`:
children are detached, and their `.exit` files land after the
spawn returns.

**Cap-trip projection.** When the iteration cap fires while the
last executed action was `AwaitCodexReviewBatch`, the runner
re-scans the batch (filesystem only), projects every `Running`
level to a synthetic `Complete` with pending slots `Abandoned`, and
routes the projected report through the codex decide. `Abandoned`
is non-clean, so the projection surfaces the landed verdicts as an
`Address` handoff and never claims a fixed point on a partial
sample.

`Stalled(Action)` carries the repeated action so the boundary can
emit `<ActionKind>:<BlockerKey>` for triage without re-deriving.

**Inspect mode** performs one observe → orient → decide pass with
`ctx.codex` unused: the codex scan runs when the ceiling is set
(read-only; a scan error is a `BinaryError`), but no batch is ever
spawned.

---

## Outcome — Binary Boundary

The internal `Decision`/`HaltReason`/`LoopExit`/`LoopError` split
is what `run_loop` and `decide` produce. Callers want **one**
variant per invocation with **one** exit code. `Outcome` is the
boundary type, defined in [`ooda-core`](../ooda-core/) generic over
a per-binary `ActionKind` and instantiated here via type alias.

```
Outcome =                                              ⟶ exit_code()  stderr
    DoneSucceeded                                      ⟶ 0            "DoneMerged"
  ⊕ Paused                                             ⟶ 1            "Paused"
  ⊕ WouldAdvance(Action)                               ⟶ 2            "WouldAdvance: ..."    (inspect-only)
  ⊕ HandoffHuman(HandoffAction)                        ⟶ 3            "Hand off to human: ..."
  ⊕ HandoffAgent(HandoffAction)                        ⟶ 4            "Hand off to agent: ..."
  ⊕ DoneAborted                                        ⟶ 5            "DoneClosed"
  ⊕ StuckRepeated(Action)                              ⟶ 6            "StuckRepeated: ..."
  ⊕ StuckCapReached(Action)                            ⟶ 7            "StuckCapReached: ..."
  ⊕ UsageError(SingleLineString)                       ⟶ 64           "UsageError: ..."
  ⊕ BinaryError(SingleLineString)                      ⟶ 70           "BinaryError: ..."
  ⊕ SignalInterrupted{exit_code: 130 | 143}            ⟶ 130 | 143    "Interrupted: exit code N"
```

**1:1 variant→exit-code.** Each variant has a unique code; `$?`
alone is sufficient for caller dispatch. Codes `8–63` and `65–69`
are deliberately unassigned. New error categories should adopt
the appropriate BSD `sysexits.h` code (`EX_IOERR = 74`,
`EX_TEMPFAIL = 75`, etc.) rather than squat on the low range.
`SignalInterrupted` is emitted by the binary itself when the loop
observes a trapped `SIGINT` / `SIGTERM` at an iteration boundary
(after appending the terminal `run_halted` event and releasing the
live marker); the shell synthesizes the same `128 + signal` for an
untrapped kill, so callers cannot distinguish the two paths on
`$?` alone.

`BinaryError` additionally covers the codex-specific failure
surface: a held `codex/.lock` at startup, a codex batch scan error
(`LoopError::CodexObserve`), and a spawn failure
(`ActError::CodexSpawn`).

### Boundary functors

```
From⟨HaltReason⟩ for Outcome    (loop mode):       [blanket impl in ooda-core]
    Decision(Success)                  → Paused
    Decision(Terminal(Succeeded))      → DoneSucceeded
    Decision(Terminal(Aborted))        → DoneAborted
    Decision(AgentNeeded(h))           → HandoffAgent(h)
    Decision(HumanNeeded(h))           → HandoffHuman(h)
    Stalled(a)                         → StuckRepeated(a)
    CapReached(action)                 → StuckCapReached(action)

LoopExit → Outcome              (loop mode):
    Halted(r)                          → From⟨HaltReason⟩ above
    SignalInterrupted{exit_code}       → SignalInterrupted{exit_code}

From⟨Decision⟩ for Outcome      (inspect mode):    [blanket impl in ooda-core]
    Execute(a)                         → WouldAdvance(a)        ← single substitution rule
    Halt(Success)                      → Paused                  ← all halts pass through
    Halt(Terminal(Succeeded))          → DoneSucceeded             via the same DecisionHalt
    Halt(Terminal(Aborted))            → DoneAborted               projection used in loop
    Halt(AgentNeeded(h))               → HandoffAgent(h)
    Halt(HumanNeeded(h))               → HandoffHuman(h)

From⟨LoopError⟩ for Outcome     (caught failures):
    e                                  → BinaryError(SingleLineString(e.to_string()))
                                         (newline-strip preserves single-line stderr header;
                                          CodexObserve renders as "observe (codex review): …")
```

`UsageError` is constructed directly by `parse_args` (failure path
returns `Result⟨Args, Outcome⟩` — the boundary always speaks
Outcome, no exception type).

### Stderr render contract

`render_outcome : &Outcome × Option⟨HandoffBlobPath⟩ → write to stderr`.
Each variant emits exactly one header line; `Handoff*` variants
additionally emit a single `see:` pointer to a content-addressed
handoff blob at `runs/<run-id>/blobs/<sha>.md`. See `SKILL.md` for
the per-variant header format.

```
header(Outcome) ::=                      ← left: variant; right: emitted stderr text
    DoneSucceeded                        "DoneMerged"
    StuckRepeated(a)                     "StuckRepeated: {a.kind.name()}:{a.blocker}"
    StuckCapReached(a)                   "StuckCapReached: {a.kind.name()}:{a.blocker}"
    HandoffHuman(h)                      "Hand off to human: {h.prompt.headline}"  + see-pointer
    WouldAdvance(a)                      "WouldAdvance: {a.kind.name()}:{format_effect(a.effect)}"
    HandoffAgent(h)                      "Hand off to agent: {h.prompt.headline}"  + see-pointer
    BinaryError(msg)                     "BinaryError: {msg}"
    Paused                               "Paused"
    DoneAborted                          "DoneClosed"
    UsageError(msg)                      "UsageError: {msg}" + usage text
    SignalInterrupted{code}              "Interrupted: exit code {code}"

format_effect ::= Full → "Full" | Wait{interval} → "Wait({interval})" | Agent → "Agent" | Human → "Human"
                  (only Full / Wait reach WouldAdvance; decide halts Agent / Human first)

see-pointer ::= "  see: {abs-path}"                        ← 7-byte prefix is contract
                                                            (path is a content-addressed
                                                             handoff blob at
                                                             runs/<run-id>/blobs/<sha>.md;
                                                             prompt body is in the file)
```

`ActionKind::name() : &'static str` returns the bare variant name
(no payload), so `<ActionKind>` placeholders in the stderr header
do not leak internal data shapes.

---

## Invariants worth naming

```
[H1] DecisionHalt ⊂ HaltReason
       Render code is structurally incapable of witnessing loop-only halts.

[H2] ∀h : HaltReason, ∀d : Decision.
       d.exit_code() = match d { Halt(h) → h.exit_code(); Execute → 2 }
       Single source of truth for the internal IPC encoding.

[H3] ∀o : Outcome. |{c : ℕ | ∃o', o'.exit_code() = c ∧ same_variant(o, o')}| = 1
       1:1 variant→exit-code at the binary boundary. $? alone is
       sufficient for caller dispatch; no two variants share a code.

[O1] OrientedState.copilot      = None  ⟺  no copilot ruleset configured
     OrientedState.cursor       = None  ⟺  no cursor activity observed
     OrientedState.codex_review = None  ⟺  --codex-review-ceiling off
       Absence of signal is structurally distinct from low signal.
       None ≠ Some(report { status: LadderSatisfied }).

[O2] ∀ SHA-keyed attestation axis. NeverAttested ≠ Drift ≠ Synced
       A never-recorded sign-off is its own state; collapsing it into
       drift would lose the "no one has ever signed off" signal.
       Drift is SHA-inequality alone — distance is a hint, never a gate.

[O3] batch_dir/head_sha.txt absent ∨ content ≠ head_ref_oid  ⇒  scan_batch = NotStarted
       Stale-batch invalidation is structural: a pushed fix changes the
       PR head, the stamp no longer matches, and the next iteration
       emits a fresh RunCodexReviewBatch at the current level.

[O4] slot completed  ⟺  .exit present ∧ verdict marker ∧ non-empty body
       .exit is the ground truth for "subprocess stopped"; a log without
       it is PendingSlot{mtime, bytes}, never a verdict.

[O5] Running ∧ ∀ pending slot idle > ALIVE_THRESHOLD  ⇒  orient projects Address with Abandoned slots
     Running ∧ ∃ pending slot alive                    ⇒  orient projects Await
       A slow-but-progressing slot is never abandoned mid-flight.

[D1] drive(o) = ∅  ⟺  Halt(Success)
       Halt is a predicate over the candidate set, not a scalar.

[D2] MidTier::Critical < every other tier; Urgency::Post > every Mid tier
       Unconditional forward progress preempts any blocking handoff;
       the closeout gate wins only on global quiescence. Full effects
       are NOT required to be Critical — ReRunWorkflow, RerequestCopilot,
       and SyncGraphiteStack sit at BlockingFix; RunCodexReviewBatch is
       the one Full effect at Critical.

[D3] merge_blocked_policy ∈ drive(o) ∧ |drive(o)| > 1  ⟹  it is dropped
       Verdict-by-absence never outranks a concrete gate.

[D4] |codex_review::candidates(r)| = [r.status ≠ LadderSatisfied]
       Exactly one candidate per unsatisfied status; the empty set is
       the axis's only release of the merge gate.

[A1] act receives only Action where effect ∈ {Full, Wait}
       decide already routed Agent/Human through Halt.

[A2] Every Full effect runs under the per-PR action FileLock.
       Concurrent invocations against one PR serialise their side effects.

[A3] head_sha.txt present  ⇒  every slot's spawn returned Ok
       The stamp is written last; a partial spawn leaves the identity
       gate closed and the next iteration retries from clean.

[R1] StallKey = (kind_name, blocker); payload is not part of the key.
       Two actions of the same kind with the same blocker are the same
       gate regardless of what their payloads carry.

[R2] runner seeds `last_non_wait_key` only from non-Wait actions.
       Polling (Wait) is expected to repeat; `Run(A), Wait, Run(A)` trips
       Stalled(A) for a Sync effect — the intervening Wait is invisible.

[R3] A Full{Eventual(d)} repeat is granted exactly one synthetic Wait{d}.
       The first repeat converts; the second repeat (after the window)
       halts. A different non-Wait key in between re-arms the window.

[R4] CapReached while last_attempted = AwaitCodexReviewBatch
       ⇒ landed verdicts surface through Address, never discarded.
       Abandoned ∉ Clean, so the cap-trip projection cannot satisfy the ladder.

[T1] ∀t1, t2 : Timestamp. t1 = t2  ⟺  t1.at() = t2.at()
       Timestamp Eq is on instant, not on bytes — surface forms collapse.
```
