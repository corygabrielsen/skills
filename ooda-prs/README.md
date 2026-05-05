# `/ooda-prs` — Type Algebra

Single-binary OODA loop for driving **N PRs in parallel** through
observe → orient → decide → act until each halts (merged, closed,
handed off, stuck, or paused).

`/ooda-prs` is a fork of `/ooda-pr`. The per-PR pipeline is
bit-equivalent; the additions live at the **suite boundary**:
multi-PR CLI grammar, parallel spawn loop, `MultiOutcome` aggregate
type, suite-level `Recorder`, JSONL stdout contract.

This document is the **type-level specification**. For invocation
and the operator-facing contract see `SKILL.md`. For implementation
see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder
   ⊕ outcome ⊕ multi_outcome ⊕ suite ⊕ suite_recorder

Suite          = Vec⟨RepoSlug × PullRequestNumber⟩          (non-empty, distinct)
ProcessOutcome = (RepoSlug, PullRequestNumber, Outcome)
MultiOutcome   = UsageError(String) ⊕ Bundle(Vec⟨ProcessOutcome⟩)

drive_suite : Suite × Option⟨u32⟩ × (RepoSlug × PullRequestNumber → Outcome)
           → Vec⟨ProcessOutcome⟩          (parallel; returns in input order)

run_loop : RepoSlug × PullRequestNumber × LoopConfig × Recorder × OnState
        → Result⟨HaltReason, LoopError⟩   (per-PR, unchanged from /ooda-pr)

main : Argv → MultiOutcome → ExitCode
ExitCode = MultiOutcome.exit_code()       (priority projection; see MultiOutcome)
```

`recorder` is the always-on per-PR memory harness (keyed by forge

- repo + PR; identical to `/ooda-pr`). `suite_recorder` is the
  new suite-level harness, keyed by `<suite-id>` per invocation.
  The two are independent and coexist on the same state-root tree.

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
BlockerKey       := { String | non-empty }                  (parse: Result, tag: pub(crate) infallible)
Urgency          := total enum { Critical < BlockingFix < BlockingWait
                                 < BlockingHuman < Advancing < Hygiene }
```

### Composition law

Internal taxonomy (decide/runner/loop layer) is unchanged in shape;
the binary boundary (`Outcome`) re-encodes it as a single 1:1
variant→exit-code mapping for caller dispatch on `$?` alone.

```
Decision::exit_code()  ≡  match { Execute → 4, Halt(h) → h.exit_code() }    (internal, used by inspect)
HaltReason::exit_code()≡  match { Decision(h) → h.exit_code(),
                                  Stalled(_) → 1, CapReached(_) → 2 }      (internal, used by loop)
DecisionHalt::exit_code() ≡ match { Success | Terminal → 0,
                                    HumanNeeded → 3, AgentNeeded → 5 }       (shared)
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
    pr_view              : PullRequestView
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
PullRequestView     ::  state              : PrState (Open ⊕ Closed ⊕ Merged)
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

**Concurrency:** nine fetchers fan out under `thread::scope`;
first-error fail-fast. Terminal short-circuit:
`state ∈ {Merged, Closed} → terminal_observations(pr_view)` skips
auxiliary endpoints whose base may have been deleted post-merge.

---

## O — Orient

Boundary: typed observations → per-axis reports. Pure, no I/O.

```
orient : GitHubObservations × Option⟨Timestamp⟩ → OrientedState

OrientedState =
    ci       : CiSummary                   (always-present)
  × state    : PullRequestState            (always-present)
  × reviews  : ReviewSummary               (always-present)
  × copilot  : Option⟨CopilotReport⟩       (config-gated; None ⟺ no copilot ruleset)
  × cursor   : Option⟨CursorReport⟩        (activity-gated; None ⟺ no rounds, no check)
```

**Asymmetric optionality is the soundness anchor.** Always-present
axes have empty/zero states; `Option`-gated axes structurally
distinguish _unconfigured_ from _configured-but-dormant_. The old
combined-score approach conflated these and produced false halts.

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
```

---

## D — Decide

Boundary: `OrientedState × PrState → Decision`. Pure, total.

```
decide : OrientedState × PrState → Decision

Decision =
    Execute(Action)              (loop runs the action)
  ⊕ Halt(DecisionHalt)           (loop halts; outer driver consumes)

DecisionHalt ⊂ HaltReason                  ⟶ exit_code()
    Success                                 ⟶ 0
  ⊕ Terminal(Terminal)                      ⟶ 0
  ⊕ AgentNeeded(Action)                     ⟶ 5
  ⊕ HumanNeeded(Action)                     ⟶ 3

Terminal = Merged ⊕ Closed
```

