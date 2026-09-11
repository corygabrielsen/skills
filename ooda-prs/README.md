# `/ooda-prs` — Type Algebra

Single-binary OODA loop for driving **N PRs in parallel** through
observe → orient → decide → act until each halts (merged, closed,
handed off, stuck, paused, or interrupted).

The per-PR pipeline is byte-identical to `/ooda-pr` on every
mirrored source file (enforced by `scripts/check-mirror-invariants.sh`).
The additions live at the **suite boundary**: multi-PR CLI grammar,
parallel spawn loop, `MultiOutcome` aggregate type, thread-local
recorder cell, JSONL stdout contract.

This document is the **type-level specification**. For invocation
and the operator-facing contract see `SKILL.md`. For implementation
see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder ⊕ dashboard
    ⊕ comment ⊕ signal ⊕ outcome ⊕ multi_outcome ⊕ suite
        ⊕ ooda-core ⊕ ooda-state (sibling crates)
        ⊕ ooda-attest (companion binary — writes the attestation files
                       the attestation axes observe)

Suite          = Vec⟨RepoSlug × PullRequestNumber⟩          (non-empty, distinct, input order)
ProcessOutcome = slug: RepoSlug × pr: PullRequestNumber × run_id: String × outcome: Outcome
MultiOutcome   = UsageError(SingleLineString) ⊕ Bundle(Vec⟨ProcessOutcome⟩)

drive_suite : Suite × Option⟨u32⟩ × (RepoSlug × PullRequestNumber → ProcessOutcome)
           → Vec⟨ProcessOutcome⟩          (parallel; returns in input order)

run_loop : RepoSlug × PullRequestNumber × Option⟨StateRoot⟩ × RepoRoot
         × LoopConfig × Recorder × OnState
         → Result⟨LoopExit, LoopError⟩   (per-PR; mirrored from /ooda-pr)

LoopExit  = Halted(HaltReason) ⊕ SignalInterrupted{exit_code: u8}
LoopError = Observe(GhError) ⊕ Act(ActError) ⊕ Recorder(RecorderError)

main : Argv → MultiOutcome → ExitCode
ExitCode = MultiOutcome.exit_code()       (priority projection; see MultiOutcome)
```

`recorder.rs` is a thin per-worker adapter over the shared
`ooda-state` crate. One `Recorder` per `(slug, pr)` worker; each
owns one `RunWriter` on a distinct `runs/<run-id>/` directory under
the shared state root. `RecorderError` is an alias for
`ooda_state::StateError`. The tool-call sink is a `thread_local!`
cell (`THREAD_RECORDER`), so worker _i_'s subprocess records cannot
land in worker _j_'s ledger. Invariants the underlying state model
establishes:

- **Append-only causality**: events appended to `events.jsonl`
  under `PIPE_BUF` are atomic w.r.t. concurrent readers.
- **Content-addressed write-once**: every payload is written via
  `tmp+rename` to `blobs/<sha>.<ext>`; identical bytes dedup.
- **Atomic live marker**: `live/<run-id>` is created via
  `O_CREAT|O_EXCL` at run start and `unlink`-ed at halt; presence
  is the source of truth for "active".
- **Domain-agnostic paths**: `runs/<run-id>/` carries no slug or PR
  number; PR identity lives only in the `RunStarted` target payload.

### Shared boundary types (`ooda-core` crate)

This binary depends on the sibling [`ooda-core`](../ooda-core/)
library crate for the per-PR cross-binary type spine: `Outcome`,
`Decision`, `DecisionHalt`, `HaltReason`, `Terminal`, `Action`,
`HandoffAction`, `ActionEffect`, `UpstreamConsistency`, `Urgency`,
`MidTier`, `TargetEffect`, `BlockerKey`, `StallKey`, `NonEmpty`,
`PollingInterval`, `HandoffPrompt`, `SingleLineString`, `ExitCode`,
the rate-limit types (`RateLimitBudget`, `RateLimitHit`,
`RateLimitScope`), the `attest` schema module, and the
`ActionKindName` trait. Each generic-over-`ActionKind` type is
instantiated locally via a type alias
(`pub type Outcome = ooda_core::Outcome<ActionKind>` and similar)
so call sites stay non-generic. The PR-domain `ActionKind` enum and
its `ActionKindName::name()` implementation stay in
`decide/action.rs`. Per-binary `LoopError`, `runner.rs`,
`recorder.rs`, `multi_outcome.rs`, and `suite.rs` are not lifted.
`MultiOutcome::exit_code()` returns the shared `ooda_core::ExitCode`
enum, so the numeric values live in one place.

**Variant name ≠ stderr / JSONL header.** Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are neutral verbs
defined in `ooda-core`. Stderr headers and the JSONL `outcome`
field both emit the PR-domain strings (`DoneMerged`, `DoneClosed`,
`Paused`) via the per-binary `render_outcome` and
`outcome_variant_name` functions. Mapping shown in the Outcome
section; callers dispatch on `$?` and read the stderr header or
JSONL field, not the variant name.

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
the per-PR boundary (`Outcome`) re-encodes it as a single 1:1
variant→exit-code mapping, and the suite boundary (`MultiOutcome`)
projects N of those onto one `$?`.

```
Decision::exit_code()  ≡  match { Execute → 2, Halt(h) → h.exit_code() }    (internal, used by inspect)
HaltReason::exit_code()≡  match { Decision(h) → h.exit_code(),
                                  Stalled(_) → 6, CapReached(_) → 7 }      (internal, used by loop)
