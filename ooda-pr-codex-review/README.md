# `/ooda-pr-codex-review` — Type Algebra

Single-binary OODA loop for driving a PR through observe → orient →
decide → act until merge or external resolution, optionally running
a local `codex review` axis on the same tick.

This document is the **type-level specification**. For invocation
and exit-code taxonomy see `SKILL.md`. For implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder ⊕ outcome
        ⊕ ooda-core (sibling crate)

run_loop : ActContext × LoopConfig × Recorder × OnState
        → Result⟨HaltReason, LoopError⟩

main : Argv → Outcome → ExitCode
ExitCode = Outcome.exit_code()       (1:1 variant → code; see Outcome)
```

`recorder` is the always-on local memory harness. Keyed by
forge + repo + PR; writes under the configured state root.
Invariants it establishes:

- **Append-only causality**: events are written monotonically;
  readers truncate at the last good record.
- **Content-addressed write-once**: large artifacts are stored
  by hash, so a torn write is detectable and regeneratable.
- **Mutable pointer over immutable history**: a single
  atomically-replaced pointer file names the current
  per-iteration immutable directory; per-iteration artifacts
  never move once written. Agents enter via the pointer.

### Shared boundary types (`ooda-core` crate)

This binary depends on the sibling [`ooda-core`](../ooda-core/)
library crate for the cross-binary type spine: `Outcome`,
`Decision`, `DecisionHalt`, `HaltReason`, `Terminal`, `Action`,
`Automation`, `Urgency`, `TargetEffect`, `BlockerKey`, and the
`ActionKindName` trait. Each generic-over-`ActionKind` type is
instantiated locally via a type alias
(`pub type Outcome = ooda_core::Outcome<ActionKind>` and similar).
This binary's `ActionKind` is the merged PR + codex-review enum
(22 PR variants + 3 codex variants); `ReasoningLevel`, the codex
sub-tree types, and the unified-axis decide logic stay
per-binary.

**Variant name ≠ stderr header.** Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are neutral verbs from
`ooda-core`. Stderr headers emitted by this binary's
`render_outcome` are the PR-domain vocabulary (`DoneMerged`,
`DoneClosed`, `Paused`). Mapping shown in the Outcome section
below.

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
BlockerKey       := { String | non-empty }                  (parse: Result, tag: pub infallible; defined in ooda-core)
Urgency          := total enum { Critical < BlockingFix < BlockingWait
                                 < BlockingHuman < Advancing < Hygiene }
ReasoningLevel   := total enum { Low < Medium < High < Xhigh }  (codex review ladder rungs)
```

### Composition law

Internal taxonomy (decide/runner/loop layer) is unchanged in shape;
the binary boundary (`Outcome`) re-encodes it as a single 1:1
variant→exit-code mapping for caller dispatch on `$?` alone.

```
Decision::exit_code()  ≡  match { Execute → 2, Halt(h) → h.exit_code() }    (internal, used by inspect)
HaltReason::exit_code()≡  match { Decision(h) → h.exit_code(),
                                  Stalled(_) → 6, CapReached(_) → 7 }      (internal, used by loop)
DecisionHalt::exit_code() ≡ match { Success | Terminal → 0,
                                    HumanNeeded → 3, AgentNeeded → 4 }       (shared)
Outcome::exit_code()   ≡  see Outcome section below                          (boundary, 1:1)
```

Internal exit-code methods remain for unit-test ergonomics; the
binary itself dispatches via `Outcome::exit_code()` after collapsing
`HaltReason` (loop) or `Decision` (inspect) at the boundary.

---

## O — Observe

Boundary: `gh` subprocess (REST + GraphQL) → typed Rust structs. Pure I/O.

```
fetch_all : RepoSlug × PullRequestNumber → Result⟨GitHubObservations, GhError⟩

GitHubObservations =
    pull_request_view              : PullRequestView
  × checks               : Vec⟨PullRequestCheck⟩
  × reviews              : Vec⟨PullRequestReview⟩
  × review_threads_page  : ReviewThreadsResponse
  × issue_events         : Vec⟨IssueEvent⟩
  × issue_comments       : Vec⟨IssueComment⟩
  × requested_reviewers  : RequestedReviewers
  × branch_rules         : Vec⟨BranchRule⟩
  × branch_protection    : Option⟨BranchProtectionRequiredStatusChecks⟩
  × stack_root_branch    : BranchName
  × copilot_config       : Option⟨CopilotCodeReviewParams⟩
```

