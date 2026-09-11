# `/ooda-pr` — Type Algebra

Single-binary OODA loop for driving a PR through observe → orient →
decide → act until merge or external resolution.

This document is the **type-level specification**. For invocation
and exit-code taxonomy see `SKILL.md`. For implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder ⊕ dashboard
    ⊕ comment ⊕ signal ⊕ outcome
        ⊕ ooda-core ⊕ ooda-state (sibling crates)
        ⊕ ooda-attest (companion binary — writes the attestation files
                       the attestation axes observe)

run_loop : RepoSlug × PullRequestNumber × Option⟨StateRoot⟩ × RepoRoot
         × LoopConfig × Recorder × OnState
         → Result⟨LoopExit, LoopError⟩

LoopExit  = Halted(HaltReason) ⊕ SignalInterrupted{exit_code: u8}
LoopError = Observe(GhError) ⊕ Act(ActError) ⊕ Recorder(RecorderError)

main : Argv → Outcome → ExitCode
ExitCode = Outcome.exit_code()       (1:1 variant → code; see Outcome)
```

`recorder.rs` is a thin adapter over the shared `ooda-state` crate.
The PR-specific event vocabulary (`action_started`,
`status_comment_rendered`, `tool_call_finished`, …) lives here;
the generic on-disk layout (events.jsonl plus content-addressed
blobs) is owned by `ooda-state`. Invariants the underlying state
model establishes:

- **Append-only causality**: events appended to `events.jsonl`
  under `PIPE_BUF` are atomic w.r.t. concurrent readers.
- **Content-addressed write-once**: every payload is written via
  `tmp+rename` to `blobs/<sha>.<ext>`; identical bytes dedup.
- **Atomic live marker**: `live/<run-id>` is created via
  `O_CREAT|O_EXCL` at run start and `unlink`-ed at halt; presence
  is the source of truth for "active".

### Shared boundary types (`ooda-core` crate)

This binary depends on the sibling [`ooda-core`](../ooda-core/)
library crate for the cross-binary type spine: `Outcome`,
`Decision`, `DecisionHalt`, `HaltReason`, `Terminal`, `Action`,
`HandoffAction`, `ActionEffect`, `UpstreamConsistency`, `Urgency`,
`MidTier`, `TargetEffect`, `BlockerKey`, `StallKey`, `NonEmpty`,
`PollingInterval`, `HandoffPrompt`, the rate-limit types
(`RateLimitBudget`, `RateLimitHit`, `RateLimitScope`), the
`attest` schema module, and the `ActionKindName` trait. Each
generic-over-`ActionKind` type is instantiated locally via a type
alias (`pub type Outcome = ooda_core::Outcome<ActionKind>` and
similar) so call sites stay non-generic. The PR-domain
`ActionKind` enum and its `ActionKindName::name()` implementation
stay in `decide/action.rs`. Per-binary `LoopError`, `runner.rs`,
and `recorder.rs` are not lifted (see `ooda-core/README.md`).

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

---

## O — Orient

Boundary: typed observations → per-axis reports. Pure, no I/O.
The clock is read once per iteration and passed in so every axis
sees the same instant.

```
orient : GitHubObservations × Option⟨Timestamp⟩ × Timestamp → OrientedState

OrientedState =
    ci                        : CiReport                    (always-present)
  × state                     : PullRequestProjection       (always-present)
  × reviews                   : ReviewSummary               (always-present)
  × copilot                   : Option⟨CopilotReport⟩       (config-gated; None ⟺ no copilot ruleset)
  × cursor                    : Option⟨CursorReport⟩        (activity-gated; None ⟺ no rounds, no check)
  × threads                   : Vec⟨ReviewThread⟩
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
```

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
    kind          : ActionKind     (sum over 36 variants)
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

### `ActionKind` taxonomy (36 variants — the funnel basins, all payloads typed)

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

   Attest.  ┌─ SyncPullRequestMetadata{attest_path: PathBuf}   (SHA-keyed)
            ├─ ReviewDocs{attest_path: PathBuf}                (SHA-keyed)
            ├─ AddressClaudeReview{attest_path: PathBuf}       (content-keyed)
            ├─ AttestReviewClass{attest_path: PathBuf}         (content-keyed — sweep witness with enumerated sites)
            └─ Closeout{attest_path: PathBuf}                  (convergence gate — Urgency::Post)

   Branch   ┌─ SyncGraphiteStack{from_sha: String, to_sha: String}   (Full — `gt sync`)
   sync     └─ InvestigatePush{from_sha: String, to_sha: String}     (Agent handoff)
```

