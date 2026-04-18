import type { Score } from "./branded.js";

/**
 * PR fitness report — the public contract.
 *
 * Every field is derived from live GitHub API queries.
 * Nothing is cached. Nothing is inferred from prior runs.
 */
export interface PullRequestFitnessReport {
  readonly version: string;
  readonly pr: number;
  readonly url: string;
  readonly title: string;
  /** First 8 chars of HEAD SHA. */
  readonly head: string;
  readonly base: string;
  /** PR lifecycle: open, merged, or closed. */
  readonly lifecycle: Lifecycle;
  /**
   * Current fitness score. Higher is better. Compared against `target` by
   * /converge to decide loop termination. For PRs this is a Copilot tier
   * ordinal (🥉 bronze=1, 🥈 silver=2, 🥇 gold=3, 💠 platinum=4) when Copilot
   * is configured, else a simple blocker-vs-clean scalar. 💎 Diamond is a
   * reserved name for a future tier above platinum — not yet emitted.
   */
  readonly score: Score;
  /**
   * Branded presentation of `score` for human-readable trace output —
   * e.g. `"🥇 (gold)"`. /converge renders this in PR trace comments.
   */
  readonly score_display: string;
  /** Target score the caller asked for (default 💠 platinum, 4). */
  readonly target: Score;
  /** Branded presentation of `target`, mirror of `score_display`. */
  readonly target_display: string;
  /**
   * Terminal state — present iff the PR can no longer make progress
   * (merged, closed). /converge reads `kind` opaquely and halts.
   */
  readonly terminal?: { readonly kind: string };
  /** ISO 8601 — when the PR was merged. Null if not merged. */
  readonly merged_at: string | null;
  /** ISO 8601 — when the PR was closed. Null if open. */
  readonly closed_at: string | null;
  /** True when all hard blockers are clear. Always true for merged PRs. */
  readonly mergeable: boolean;
  /** Human-readable blocker descriptions. Empty when mergeable or merged. */
  readonly blockers: readonly string[];
  /**
   * Blockers split by resolution authority. Agent blockers cap score;
   * human and structural don't. Converge uses this to choose halt
   * behavior: "keep working" vs "your turn" vs "waiting on downstack."
   */
  readonly blocker_split: {
    readonly agent: readonly string[];
    readonly human: readonly string[];
    readonly structural: readonly string[];
  };
  readonly ci: CiSummary;
  readonly reviews: ReviewSummary;
  readonly copilot: import("./copilot.js").CopilotReport;
  readonly state: PullRequestState;
  /** Graphite stack-ordering check (separate from CI). */
  readonly graphite: GraphiteCheck;
  /** Ordered action plan to increase fitness. */
  readonly actions: readonly import("./action.js").Action[];
  /**
   * Human-readable one-liner. Named `status` to match /converge's
   * `FitnessReport.status` contract so downstream consumers don't need
   * a field-name translation layer.
   */
  readonly status: string;
  /**
   * Informational lines surfaced by /converge as bullet points. Used
   * today for advisory-check context ("2 advisory checks failing: …").
   * Empty when there's nothing noteworthy to flag.
   */
  readonly notes: readonly string[];
  /**
   * Per-axis activity state for /converge's iteration-key dedup. Lets
   * converge distinguish "Copilot working" from "Copilot reviewed"
   * when the picked action and blockers are identical. Keys are
   * skill-defined labels; /converge treats the map opaquely.
   */
  readonly activity_state: Readonly<Record<string, string>>;
  /** Decomposed score components for PR progress comment rendering. */
  readonly score_emoji: string;
  readonly score_label: string;
  /** Target label WITHOUT emoji — revealed only when score reaches target. */
  readonly target_label: string;
  /**
   * Per-axis status lines for the PR progress comment. Each entry is
   * rendered as `{emoji} {name} {summary}`.
   */
  readonly axes: readonly AxisLine[];
  /**
   * Curated state snapshot for machine readers. Rendered inside a
   * collapsed `<details>` block on the PR progress comment.
   * /converge enriches it with iter + action + halt fields.
   */
  readonly snapshot: Readonly<Record<string, unknown>>;
  /** ISO 8601 timestamp of when this report was generated. */
  readonly timestamp: string;
  /** Milliseconds taken to generate this report. */
  readonly duration_ms: number;
}

export type Lifecycle = "open" | "merged" | "closed";

