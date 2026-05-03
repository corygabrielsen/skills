# `/ooda-pr` — Type Algebra

Single-binary OODA loop for driving a PR through observe → orient →
decide → act until merge or external resolution.

This document is the **type-level specification**. For invocation
and exit-code taxonomy see `SKILL.md`. For implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner

run_loop : RepoSlug × PullRequestNumber × LoopConfig × OnState
        → Result⟨HaltReason, LoopError⟩

main : Argv → ExitCode
ExitCode = HaltReason.exit_code() ⊎ {6 (runtime), 64 (usage), 4 (in_progress)}
```

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

```
Decision::exit_code()  ≡  match { Execute → 4, Halt(h) → h.exit_code() }
HaltReason::exit_code()≡  match { Decision(h) → h.exit_code(),
                                  Stalled → 1, CapReached{..} → 2 }
DecisionHalt::exit_code() ≡ match { Success | Terminal → 0,
                                    HumanNeeded → 3, AgentNeeded → 5 }
```

One taxonomy, three layers, single source of truth.

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
  ⊕ Stalled                                 ⟶ 1
  ⊕ CapReached{last_action: Option⟨Action⟩} ⟶ 2

run_loop(slug, pr, cfg, on_state) =
    last := None
    for i in 1..=cfg.max_iterations:
        obs      := fetch_all(slug, pr)?              -- LoopError::Observe
        oriented := orient(obs, None)
        decision := decide(oriented, obs.pr_view.state)
        on_state(i, oriented, decision)
        case decision of
            Halt(h)    → return Decision(h)
            Execute(a) → if same_action_repeated(last, a) return Stalled
                         act(a, slug, pr)?            -- LoopError::Act
                         last := Some(a)
    return CapReached{last_action: last}

same_action_repeated(prev, cur) =
    cur.automation ≠ Wait ∧
    prev.exists(p ⟹ p.kind = cur.kind ∧ p.blocker = cur.blocker)
```

The loop is a Kleene iteration of `(observe ∘ orient ∘ decide ∘ act)*`
until either `decide` halts or stall/cap fires. Wait actions are
excluded from stall detection — polling is _expected_ to repeat.

---

## Invariants worth naming

```
[H1] DecisionHalt ⊂ HaltReason
       Render code is structurally incapable of witnessing loop-only halts.

[H2] ∀h : HaltReason, ∀d : Decision.
       d.exit_code() = match d { Halt(h) → h.exit_code(); Execute → 4 }
       Single source of truth for the IPC encoding.

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

[R1] runner.same_action_repeated returns false when current.automation = Wait
       Polling is expected to repeat; stall detection ignores it.

[T1] ∀t1, t2 : Timestamp. t1 = t2  ⟺  t1.at() = t2.at()
       Timestamp Eq is on instant, not on bytes — surface forms collapse.
```