DecisionHalt::exit_code() ≡ match { Success | Terminal(Succeeded) → 0,
                                    Terminal(Aborted) → 5,
                                    HumanNeeded → 3, AgentNeeded → 4 }       (shared; the boundary
                                                                              re-maps Success → Paused = 1)
Outcome::exit_code()   ≡  see Outcome section below                          (per-PR boundary, 1:1;
                                                                              JSONL `exit` field)
MultiOutcome::exit_code() ≡ see Suite Boundary below                         (process `$?`, projection)
```

Internal exit-code methods remain for unit-test ergonomics; each
worker collapses `LoopExit` (loop) or `Decision` (inspect) to an
`Outcome` at its boundary, and `main` collapses the bundle via
`MultiOutcome::exit_code()`.

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
Within one suite the lock is per-PR, so distinct workers never
contend.

**Repo-root pinning.** Every `gt` / `git` subprocess runs with
`current_dir = RepoRoot`, resolved once at startup (`--repo-root`,
else `git rev-parse --show-toplevel` from CWD). One path covers the
whole suite: `ooda-prs` drives many PRs but one local working tree.

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
observed at an iteration boundary. Every worker polls the same
process-wide shutdown atomic, so one `SIGTERM` lands on each
worker's next iteration boundary uniformly.

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

## Outcome — Per-PR Boundary

The internal `Decision`/`HaltReason`/`LoopExit`/`LoopError` split
is what `run_loop` and `decide` produce. Each worker collapses it
to **one** variant with **one** exit code. `Outcome` is the per-PR
boundary type, defined in [`ooda-core`](../ooda-core/) generic over
a per-binary `ActionKind` and instantiated here via type alias.

```
Outcome =                                              ⟶ exit_code()  stderr / JSONL `outcome`
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

**1:1 variant→exit-code per PR.** Each variant has a unique code;
the JSONL `exit` field carries it verbatim. Codes `8–63` and
`65–69` are deliberately unassigned. New error categories should
adopt the appropriate BSD `sysexits.h` code (`EX_IOERR = 74`,
`EX_TEMPFAIL = 75`, etc.) rather than squat on the low range.
`SignalInterrupted` is emitted by a worker when its loop observes
a trapped `SIGINT` / `SIGTERM` at an iteration boundary (after
appending the terminal `run_halted` event and releasing the live
marker); the shell synthesizes the same `128 + signal` for an
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

inspect ∘ RateLimited(hit)      (inspect mode):
    hit                                → WouldAdvance(WaitForRateLimit{hit.scope} as Wait{hit.retry_after})