### The `decide` predicate

Candidate assembly is per-axis through the `ooda_core::Axis` trait
(`runner.rs::drive`); the halt predicate is the shared
`ooda_core::decide_from_candidates`.

```
drive : OrientedState × PullRequestNumber → Vec⟨Action⟩
  = StateAxis ⊎ CiAxis ⊎ ReviewsAxis ⊎ CopilotAxis ⊎ CursorAxis
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
Empty candidate set ⟺ Success.

**Verdict-by-absence yields to any concrete gate.** The
`merge_blocked_policy` fallback (GitHub reports `BLOCKED` and no
modeled axis explains it) is dropped whenever another candidate
exists, independent of tier. It survives only when it is the sole
signal.

---

## A — Act

Boundary: `Action × RepoSlug × PullRequestNumber × LockPath × RepoRoot → Result⟨(), ActError⟩`. Side-effecting.

```
act : Action × RepoSlug × PullRequestNumber × LockPath × RepoRoot → Result⟨(), ActError⟩

ActError =
    UnsupportedAutomation        (Agent / Human reached act, or Full kind with no handler — programmer error)
  ⊕ Gh(GhError)                  (subprocess failure on a Full action)
  ⊕ Lock(io::Error)              (per-PR action lock could not be acquired)
  ⊕ GraphiteSync(String)         (`gt sync` exited non-zero / timed out / overflowed)

act(a, slug, pr, lock, root) =
    case a.effect of
        Full{..}         → with FileLock(lock): run_full(a.kind, slug, pr, root)
        Wait{interval,..}→ thread::sleep(interval); Ok(())
        Agent | Human    → Err(UnsupportedAutomation)

run_full : ActionKind × RepoSlug × PullRequestNumber × RepoRoot → Result⟨(), ActError⟩
    MarkReady             → gh pr ready
    RemoveWipLabel        → gh pr edit --remove-label
    RerequestCopilot      → gh api .../requested_reviewers POST
    ReRunWorkflow{checks} → ∀c ∈ checks: gh api .../actions/runs/<c.run_id>/rerun   (fail-fast)
    SyncGraphiteStack     → gt sync   (cwd pinned to RepoRoot)
    _                     → Err(UnsupportedAutomation)   (no Full handler)
```

**Class invariant:** `decide` guarantees only `Full | Wait` reach
`act`; the `Agent | Human` arms are dead-by-construction (modulo
programmer error). The `UnsupportedAutomation` variant exists for
that bug class, not for runtime behavior.

**Action lock.** Every `Full` effect runs under an advisory
`FileLock` on the per-PR `.action.lock` sidecar so concurrent OODA
invocations against the same PR serialise their side effects.

**Repo-root pinning.** Every `gt` / `git` subprocess runs with
`current_dir = RepoRoot`, resolved once at startup (`--repo-root`,
else `git rev-parse --show-toplevel` from CWD). A caller invoking
the binary from a sibling checkout cannot have `gt sync` rewrite
that sibling's stack.

---

## Runner / Loop

```
LoopConfig = max_iterations: NonZeroU32
IterStep   = Halt(HaltReason) ⊕ Executed(Action)

HaltReason =                                ⟶ exit_code()
    Decision(DecisionHalt)                  ⟶ delegate
  ⊕ Stalled(Action)                         ⟶ 6
  ⊕ CapReached(Action)                      ⟶ 7