### Per-endpoint shapes (key fields)

```
PullRequestView     ::  state              : PullRequestState (Open ⊕ Closed ⊕ Merged)
                    ::  is_draft           : Bool
                    ::  mergeable          : Mergeable (Mergeable ⊕ Conflicting ⊕ Unknown)
                    ::  merge_state_status : MergeStateStatus
                    ::  head_ref_oid       : GitCommitSha
                    ::  base_ref_name      : BranchName
                    ::  review_decision    : Option⟨ReviewDecision⟩
                    ::  updated_at         : Timestamp
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

CopilotCodeReviewParams        ::  review_on_push, review_draft_pull_requests : Bool
RequiredStatusChecksParams     ::  required_status_checks : Vec⟨RequiredStatusCheck{context: CheckName, ...}⟩

GhError =
    NotFound
  ⊕ ExitNonZero{code, stderr}
  ⊕ Json{stdout, error}
  ⊕ Spawn{io_error}
  ⊕ ...
```

**Concurrency:** fetchers fan out in parallel, joined before
return; first-error fail-fast. Terminal short-circuit:
`state ∈ {Merged, Closed}` skips auxiliary endpoints whose base
may have been deleted post-merge.

### Codex review observation (axis-gated)

```
fetch_all : Path × ReasoningLevel × ReasoningLevel × u32 × HeadSha
          → io::Result⟨CodexObservations⟩

CodexObservations =
    levels   : Vec⟨CodexLevelObservation⟩       (ladder slice [floor, ceiling])
  × expected : u32                              (= --codex-review-n)
  × head_sha : String                           (current PR head)
  × floor    : ReasoningLevel
  × ceiling  : ReasoningLevel

CodexLevelObservation =
    level       : ReasoningLevel
  × batch_state : BatchState
  × batch_dir   : PathBuf                        (= <pr_root>/codex/levels/<L>/<sha[:12]>/)

BatchState =
    NotStarted                                   (no batch_dir, or head_sha.txt missing/mismatched)
  ⊕ Running { total: u32, completed: u32 }
  ⊕ Complete { verdicts: Vec⟨VerdictRecord⟩ }

VerdictRecord = slot: u32 × body: String × class: VerdictClass
VerdictClass  = Clean ⊕ HasIssues
```

Pure filesystem read. Head-SHA gate: `<batch_dir>/head_sha.txt`
content must match the PR's `pull_request_view.head_ref_oid`; mismatch or
absent file is reported as `NotStarted` so the runner re-spawns at
the current head.

---

## O — Orient

Boundary: typed observations → per-axis reports. Pure, no I/O.

```
orient : GitHubObservations × Option⟨CodexObservations⟩ × Option⟨Timestamp⟩ → OrientedState

OrientedState =
    ci           : CiSummary                  (always-present)
  × state        : PullRequestState           (always-present)
  × reviews      : ReviewSummary              (always-present)
  × copilot      : Option⟨CopilotReport⟩      (config-gated; None ⟺ no copilot ruleset)
  × cursor       : Option⟨CursorReport⟩       (activity-gated; None ⟺ no rounds, no check)
  × threads      : Vec⟨ReviewThread⟩          (always-present; empty ≡ none)
  × codex_review : Option⟨CodexReviewReport⟩  (flag-gated; None ⟺ --codex-review-ceiling off)
```

**Asymmetric optionality is the soundness anchor.** Always-present
axes have empty/zero states; `Option`-gated axes structurally
distinguish _unconfigured_ from _configured-but-dormant_. The old
combined-score approach conflated these and produced false halts.
`codex_review` extends the same pattern: `None` is structurally
distinct from "configured but ladder satisfied" (which is encoded
as `Some(CodexReviewReport { status: LadderSatisfied, .. })`).

### Per-axis algebra