From⟨LoopError⟩ for Outcome     (caught failures):
    e                                  → BinaryError(SingleLineString(e.to_string()))
                                         (newline-strip preserves single-line stderr header)

Recorder::open failure          (per worker, before the pipeline):
    e                                  → BinaryError("recorder: {e}")   with run_id = ""
```

`UsageError` is never produced by a worker: the parser and the
repo-root resolver fail before any PR is driven (see Suite
Boundary).

### Stderr render contract

`render_outcome : &Outcome × Option⟨HandoffBlobPath⟩ → write to stderr`.
Each variant emits exactly one header line; `Handoff*` variants
additionally emit one pointer line. With a recorder-written blob
the pointer is `see:` to the content-addressed handoff blob at
`runs/<run-id>/blobs/<sha>.md`; without one (recorder unavailable)
the prompt body is emitted inline under `prompt:`. See `SKILL.md`
for the per-variant header format.

```
header(Outcome) ::=                      ← left: variant; right: emitted stderr text
    DoneSucceeded                        "DoneMerged"
    StuckRepeated(a)                     "StuckRepeated: {a.kind.name()}:{a.blocker}"
    StuckCapReached(a)                   "StuckCapReached: {a.kind.name()}:{a.blocker}"
    HandoffHuman(h)                      "Hand off to human: {h.prompt.headline}"  + pointer
    WouldAdvance(a)                      "WouldAdvance: {a.kind.name()}:{format_effect(a.effect)}"
    HandoffAgent(h)                      "Hand off to agent: {h.prompt.headline}"  + pointer
    BinaryError(msg)                     "BinaryError: {msg}"
    Paused                               "Paused"
    DoneAborted                          "DoneClosed"
    UsageError(msg)                      "UsageError: {msg}" + usage text
    SignalInterrupted{code}              "Interrupted: exit code {code}"

format_effect ::= Full → "Full" | Wait{interval} → "Wait({interval})" | Agent → "Agent" | Human → "Human"
                  (only Full / Wait reach WouldAdvance; decide halts Agent / Human first)

pointer ::= "  see: {abs-path}"                            ← 7-byte prefix is contract
                                                            (content-addressed handoff blob at
                                                             runs/<run-id>/blobs/<sha>.md;
                                                             prompt body is in the file)
          | "  prompt: {body}"                              ← inline fallback, no recorder
```

`ActionKind::name() : &'static str` returns the bare variant name
(no payload), so `<ActionKind>` placeholders in the stderr header
do not leak internal data shapes.

**Stderr is not serialized across workers.** Per-iteration lines
and per-PR variant blocks from N threads interleave. Every advisory
line carries the loop-identity prefix
`[ooda-prs {slug}#{pr} run={run-id}]` (`run=` omitted when the
recorder is not yet open) so a stderr grep disambiguates each
worker. The authoritative per-PR record is the JSONL line on
stdout and the `runs/<run-id>/` audit trail, not stderr.

---

## Suite Boundary — `MultiOutcome`

The per-PR `Outcome` is one PR's boundary. `MultiOutcome` lifts it
to N PRs. Internal types unchanged; the suite boundary re-encodes
the bundle as a single aggregate exit code for shell dispatch, with
per-PR records flowing through stdout (JSONL).

```
ProcessOutcome =
    slug    : RepoSlug
  × pr      : PullRequestNumber
  × run_id  : String              (opaque ooda-state identifier; "" when Recorder::open failed)
  × outcome : Outcome

MultiOutcome =                                              ⟶ exit_code()
    UsageError(SingleLineString)                            ⟶ 64
  ⊕ Bundle(Vec⟨ProcessOutcome⟩)                             ⟶ priority projection
```

### Aggregate exit-code projection