`DecisionHalt ⊂ HaltReason` is a strict subtype: render code matches
exhaustively over `DecisionHalt` and the compiler proves it cannot
witness loop-only halts (`Stalled`, `CapReached`).

### Action algebra

```
Action =
    kind          : ActionKind     (sum over 22 variants)
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

### `ActionKind` taxonomy (22 variants — the funnel basins, all payloads typed)

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
```

### The `decide` predicate

```
candidates : OrientedState → Vec⟨Action⟩
  = state.blocking ⊎ ci ⊎ reviews ⊎ copilot? ⊎ cursor?
    |> if ¬∃ Blocks ∨ Advances : ⊎ state.fallback_merge_state_blocker
    |> sort by Urgency

decide(o, lifecycle) =
    case lifecycle of
        Merged → Halt(Terminal(Merged))
        Closed → Halt(Terminal(Closed))
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
  ⊕ Stalled(Action)                         ⟶ 1
  ⊕ CapReached(Action)                      ⟶ 2

run_loop(slug, pr, cfg, on_state) =
    last_non_wait := None       -- feeds the stall comparator
    last_attempted := None      -- feeds CapReached's diagnostic
    for i in 1..=cfg.max_iterations:
        obs      := fetch_all(slug, pr)?              -- LoopError::Observe
        oriented := orient(obs, None)
        decision := decide(oriented, obs.pr_view.state)
        on_state(i, oriented, decision)
        case decision of
            Halt(h)    → return Decision(h)
            Execute(a) → if same_action_repeated(last_non_wait, a) return Stalled(a)
                         act(a, slug, pr)?            -- LoopError::Act
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
Outcome =                                              ⟶ exit_code()
    DoneMerged                                         ⟶ 0
  ⊕ StuckRepeated(Action)                              ⟶ 1
  ⊕ StuckCapReached(Action)                            ⟶ 2
  ⊕ HandoffHuman(Action)                               ⟶ 3
  ⊕ WouldAdvance(Action)                               ⟶ 4    (inspect-only)
  ⊕ HandoffAgent(Action)                               ⟶ 5
  ⊕ BinaryError(String)                                ⟶ 6
  ⊕ Paused                                             ⟶ 7
  ⊕ DoneClosed                                         ⟶ 8
  ⊕ UsageError(String)                                 ⟶ 64
```

**1:1 variant→exit-code.** Each variant has a unique code; `$?`
alone is sufficient for caller dispatch. Codes 9–63 are reserved
for future variants; codes ≥64 follow BSD `sysexits` starting at
`UsageError = 64`.

### Boundary functors

```
From⟨HaltReason⟩ for Outcome    (loop mode):
    Decision(Success)                  → Paused
    Decision(Terminal(Merged))         → DoneMerged
    Decision(Terminal(Closed))         → DoneClosed
    Decision(AgentNeeded(a))           → HandoffAgent(a)
    Decision(HumanNeeded(a))           → HandoffHuman(a)
    Stalled(a)                         → StuckRepeated(a)
    CapReached(action)                 → StuckCapReached(action)

From⟨Decision⟩ for Outcome      (inspect mode):
    Execute(a)                         → WouldAdvance(a)        ← single substitution rule
    Halt(Success)                      → Paused                  ← all halts pass through
    Halt(Terminal(Merged))             → DoneMerged                via the same DecisionHalt
    Halt(Terminal(Closed))             → DoneClosed                projection used in loop
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
prompt block. See `SKILL.md` for the per-variant header format.

```
header(Outcome) ::=
    DoneMerged                           "DoneMerged"
    StuckRepeated(a)                     "StuckRepeated: {a.kind.name()}:{a.blocker}"
    StuckCapReached(a)                   "StuckCapReached: {a.kind.name()}:{a.blocker}"
    HandoffHuman(a)                      "HandoffHuman: {a.kind.name()}"  + prompt block
    WouldAdvance(a)                      "WouldAdvance: {a.kind.name()}:{format_automation(a.automation)}"
    HandoffAgent(a)                      "HandoffAgent: {a.kind.name()}"  + prompt block
    BinaryError(msg)                     "BinaryError: {msg}"
    Paused                               "Paused"
    DoneClosed                           "DoneClosed"
    UsageError(msg)                      "UsageError: {msg}" + usage text

prompt block ::= "  prompt: {a.description}"      ← 10-byte prefix is contract
```

`ActionKind::name() : &'static str` returns the bare variant name
(no payload), so `<ActionKind>` placeholders in the stderr header
do not leak internal data shapes.

---

## Suite Boundary — `MultiOutcome`