```
CiSummary =
    required      : CheckBucket
  × missing_names : Vec⟨CheckName⟩
  × completed_at  : Option⟨Timestamp⟩
  × advisory      : CheckBucket

CheckBucket =  pass: ℕ × failed: Vec⟨FailedCheck⟩ × pending_names: Vec⟨CheckName⟩
FailedCheck =  name: CheckName × description: String × link: String

ReviewSummary =
    decision           : Option⟨ReviewDecision⟩    (None ⟺ no policy)
  × threads_unresolved : ℕ
  × threads_total      : ℕ
  × bot_comments       : ℕ
  × approvals_on_head  : ℕ
  × approvals_stale    : ℕ
  × pending_reviews    : PendingReviews
  × bot_reviews        : Vec⟨BotReview⟩

PendingReviews =
    bots   : Vec⟨GitHubLogin⟩    ← bots are always logins (structural invariant)
  × humans : Vec⟨Reviewer⟩       ← humans may be users OR teams (sum preserved)

PullRequestState =
    is_draft           : Bool
  × wip_label_present  : Bool
  × title_too_long     : Option⟨ℕ⟩
  × content_label      : Bool
  × assignees          : Vec⟨String⟩
  × mergeable          : Mergeable
  × merge_state_status : MergeStateStatus
  × ...

CopilotReport =
    config   : CopilotRepoConfig
  × activity : CopilotActivity
  × rounds   : Vec⟨CopilotReviewRound⟩
  × threads  : BotThreadSummary
  × tier     : CopilotTier
  × fresh    : Bool

CopilotActivity =
    Idle
  ⊕ Requested{requested_at: Timestamp}
  ⊕ Working{requested_at: Timestamp, ack_at: Timestamp}
  ⊕ Reviewed{latest: CopilotReviewRound}

CopilotTier  = Bronze ⊕ Silver ⊕ Gold ⊕ Platinum
CursorTier   = Bronze ⊕ Silver ⊕ Gold ⊕ Platinum
              (slug: &'static str — same vocab; types kept distinct
               to prevent accidental cross-bot comparison)

CursorReport ≅ CopilotReport (same skeleton, atomic-review state machine)

CodexReviewReport =
    status            : CodexReviewStatus
  × floor             : ReasoningLevel
  × ceiling           : ReasoningLevel
  × head_sha          : String
  × expected          : u32
  × current_batch_dir : PathBuf
  × current_level     : ReasoningLevel

CodexReviewStatus =
    Spawn   { level: ReasoningLevel }
  ⊕ Await   { level: ReasoningLevel, total: u32, completed: u32 }
  ⊕ Address { level: ReasoningLevel, verdicts: Vec⟨VerdictRecord⟩ }
  ⊕ LadderSatisfied                              (every level in [floor, ceiling] is Complete + all-clean)
```

`current_level` is the first level from the floor whose batch
isn't `Complete { all-clean }` for the current head SHA. Ladder
climbing is implicit — a clean batch at level N means the next
iteration's orient picks level N+1 without any explicit
"AdvanceLevel" action.

---

## D — Decide

Boundary: `OrientedState × PullRequestState → Decision`. Pure, total.

```
decide : OrientedState × PullRequestState → Decision

Decision =
    Execute(Action)              (loop runs the action)
  ⊕ Halt(DecisionHalt)           (loop halts; outer driver consumes)

DecisionHalt ⊂ HaltReason                  ⟶ exit_code()
    Success                                 ⟶ 0
  ⊕ Terminal(Terminal)                      ⟶ 0
  ⊕ AgentNeeded(Action)                     ⟶ 4
  ⊕ HumanNeeded(Action)                     ⟶ 3

Terminal = Merged ⊕ Closed
```

`DecisionHalt ⊂ HaltReason` is a strict subtype: render code matches
exhaustively over `DecisionHalt` and the compiler proves it cannot
witness loop-only halts (`Stalled`, `CapReached`).

### Action algebra

```
Action =
    kind          : ActionKind     (sum over 25 variants — 22 PR-side + 3 codex review)
  × automation    : Automation     (who runs it)
  × target_effect : TargetEffect   (Blocks ⊕ Advances ⊕ Neutral)
  × urgency       : Urgency        (total-ordered tier)
  × description   : String         (prompt material — display-only)
  × blocker       : BlockerKey     (stable stall-detection key — typed)

Automation =
    Full                            (we run it)
  ⊕ Wait{interval: Duration}        ("Wait without duration" unrepresentable)
  ⊕ Agent
  ⊕ Human
```