```
Bundle(prs).exit_code() :=
    if ∃ p ∈ prs. p.outcome = SignalInterrupted{143}  → 143   (SIGTERM)
    else if ∃ p. p.outcome = SignalInterrupted{_}     → 130   (SIGINT)
    else if ∃ p. p.outcome = BinaryError(_)           → 70
    else if ∃ p. p.outcome = HandoffAgent(_)          → 4
    else if ∃ p. p.outcome = HandoffHuman(_)          → 3
    else if ∃ p. p.outcome = StuckCapReached(_)       → 7
    else if ∃ p. p.outcome = StuckRepeated(_)         → 6
    else if ∃ p. p.outcome = WouldAdvance(_)          → 2     (inspect-only)
    else if ∃ p. p.outcome = DoneAborted              → 5
    else (DoneSucceeded | Paused only, or empty)      → 0
```

Per-PR `UsageError` is unreachable inside a bundle: the parser
fails before any worker spawns. `Paused` (per-PR 1) folds into 0
at the suite level; `DoneAborted` keeps its per-PR 5. The numeric
values are the shared `ooda_core::ExitCode` enum, so the suite
projection and the per-PR `exit` field agree on every code they
both emit.

**Coarsening.** `/ooda-pr`'s 1:1 variant→exit gives single-byte
dispatch on one PR. At `|suite| > 1` the harness needs ≥ N bytes
of state (one per PR), so `$?` cannot encode it losslessly. The
split:

| Channel | Carries                                             | Granularity |
| ------- | --------------------------------------------------- | :---------: |
| `$?`    | priority projection — coarse class of work to do    |   1 byte    |
| stdout  | per-PR JSONL records — fine-grained per-PR Outcome  |   N lines   |
| stderr  | per-iteration logs + per-PR variant blocks (triage) |      —      |

### `main`

```
Args =
    mode           : Loop ⊕ Inspect
  × suite          : Suite
  × max_iter       : NonZeroU32           (default 50; ignored by inspect)
  × status_comment : Bool
  × state_root     : Option⟨PathBuf⟩      (must exist and be a directory at parse time)
  × repo_root      : Option⟨PathBuf⟩
  × concurrency    : Option⟨u32⟩          (≥ 1 when given; None ⟹ |suite|)

parse_args : Argv → Result⟨Args, SingleLineString⟩        (total; -h/--help exits 0 first)
parse_suite : Vec⟨String⟩ → Result⟨Suite, SingleLineString⟩

suite ::= group ( ',' group )*
group ::= slug? pr+
slug  ::= token containing '/'
pr    ::= token without '/'  (PullRequestNumber::parse)
-- slug resolution per group: own token, else prior group's slug, else `gh repo view` on cwd
-- duplicate (slug, pr) → UsageError; empty group → UsageError; slug with no pr → UsageError

resolve_repo_root : Option⟨PathBuf⟩ → Result⟨PathBuf, RepoRootError⟩
    Some(p) → canonicalize(p)
    None    → git rev-parse --show-toplevel   (cwd; bounded by SpawnLimits)

main(argv) =
    install_signal_handlers() or return render(BinaryError) ; 70
    case parse_args(argv) of
        Err(msg)  → render_outcome(Outcome::UsageError(msg)); MultiOutcome::UsageError(msg)
        Ok(args)  →
            repo_root := resolve_repo_root(args.repo_root)
                         or return render_outcome(Outcome::UsageError(e)); 64
            prs   := drive_suite(args.suite, args.concurrency,
                                 |slug, pr| drive_one_pull_request(slug, pr, args, repo_root))
            multi := Bundle(prs)
            render_multi_jsonl(stdout, multi)
            multi
    |> exit_code()
```

One diagnostic, two framings: the parser returns a bare
`SingleLineString`; `main` lifts it into `Outcome::UsageError`
(stderr header + usage text) and `MultiOutcome::UsageError`
(exit code) by direct construction. Repo-root resolution happens
once before fan-out so a misconfigured invocation surfaces as one
`UsageError`, not one per worker.

### Suite spawn loop (`suite::drive_suite`)