The per-PR `Outcome` is one PR's binary boundary. `MultiOutcome`
lifts it to N PRs. Internal types unchanged; the suite boundary
re-encodes the bundle as a single aggregate exit code for shell
dispatch, with per-PR records flowing through stdout (JSONL) and
the suite-level `SuiteRecorder` directory.

```
ProcessOutcome = (RepoSlug, PullRequestNumber, Outcome)

MultiOutcome =                                              ⟶ exit_code()
    UsageError(String)                                      ⟶ 64
  ⊕ Bundle(Vec⟨ProcessOutcome⟩)                             ⟶ priority projection
```

### Aggregate exit-code projection

```
Bundle(prs).exit_code() :=
    if ∃ p ∈ prs. p.outcome = BinaryError(_)        → 6
    else if ∃ p ∈ prs. p.outcome = HandoffAgent(_)  → 5
    else if ∃ p ∈ prs. p.outcome = HandoffHuman(_)  → 3
    else if ∃ p ∈ prs. p.outcome = StuckCapReached  → 2
    else if ∃ p ∈ prs. p.outcome = StuckRepeated    → 1
    else if ∃ p ∈ prs. p.outcome = WouldAdvance     → 4
    else (DoneMerged | DoneClosed | Paused only)    → 0
```

**Coarsening trade-off.** `/ooda-pr`'s 1:1 variant→exit gives
single-byte dispatch on one PR. At `|suite| > 1` the harness
needs ≥ N bytes of state (one per PR), so `$?` cannot encode it
losslessly. The split:

| Channel | Carries                                             | Granularity |
| ------- | --------------------------------------------------- | :---------: |
| `$?`    | priority projection — coarse class of work to do    |   1 byte    |
| stdout  | per-PR JSONL records — fine-grained per-PR Outcome  |   N lines   |
| stderr  | per-iteration logs + per-PR variant blocks (triage) |      —      |

### Suite spawn loop (`suite::drive_suite`)

```
drive_suite(suite, cap, drive_one) =
    n         := |suite|
    cap'      := clamp(cap, 1, n)            (None → n)
    next      := AtomicUsize(0)
    results[i] := Mutex⟨Option⟨Outcome⟩⟩      for i ∈ [0, n)
    thread::scope |s|
      for w ∈ [0, cap'): s.spawn(|| {
        loop:
          i := next.fetch_add(1)
          if i ≥ n : break
          results[i] := drive_one(suite[i].slug, suite[i].pr)
      })
    return zip(suite, results) |> in input order
```

**Rolling concurrency, not batching.** The atomic-counter design
means a finished PR releases its slot for the next: PR_3 can start
before PR_1 finishes if a worker slot is free. Compared to a
batch-of-`cap` approach, no slow PR blocks the whole batch.

**Cross-thread isolation:**

- `THREAD_RECORDER` (in `recorder.rs`) is `thread_local!`, so each
  worker installs its own per-PR `Recorder` as the tool-call sink.
  No cross-PR aliasing.
- Per-PR `Recorder` is `Arc⟨Mutex⟨Inner⟩⟩`-backed; only one thread
  ever holds it.
- Per-PR `run_loop` state (`last_non_wait`, `last_attempted`) is
  on the worker's stack frame.
- `SuiteRecorder` is `Arc⟨Mutex⟨Inner⟩⟩`-backed; worker threads
  call `register_pr` after their own per-PR `Recorder` opens.

### Boundary functors

```
From⟨Vec⟨ProcessOutcome⟩⟩ for MultiOutcome    (suite mode):
    prs                                       → Bundle(prs)

UsageError construction (parser failure path):
    parse_args : Argv → Result⟨Args, Outcome⟩
    Outcome::UsageError(msg) → MultiOutcome::UsageError(msg)
       (lifted by main; exit_code = 64)
```

### Stdout JSONL contract

```
emit_jsonl : MultiOutcome → write to stdout
    UsageError(_)             → ε                              (no stdout)
    Bundle(prs)               → for p ∈ prs: writeln(record(p))

record : ProcessOutcome → JSON object
    base       := { slug, pr, outcome (variant name), exit (per-PR exit code) }
    Stuck*(a)  → base ⊎ { action = a.kind.name(), blocker = a.blocker.to_string() }
    Handoff*(a)→ Stuck*(a) fields ⊎ { prompt = a.description }
    WouldAdvance(a) → Stuck*(a) fields ⊎ { automation = format_automation(a.automation) }
    BinaryError(s)  → base ⊎ { msg = s }
    DoneMerged | DoneClosed | Paused → base
```

### Suite-level Recorder (`suite_recorder`)