### `ActionKind` taxonomy (25 variants — the funnel basins, all payloads typed)

```
                ┌─ FixCi{check_name: CheckName}
        CI ─────┼─ WaitForCi{pending: Vec⟨CheckName⟩}
                └─ TriageWait{blocked_checks: Vec⟨CheckName⟩}

                ┌─ AddressThreads{count: ℕ}
   Reviews ─────┼─ AddressChangeRequest
                └─ RequestApproval

   Mech.   ┌─ Rebase
   merge ──┼─ MarkReady       ─┬─ ShortenTitle{current_len: ℕ}
   block.  └─ RemoveWipLabel   └─ WaitForMergeability  ⊕ ResolveMergePolicy

   Hygiene ─── AddContentLabel ⊕ AddAssignee ⊕ AddDescription
              (computed, NOT emitted — domain-purity invariant)

   Bot tier ┌─ RerequestCopilot          ⊕ AddressCopilotSuppressed{count: ℕ}
   advance  ├─ WaitForCopilotAck         ⊕ WaitForCopilotReview
            └─ WaitForCursorReview

   Pending  ┌─ WaitForBotReview{reviewers: Vec⟨GitHubLogin⟩}    ← bots only
   review.  └─ WaitForHumanReview{reviewers: Vec⟨Reviewer⟩}     ← user|team sum

   Codex    ┌─ RunCodexReviewBatch{level, n}                    Full,  Critical
   review   ├─ AwaitCodexReviewBatch{level, pending}            Wait,  BlockingWait
   axis     └─ AddressCodexReviewBatch{level, count}            Agent, BlockingFix
            (LadderSatisfied emits no candidate — axis goes silent
             once every level in [floor, ceiling] is clean)
```

**Codex review axis Urgency mapping rationale.**
`RunCodexReviewBatch` is `Critical`/`Full` so the axis preempts
PR-side waits whenever it has work to spawn — free progress beats
passive polling. `AwaitCodexReviewBatch` shares the `BlockingWait`
tier with `WaitForCi`/`WaitForCopilotReview`, so codex's poll
cycles serialize with the PR loop's other polls via the same
Urgency sort. `AddressCodexReviewBatch` shares `BlockingFix` with
`AddressThreads`/`FixCi` — codex issues get the same priority
class as PR review thread issues. The `Address` description bundles
the per-slot verdict bodies so the dispatched agent sees the
actual issues in its prompt without a second filesystem read.

### The `decide` predicate

```
candidates : OrientedState → Vec⟨Action⟩
  = state.blocking ⊎ ci ⊎ reviews ⊎ copilot? ⊎ cursor?
    |> if ¬∃ Blocks ∨ Advances : ⊎ state.fallback_merge_state_blocker
    |> sort by Urgency

decide(o, lifecycle) =
    case lifecycle of
        Merged → Halt(Terminal(Succeeded))
        Closed → Halt(Terminal(Aborted))
        Open → case candidates(o) of
            []        → Halt(Success)
            top :: _  → classify(top)

classify(a) =
    case a.automation of
        Full | Wait → Execute(a)
        Agent       → Halt(AgentNeeded(a))
        Human       → Halt(HumanNeeded(a))
```

**Halt-as-predicate, not scalar.** No `score ≥ target` anywhere.
Empty candidate set ⟺ Success.

---

## A — Act

Boundary: `Action × RepoSlug × PullRequestNumber → Result⟨(), ActError⟩`. Side-effecting.

```
act : Action × RepoSlug × PullRequestNumber → Result⟨(), ActError⟩

ActError =
    UnsupportedAutomation        (Agent / Human reached act — programmer error)
  ⊕ Gh(GhError)                  (subprocess failure on a Full action)

act(a, slug, pr) =
    case a.automation of
        Full           → run_full(a.kind, slug, pr)
        Wait{interval} → thread::sleep(interval); Ok(())
        Agent | Human  → Err(UnsupportedAutomation)

run_full : ActionKind × RepoSlug × PullRequestNumber → Result⟨(), ActError⟩
    MarkReady             → gh pr ready
    RemoveWipLabel        → gh pr edit --remove-label
    RerequestCopilot      → gh api .../requested_reviewers POST
    _                     → Err(UnsupportedAutomation)   (no Full handler)
```