```
drive_suite(suite, concurrency, drive_one) =
    n         := |suite|
    if n = 0 : return []
    cap       := clamp(concurrency ?? n, 1, n)
    next      := AtomicUsize(0)
    results[i] := Mutex⟨Option⟨ProcessOutcome⟩⟩   for i ∈ [0, n)
    thread::scope |s|
      for w ∈ [0, cap): s.spawn(|| {
        loop:
          i := next.fetch_add(1, SeqCst)
          if i ≥ n : break
          results[i] := Some(drive_one(suite[i].slug, suite[i].pr))
      })
    return results |> unwrap each, in input order
```

**Rolling concurrency, not batching.** The atomic work index means
a finished PR releases its slot for the next: PR_3 can start before
PR_1 finishes if a worker slot is free. Each slot is written exactly
once by the worker that claimed its index; the `Mutex` is never
contended.

### Per-worker pipeline (`main::drive_one_pull_request`)

```
drive_one_pull_request(slug, pr, args, repo_root) =
    recorder := Recorder::open{slug, pr, mode, max_iter, status_comment, state_root}
                or return ProcessOutcome{slug, pr, run_id: "", BinaryError("recorder: …")}
    recorder.install_process_recorder()          -- thread-local tool-call sink
    outcome  := case args.mode of
                    Inspect → run_inspect(slug, pr, args, repo_root, recorder)
                    Loop    → run_full(slug, pr, args, repo_root, recorder)
                |> decorate_handoff_human(slug, pr, snapshot)
    handoff_path := case outcome of
                        HandoffAgent(h) | HandoffHuman(h) → recorder.write_handoff_md(h.prompt, …).ok()
                        _                                 → None
    render_outcome(stderr, outcome, handoff_path)
    recorder.record_outcome(outcome, outcome.exit_code(), headline, handoff_path)
    return ProcessOutcome{slug, pr, run_id: recorder.run_id(), outcome}
```

`decorate_handoff_human` appends a dashboard preamble to every
`Handoff*` prompt and, for `HandoffHuman` plus allowlisted
`HandoffAgent` kinds (`Rebase`), a trailing context block (PR URL,
blocker, branch, CI, reviews, closeout attestation). Non-handoff
variants pass through unchanged.

**Cross-thread isolation:**

- `THREAD_RECORDER` (in `recorder.rs`) is `thread_local!`, so each
  worker installs its own per-PR `Recorder` as the tool-call sink.
  No cross-PR aliasing.
- Per-PR `Recorder` is `Arc⟨Mutex⟨Inner⟩⟩`-backed; only one thread
  ever holds it. Each writes a distinct `runs/<run-id>/` directory.
- Per-PR `run_loop` state (`last_non_wait_key`,
  `auto_wait_used_for`, `last_attempted`) is on the worker's stack
  frame.
- The only shared mutable state is the atomic work index, the
  per-index result slots, and the process-wide shutdown atomic.

### Stdout JSONL contract

```
render_multi_jsonl : MultiOutcome → write to stdout
    UsageError(_)             → ε                              (no stdout)
    Bundle(prs)               → for p ∈ prs: writeln(record(p))   (input order)

record : ProcessOutcome → JSON object
    base := { slug            : "owner/repo",
              pr              : ℕ,
              pr_url          : "https://github.com/{slug}/pull/{pr}",
              run_id          : String,
              outcome         : outcome_variant_name(p.outcome),
              exit            : p.outcome.exit_code() }
    StuckRepeated(a) | StuckCapReached(a)  → base ⊎ { action: a.kind.name(), blocker: a.blocker }
    HandoffHuman(h)  | HandoffAgent(h)     → base ⊎ { action: h.kind.name(), blocker: h.blocker, prompt: h.prompt.to_string() }
    WouldAdvance(a)                        → base ⊎ { action: a.kind.name(), blocker: a.blocker, effect: format_effect(a.effect) }
    BinaryError(s) | UsageError(s)         → base ⊎ { msg: s }
    SignalInterrupted{exit_code}           → base ⊎ { signal_exit_code: exit_code }
    DoneSucceeded | DoneAborted | Paused   → base

outcome_variant_name ::=
    DoneSucceeded → "DoneMerged"   | DoneAborted → "DoneClosed"   | Paused → "Paused"
    every other variant → its Rust variant name
```