/** Bucketed check counts — shared shape between required and advisory. */
export interface CheckBucketSummary {
  readonly pass: number;
  readonly fail: number;
  readonly pending: number;
  readonly total: number;
  readonly failed: readonly string[];
  readonly pending_names: readonly string[];
  readonly failed_details: readonly FailedCheck[];
}

/** Non-required check summary — reported but not gating. */
export type AdvisorySummary = CheckBucketSummary;

/**
 * CI state for the PR.
 *
 * Top-level counts reflect **required** checks only — those that gate
 * merge per branch protection or rulesets. The required set is derived
 * from branch rules configuration (`rules/branches/{base}` + legacy
 * branch protection), then joined with observed check-runs on the PR.
 *
 * `missing` captures configured requirements with no corresponding
 * check-run — the skipped-job case where a required job's `needs:`
 * dependency failed and the check-run was never created.
 *
 * Advisory-only failures surface in `advisory` but never drive
 * `fix_ci` actions or appear in blocker lists.
 *
 * When the PR has zero required checks configured, every check falls
 * into `advisory` and the required counts are all zero — a legitimate
 * state meaning "nothing is gating CI."
 */
export interface CiSummary extends CheckBucketSummary {
  /** Required checks configured but absent — no check-run exists on the PR. */
  readonly missing: number;
  /** Names of missing required checks. */
  readonly missing_names: readonly string[];
  /** ISO 8601 — most recent check completion time across all checks. Null if none completed. */
  readonly completed_at: string | null;
  /** Non-required check state, surfaced for visibility only. */
  readonly advisory: AdvisorySummary;
}

export interface FailedCheck {
  readonly name: string;
  /** Check run output title (one-liner from CI). */
  readonly description: string;
  /** URL to the check run details page. */
  readonly link: string;
}

export interface ReviewSummary {
  readonly decision: ReviewDecision;
  readonly threads_unresolved: number;
  readonly threads_total: number;
  readonly bot_comments: number;
  readonly approvals_on_head: number;
  readonly approvals_stale: number;
  /** Requested reviewers who haven't submitted yet, split by kind. */
  readonly pending_reviews: {
    readonly bots: readonly string[];
    readonly humans: readonly string[];
  };
  /** Submitted bot reviews (Copilot, Cursor, etc). */
  readonly bot_reviews: readonly BotReview[];
}

export interface BotReview {
  readonly user: string;
  readonly state: string;
  readonly submitted_at: string;
}

export type ReviewDecision =
  | "APPROVED"
  | "REVIEW_REQUIRED"
  | "CHANGES_REQUESTED"
  /** No review policy configured — approval is not required to merge. */
  | "NONE";

export interface PullRequestState {
  readonly conflict: ConflictState;
  readonly draft: boolean;
  readonly wip: boolean;
  readonly title_len: number;
  readonly title_ok: boolean;
  readonly body: boolean;
  readonly summary: boolean;
  readonly test_plan: boolean;
  readonly content_label: boolean;
  readonly assignees: number;
  readonly reviewers: number;
  readonly merge_when_ready: boolean;
  readonly commits: number;
  /**
   * True when the PR's base branch has advanced past the merge base —
   * the PR is stale relative to its parent (master for bottom-of-stack,
   * the upstream PR's branch for middle/top). Surfaced for observation;
   * no action fires on it yet.
   */
  readonly behind: boolean;
  /** ISO 8601 — last update to the PR (push, comment, label, etc). */
  readonly updated_at: string;
  /** ISO 8601 — when the HEAD commit was authored. Null if unavailable. */
  readonly last_commit_at: string | null;
}

export type ConflictState = "MERGEABLE" | "CONFLICTING" | "UNKNOWN";

/**
 * Graphite stack-ordering check.
 *
 * This is NOT CI. It's a guard that prevents out-of-order merges
 * in stacked PRs. Bottom-of-stack passes immediately. Upstack PRs
 * stay pending until the PR below merges. It never "fails".
 *
 * Title and summary come directly from Graphite's CheckRun output
 * via the GitHub API — we don't hardcode or interpret them.
 */
export interface GraphiteCheck {
  readonly status: "pass" | "pending" | "none";
  /** Graphite's one-liner, e.g. "This check will pass when downstack PRs merge". Null when status is "none" or "pass". */
  readonly title: string | null;
  /** Graphite's full markdown explanation with specific PR numbers. Null when not available. */
  readonly summary: string | null;
}

export type GraphiteStatus = GraphiteCheck["status"];

export interface AxisLine {
  readonly name: string;
  readonly emoji: string;
  readonly summary: string;
}