```
SuiteRecorder = Arc⟨Mutex⟨SuiteInner⟩⟩
    open(cfg)            — write manifest.json + trace.md header
    register_pr(s, p, r) — append PrPointer { s, p, r }; rewrite pointers.json
    record_outcome(m, x) — write outcome.json + append per-PR table to trace.md

Layout: <state-root>/suites/<suite-id>/
    manifest.json    schema_version, suite_id, started_at, argv, mode, max_iter,
                     status_comment, concurrency, cwd, suite : Vec⟨{slug, pr}⟩
    pointers.json    schema_version, suite_id, prs : Vec⟨{slug, pr, run_id}⟩
    outcome.json     schema_version, suite_id, finished_at, exit_code, multi_outcome
    trace.md         human-readable: header + per-PR table + aggregate exit
```

`<suite-id>` shares the per-PR `<run-id>` shape
(`<utc>-<nanos>-p<pid>`). Two simultaneous suite invocations
against overlapping PRs each get a distinct `<suite-id>`; per-PR
`runs/<run-id>/` namespacing prevents ledger-level collisions.

---

## Invariants worth naming

```
[H1] DecisionHalt ⊂ HaltReason
       Render code is structurally incapable of witnessing loop-only halts.

[H2] ∀h : HaltReason, ∀d : Decision.
       d.exit_code() = match d { Halt(h) → h.exit_code(); Execute → 4 }
       Single source of truth for the internal IPC encoding.

[H3] ∀o : Outcome. |{c : ℕ | ∃o', o'.exit_code() = c ∧ same_variant(o, o')}| = 1
       1:1 variant→exit-code at the binary boundary. $? alone is
       sufficient for caller dispatch; no two variants share a code.

[O1] OrientedState.copilot = None  ⟺  no copilot ruleset configured
     OrientedState.cursor  = None  ⟺  no cursor activity observed
       Absence of signal is structurally distinct from low signal.

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

[P1] ∀ (slug, pr) ∈ Suite. trajectory of run_loop(slug, pr, …) inside
     ooda-prs ≡ trajectory of /ooda-pr on the same (slug, pr).
       Per-PR semantic preservation. run_loop is a pure function of
       its parameters; no global state.

[P2] ∀ distinct PR_i, PR_j. action_stream(PR_i) ⊥ action_stream(PR_j)
       Cross-thread isolation. THREAD_RECORDER is per-thread; per-PR
       Recorder is single-writer; run_loop state is on the worker
       stack. The only shared mutable state is the SuiteRecorder's
       Arc⟨Mutex⟨_⟩⟩, accessed only via register_pr.

[P3] ooda-prs terminates ⇐  ∀ (slug, pr) ∈ Suite. run_loop(slug, pr) terminates.
       run_loop is bounded by --max-iter; thread::scope joins all
       workers before returning; total runtime ≤ max(per-PR runtimes).

[P4] MultiOutcome::exit_code is total over MultiOutcome.
       UsageError → 64; empty Bundle → 0 (parser-unreachable but
       defined); priority projection covers all 9 per-PR Outcome
       variants.

[P5] parse_args is total over Argv.
       Argv → Result⟨Args, Outcome::UsageError⟩. Every input maps to
       exactly one of: valid Suite, or UsageError(_) carrying a
       single-line diagnostic.

[P6] Recorder soundness:
       (a) Per-PR Recorder writes are single-writer per (slug, pr).
       (b) Suite Recorder writes occur only from the main thread
           (manifest at open) or via Arc⟨Mutex⟨_⟩⟩-serialized
           register_pr / record_outcome calls.
       (c) <run-id> uniqueness within a process via nanosecond + pid
           prevents file collision across simultaneous suite
           invocations on the same PR.

[P7] Harness composability:
       case $? of
         0       → all Done(Merged|Closed)/Paused — converged
         5       → ∃ HandoffAgent — caller dispatches sub-agents (parallel)
         3       → ∃ HandoffHuman — caller surfaces to human
         {1,2,6} → ∃ Stuck*/BinaryError — caller escalates
         4       → ∃ WouldAdvance — inspect-only artifact
         64      → UsageError — caller fixes invocation
       Stdout JSONL records carry the per-PR detail for each branch.

[P8] No surviving counterexample. (Sweep:)
       (a) panic in PR_i: thread::scope propagates on join; per-PR
           Recorders for completed siblings remain on disk.
       (b) gh rate-limit cascade: --concurrency K bounds the request
           burst; default = |suite|.
       (c) two ooda-prs invocations on overlapping PRs: distinct
           <suite-id> + <run-id> per process; latest/ledger collide
           harmlessly (last-writer-wins, same as /ooda-pr).
       (d) |suite| = 1: degenerate but valid; one stdout record;
           $? = that PR's per-PR exit code (since no other PR
           contributes to the priority projection).
```