Field names are an external contract pinned by a per-variant
schema golden in `main.rs`; adding an `Outcome` variant fails
compilation of the golden until its record shape is declared.

---

## Invariants worth naming

```
[H1] DecisionHalt ⊂ HaltReason
       Render code is structurally incapable of witnessing loop-only halts.

[H2] ∀h : HaltReason, ∀d : Decision.
       d.exit_code() = match d { Halt(h) → h.exit_code(); Execute → 2 }
       Single source of truth for the internal IPC encoding.

[H3] ∀o : Outcome. |{c : ℕ | ∃o', o'.exit_code() = c ∧ same_variant(o, o')}| = 1
       1:1 variant→exit-code at the per-PR boundary; the JSONL `exit`
       field is injective over variants. The process `$?` is the
       MultiOutcome projection and is not injective.

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

[P1] ∀ (slug, pr) ∈ Suite. trajectory of run_loop(slug, pr, …) inside
     ooda-prs ≡ trajectory of /ooda-pr on the same (slug, pr).
       Per-PR semantic preservation. Every mirrored source file is
       byte-identical; run_loop is a function of its parameters.

[P2] ∀ distinct PR_i, PR_j. action_stream(PR_i) ⊥ action_stream(PR_j)
       Cross-thread isolation. THREAD_RECORDER is per-thread; per-PR
       Recorder is single-writer on its own runs/<run-id>/; run_loop
       state is on the worker stack. Shared mutable state is limited
       to the atomic work index, single-assignment result slots, and
       the shutdown atomic.

[P3] ooda-prs terminates ⇐  ∀ (slug, pr) ∈ Suite. run_loop(slug, pr) terminates.
       run_loop is bounded by --max-iter; thread::scope joins all
       workers before returning.

[P4] MultiOutcome::exit_code is total over MultiOutcome.
       UsageError → 64; empty Bundle → 0 (parser-unreachable but
       defined); the priority projection covers all 11 per-PR Outcome
       variants.

[P5] parse_args is total over Argv.
       Argv → Result⟨Args, SingleLineString⟩. Every input maps to
       exactly one of: valid Args, or a single-line diagnostic.
       Non-UTF-8 argv flows through OsString.

[P6] Recorder soundness:
       (a) Per-PR Recorder writes are single-writer per (slug, pr).
       (b) Each worker's runs/<run-id>/ is disjoint; no cross-worker
           subtree exists.
       (c) <run-id> is generated by ooda-state per open; simultaneous
           suite invocations on the same PR never share a run directory.
       (d) Recorder::open failure is a per-PR BinaryError with an empty
           run_id; the bundle still observes the failure via `$?` = 70.

[P7] Harness composability:
       case $? of
         0         → all DoneMerged/Paused — converged
         2         → ∃ WouldAdvance — inspect-only artifact
         3         → ∃ HandoffHuman — caller surfaces to human
         4         → ∃ HandoffAgent — caller dispatches sub-agents (parallel)
         5         → ∃ DoneClosed — closed without merge
         6 | 7     → ∃ Stuck* — caller escalates
         64        → UsageError — caller fixes invocation; stdout empty
         70        → ∃ BinaryError — caller escalates
         130 | 143 → ∃ SignalInterrupted — shutdown was requested
       Stdout JSONL records carry the per-PR detail for each branch.

[P8] No surviving counterexample. (Sweep:)
       (a) panic in PR_i: thread::scope propagates the panic on join;
           per-PR Recorders for completed siblings remain on disk.
       (b) upstream rate-limit cascade: --concurrency K bounds the
           request burst; default = |suite|. A rate-limit hit inside
           a worker becomes WaitForRateLimit, not BinaryError.
       (c) two invocations on overlapping PRs: distinct run identifiers
           per process; per-PR Full effects serialise on the action
           FileLock [A2]; per-run event logs never collide.
       (d) |suite| = 1: degenerate but valid; one stdout record;
           $? = that PR's per-PR exit code except Paused, which folds
           to 0 (the JSONL `exit` field still reads 1).
```