**Class invariant:** `decide` guarantees only `Full | Wait` reach
`act`; the `Agent | Human` arms are dead-by-construction (modulo
programmer error). The `UnsupportedAutomation` variant exists for
that bug class, not for runtime behavior.

---

## Runner / Loop

```
LoopConfig = max_iterations: u32
LoopError  = Observe(GhError) ⊕ Act(ActError)

HaltReason =                                ⟶ exit_code()
    Decision(DecisionHalt)                  ⟶ delegate
  ⊕ Stalled(Action)                         ⟶ 6
  ⊕ CapReached(Action)                      ⟶ 7

run_loop(ctx, cfg, on_state) =
    last_non_wait := None       -- feeds the stall comparator
    last_attempted := None      -- feeds CapReached's diagnostic
    for i in 1..=cfg.max_iterations:
        obs       := fetch_all(ctx.slug, ctx.pr)?           -- LoopError::Observe
        codex_obs := if ctx.codex.is_some() && cfg.codex_review.is_some():
                         refresh ctx.codex.head_sha + base_branch from obs.pull_request_view
                         fetch_codex(...)?                  -- LoopError::CodexObserve
                     else: None
        oriented  := orient(obs, codex_obs, None)
        decision  := decide(oriented, obs.pull_request_view.state)
        on_state(i, oriented, decision)
        case decision of
            Halt(h)    → return Decision(h)
            Execute(a) → if same_action_repeated(last_non_wait, a) return Stalled(a)
                         act(a, ctx)?                       -- LoopError::Act
                         last_attempted := Some(a)
                         if a.automation ≠ Wait :  last_non_wait := Some(a)
    return CapReached(last_attempted.unwrap())     -- always Some when --max-iter ≥ 1

same_action_repeated(prev, cur) =
    -- prev is structurally non-Wait; no current=Wait gate needed.
    prev.exists(p ⟹ p.kind = cur.kind ∧ p.blocker = cur.blocker)
```

The loop is a Kleene iteration of `(observe ∘ orient ∘ decide ∘ act)*`
until either `decide` halts or stall/cap fires. Wait actions are
excluded from stall detection — polling is _expected_ to repeat.
`Stalled(Action)` carries the repeated action so the boundary can
emit `<ActionKind>:<BlockerKey>` for triage without re-deriving.

---

## Outcome — Binary Boundary

The internal `Decision`/`HaltReason`/`LoopError` split is what
`run_loop` and `decide` produce. Callers want **one** variant per
invocation with **one** exit code. `Outcome` is the boundary type.

```
Outcome =                                              ⟶ exit_code()  stderr
    DoneSucceeded                                      ⟶ 0            "DoneMerged"
  ⊕ Paused                                             ⟶ 1            "Paused"
  ⊕ WouldAdvance(Action)                               ⟶ 2            "WouldAdvance: ..."    (inspect-only)
  ⊕ HandoffHuman(Action)                               ⟶ 3            "Hand off to human: ..."
  ⊕ HandoffAgent(Action)                               ⟶ 4            "Hand off to agent: ..."
  ⊕ DoneAborted                                        ⟶ 5            "DoneClosed"
  ⊕ StuckRepeated(Action)                              ⟶ 6            "StuckRepeated: ..."
  ⊕ StuckCapReached(Action)                            ⟶ 7            "StuckCapReached: ..."
  ⊕ UsageError(String)                                 ⟶ 64           "UsageError: ..."
  ⊕ BinaryError(String)                                ⟶ 70           "BinaryError: ..."
  -- reserved: 130 (SIGINT), 143 (SIGTERM) — kernel-synthesized
```

**1:1 variant→exit-code.** Each variant has a unique code; `$?`
alone is sufficient for caller dispatch. Codes `8–63` and
`65–69` are deliberately unassigned. New error categories
should adopt the appropriate BSD `sysexits.h` code rather than
squat on the low range.

### Boundary functors