run_iter(i, last_non_wait_key, auto_wait_used_for) =
    case fetch_all(...) of
        Err(e)              → Err(Observe(e))
        Ok(RateLimited(hit))→ act(WaitForRateLimit{hit.scope}, ...);   Executed(that Wait)
        Ok(Observations(o)) →
            oriented := orient(o, None, now())
            cands    := drive(oriented, pr)
            decision := decide_from_candidates(cands, o.pull_request_view.state)
            on_state(i, o, oriented, cands, decision)
            case decision of
                Halt(h)    → Halt(Decision(h))
                Execute(a) → case apply_stall_check(a, last_non_wait_key, auto_wait_used_for) of
                                 Halt(r)    → Halt(r)
                                 Proceed(a')→ act(a', ...)?;  Executed(a')

apply_stall_check(a, prev, auto_waited) =
    k := a.stall_key()
    if prev ≠ Some(k)                    → Proceed(a)                       -- no repeat
    else if auto_waited = Some(k)        → Halt(Stalled(a))                 -- window already granted
    else if a.effect = Full{Eventual(d)} → Proceed(a with effect := Wait{d}) -- synthetic Wait
    else                                 → Halt(Stalled(a))                 -- Sync repeat is a stall

run_loop(cfg) =
    -- iteration 1 is unrolled: NonZeroU32 guarantees it runs and no comparator exists yet
    last_attempted     := (run_iter(1, None, None) as Executed) or return Halted
    last_non_wait_key  := if last_attempted.effect.is_wait() then None else Some(key)
    auto_wait_used_for := None
    for i in 2..=cfg.max_iterations:
        if SHUTDOWN_SIGNAL set → return SignalInterrupted{130 | 143}
        case run_iter(i, last_non_wait_key, auto_wait_used_for) of
            Halt(r)     → return Halted(r)
            Executed(a) →
                k := a.stall_key()
                if a.is_wait ∧ last_non_wait_key = Some(k):      -- synthetic-Wait conversion seen
                    auto_wait_used_for := Some(k)
                else if ¬a.is_wait:
                    if auto_wait_used_for ≠ Some(k): auto_wait_used_for := None   -- real progress elsewhere
                    last_non_wait_key := Some(k)
                last_attempted := a
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
resolved it.

`Stalled(Action)` carries the repeated action so the boundary can
emit `<ActionKind>:<BlockerKey>` for triage without re-deriving.

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
                                         (newline-strip preserves single-line stderr header)
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

[O1] OrientedState.copilot = None  ⟺  no copilot ruleset configured
     OrientedState.cursor  = None  ⟺  no cursor activity observed
       Absence of signal is structurally distinct from low signal.

[O2] ∀ SHA-keyed attestation axis. NeverAttested ≠ Drift ≠ Synced
       A never-recorded sign-off is its own state; collapsing it into
       drift would lose the "no one has ever signed off" signal.
       Drift is SHA-inequality alone — distance is a hint, never a gate.

[D1] drive(o) = ∅  ⟺  Halt(Success)
       Halt is a predicate over the candidate set, not a scalar.

[D2] MidTier::Critical < every other tier; Urgency::Post > every Mid tier
       Unconditional forward progress preempts any blocking handoff;
       the closeout gate wins only on global quiescence. Full effects
       are NOT required to be Critical — ReRunWorkflow, RerequestCopilot,
       and SyncGraphiteStack sit at BlockingFix.

[D3] merge_blocked_policy ∈ drive(o) ∧ |drive(o)| > 1  ⟹  it is dropped
       Verdict-by-absence never outranks a concrete gate.

[A1] act receives only Action where effect ∈ {Full, Wait}
       decide already routed Agent/Human through Halt.

[A2] Every Full effect runs under the per-PR action FileLock.
       Concurrent invocations against one PR serialise their side effects.

[R1] StallKey = (kind_name, blocker); payload is not part of the key.
       Two actions of the same kind with the same blocker are the same
       gate regardless of what their payloads carry.

[R2] runner seeds `last_non_wait_key` only from non-Wait actions.
       Polling (Wait) is expected to repeat; `Run(A), Wait, Run(A)` trips
       Stalled(A) for a Sync effect — the intervening Wait is invisible.

[R3] A Full{Eventual(d)} repeat is granted exactly one synthetic Wait{d}.
       The first repeat converts; the second repeat (after the window)
       halts. A different non-Wait key in between re-arms the window.

[T1] ∀t1, t2 : Timestamp. t1 = t2  ⟺  t1.at() = t2.at()
       Timestamp Eq is on instant, not on bytes — surface forms collapse.
```