```
From⟨HaltReason⟩ for Outcome    (loop mode):       [blanket impl in ooda-core]
    Decision(Success)                  → Paused
    Decision(Terminal(Succeeded))      → DoneSucceeded
    Decision(Terminal(Aborted))        → DoneAborted
    Decision(AgentNeeded(a))           → HandoffAgent(a)
    Decision(HumanNeeded(a))           → HandoffHuman(a)
    Stalled(a)                         → StuckRepeated(a)
    CapReached(action)                 → StuckCapReached(action)

From⟨Decision⟩ for Outcome      (inspect mode):    [blanket impl in ooda-core]
    Execute(a)                         → WouldAdvance(a)        ← single substitution rule
    Halt(Success)                      → Paused                  ← all halts pass through
    Halt(Terminal(Succeeded))          → DoneSucceeded             via the same DecisionHalt
    Halt(Terminal(Aborted))            → DoneAborted               projection used in loop
    Halt(AgentNeeded(a))               → HandoffAgent(a)
    Halt(HumanNeeded(a))               → HandoffHuman(a)

From⟨LoopError⟩ for Outcome     (caught failures):
    e                                  → BinaryError(flatten_one_line(e.to_string()))
                                         (newline-strip preserves single-line stderr header)
```

`UsageError` is constructed directly by `parse_args` (failure path
returns `Result⟨Args, Outcome⟩` — the boundary always speaks
Outcome, no exception type).

### Stderr render contract

`render_outcome : &Outcome → write to stderr`. Each variant emits
exactly one header line; `Handoff*` variants additionally emit a
single `see:` pointer to the per-iteration `handoff.md` under
`runs/<run-id>/iterations/<NNNN>/`. See `SKILL.md` for the
per-variant header format.

```
header(Outcome) ::=                      ← left: variant; right: emitted stderr text
    DoneSucceeded                        "DoneMerged"
    StuckRepeated(a)                     "StuckRepeated: {a.kind.name()}:{a.blocker}"
    StuckCapReached(a)                   "StuckCapReached: {a.kind.name()}:{a.blocker}"
    HandoffHuman(a)                      "Hand off to human: {a.prompt.headline}"  + see-pointer
    WouldAdvance(a)                      "WouldAdvance: {a.kind.name()}:{format_automation(a.automation)}"
    HandoffAgent(a)                      "Hand off to agent: {a.prompt.headline}"  + see-pointer
    BinaryError(msg)                     "BinaryError: {msg}"
    Paused                               "Paused"
    DoneAborted                          "DoneClosed"
    UsageError(msg)                      "UsageError: {msg}" + usage text

see-pointer ::= "  see: {abs-path}"                        ← 7-byte prefix is contract
                                                            (path is the per-iteration
                                                             runs/<run-id>/iterations/<NNNN>/handoff.md;
                                                             also CURRENT.json's artifacts.handoff;
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
       The codex_review axis extends the same pattern: None ≠ "axis
       enabled but ladder satisfied" (which is encoded as
       Some(report { status: LadderSatisfied })).

[O2] CodexObservations.head_sha = ph
     ∧ batch_dir/head_sha.txt content ≠ ph
       ⇒ scan_batch reports NotStarted.
       Stale-batch invalidation is structural: a pushed fix changes
       the PR head, the per-batch stamp no longer matches, and the
       next iteration emits a fresh RunCodexReviewBatch.

[D1] candidates(o) = ∅  ⟺  Halt(Success)
       Halt is a predicate over the candidate set, not a scalar.

[D2] ∀a ∈ candidates. a.automation = Full ⇒ a.urgency = Critical
     ∀a ∈ candidates. urgency ordering ⇒ Full preempts Wait/Human
       Active fix-it-now beats passive handoff regardless of axis.

[A1] act receives only Action where automation ∈ {Full, Wait}
       decide already routed Agent/Human through Halt.

[R1] runner records only non-Wait actions in `last_non_wait`.
       Polling (Wait) is expected to repeat; stall detection sees
       only non-Wait actions, so `Run(A), Wait, Run(A)` correctly
       trips Stalled(A) — the intervening Wait is invisible.

[T1] ∀t1, t2 : Timestamp. t1 = t2  ⟺  t1.at() = t2.at()
       Timestamp Eq is on instant, not on bytes — surface forms collapse.
```
