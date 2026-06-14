#!/bin/bash
# Enforce the anti-DRY mirror invariant across the 3 PR-side OODA binaries.
#
# `ooda-pr` (canonical), `ooda-prs`, and `ooda-pr-codex-review` hold
# byte-identical copies of a curated set of files. The "copy then sync"
# discipline is human policy; this script makes it CI-enforced.
#
# When this script fails:
#   1. Identify the canonical version (usually the one that changed last).
#   2. `cp $CANONICAL/<file> $OTHER/<file>` for each drifted pair.
#   3. Re-run this script until clean.
#
# Two diff modes:
#   - Strict: every listed file must be byte-identical across all 3 binaries.
#   - Allowlist: pr-codex-review may inject a fixed set of `codex_review:
#     None` test-fixture lines into decide/reviews.rs and decide/state.rs;
#     comparison strips those before diffing.

set -o errexit
set -o nounset
set -o pipefail

# Bash-only constructs in use: process substitution `<(...)`, here-string
# `<<<`, `declare -a`, `[[ ]]`. Guard against accidental `sh script.sh`.
[ -n "${BASH_VERSION:-}" ] || { printf '%s: requires bash\n' "$0" >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CANON="ooda-pr"
MIRRORS=("ooda-prs" "ooda-pr-codex-review")

# Files that must be byte-identical across all 3 PR-side binaries.
# Keep this list in lock-step with the actual mirror set; add to it
# whenever a new file gains "shared across PR-side binaries" status.
STRICT_FILES=(
    "src/act/address_claude_review.rs"
    "src/act/ci.rs"
    "src/act/closeout.rs"
    "src/axis_impls/branch_sync.rs"
    "src/axis_impls/ci.rs"
    "src/axis_impls/claude_review.rs"
    "src/axis_impls/closeout.rs"
    "src/axis_impls/copilot.rs"
    "src/axis_impls/cursor.rs"
    "src/axis_impls/doc_review.rs"
    "src/axis_impls/mod.rs"
    "src/axis_impls/pull_request_metadata.rs"
    "src/axis_impls/reviews.rs"
    "src/axis_impls/state.rs"
    "src/act/copilot.rs"
    "src/act/review_docs.rs"
    "src/act/sync_pull_request_metadata.rs"
    "src/comment/post.rs"
    "src/comment.rs"
    "src/dashboard.rs"
    "src/decide/branch_sync.rs"
    "src/decide/ci.rs"
    "src/decide/claude_review.rs"
    "src/decide/closeout.rs"
    "src/decide/copilot.rs"
    "src/decide/cursor.rs"
    "src/decide/decision.rs"
    "src/decide/doc_review.rs"
    "src/decide/merge_eligibility.rs"
    "src/decide/pull_request_metadata.rs"
    "src/decide/signing_eligibility.rs"
    "src/observe/branch.rs"
    "src/observe/github/branch_protection.rs"
    "src/observe/github/branch_rules.rs"
    "src/observe/github/checks.rs"
    "src/observe/github/claude_review_attest.rs"
    "src/observe/github/closeout_attest.rs"
    "src/observe/github/comments.rs"
    "src/observe/github/compare.rs"
    "src/observe/github/copilot_config.rs"
    "src/observe/github/cursor_status.rs"
    "src/observe/github/doc_review_attest.rs"
    "src/observe/github/gh.rs"
    "src/observe/github/issue_events.rs"
    "src/observe/github/pull_request_metadata_attestation.rs"
    "src/observe/github/pull_request_view.rs"
    "src/observe/github/rate_limit.rs"
    "src/observe/github/requested_reviewers.rs"
    "src/observe/github/pr_commits.rs"
    "src/observe/github/reviews.rs"
    "src/observe/github/review_threads.rs"
    "src/observe/github.rs"
    "src/observe/github/rulesets.rs"
    "src/observe/github/stack_root.rs"
    "src/observe/github/workflow_runs.rs"
    "src/orient/bot_threads.rs"
    "src/orient/ci.rs"
    "src/orient/claude_review.rs"
    "src/orient/closeout.rs"
    "src/orient/copilot.rs"
    "src/orient/cursor.rs"
    "src/orient/doc_review.rs"
    "src/orient/pull_request_metadata.rs"
    "src/orient/required_checks.rs"
    "src/orient/reviews.rs"
    "src/orient/state.rs"
    "src/orient/thread.rs"
    "src/outcome.rs"
    "src/signal.rs"
    "src/text.rs"
)

# Per-mirror allowlist: each entry is "<mirror>:<file>". The script
# strips lines matching the allowed pattern from BOTH sides before
# diffing. Used for cases where pr-codex-review must carry an
# additional test-fixture line (the `codex_review: None` field on
# OrientedState) that the other binaries' OrientedState does not have.
declare -a ALLOWLIST_PATHS=(
    "ooda-pr-codex-review:src/decide/reviews.rs"
    "ooda-pr-codex-review:src/decide/state.rs"
    "ooda-pr-codex-review:src/decide/pull_request_metadata.rs"
    "ooda-pr-codex-review:src/decide/doc_review.rs"
    "ooda-pr-codex-review:src/decide/claude_review.rs"
    "ooda-pr-codex-review:src/decide/closeout.rs"
    "ooda-pr-codex-review:src/comment/render.rs"
)
ALLOWLIST_PATTERN='codex_review: None,'

fail=0
report_fail() {
    fail=1
    printf 'MIRROR DRIFT: %s\n' "$1" >&2
}

is_allowlisted() {
    local mirror="$1"
    local file="$2"
    local key="${mirror}:${file}"
    local entry
    for entry in "${ALLOWLIST_PATHS[@]}"; do
        if [ "$entry" = "$key" ]; then
            return 0
        fi
    done
    return 1
}

diff_one() {
    local file="$1"
    local mirror="$2"
    local canon_path="$ROOT/$CANON/$file"
    local mirror_path="$ROOT/$mirror/$file"

    if [ ! -f "$canon_path" ]; then
        report_fail "$CANON/$file missing (canonical source absent)"
        return
    fi
    if [ ! -f "$mirror_path" ]; then
        report_fail "$mirror/$file missing (mirror absent)"
        return
    fi

    if is_allowlisted "$mirror" "$file"; then
        # Strip allowlisted lines on BOTH sides before comparing.
        local canon_filtered mirror_filtered
        canon_filtered=$(grep -v "$ALLOWLIST_PATTERN" "$canon_path" || true)
        mirror_filtered=$(grep -v "$ALLOWLIST_PATTERN" "$mirror_path" || true)
        if [ "$canon_filtered" != "$mirror_filtered" ]; then
            report_fail "$mirror/$file diverges from $CANON/$file (after allowlist filter)"
            diff <(printf '%s\n' "$canon_filtered") \
                 <(printf '%s\n' "$mirror_filtered") >&2 || true
        fi
        return
    fi

    if ! cmp -s "$canon_path" "$mirror_path"; then
        report_fail "$mirror/$file diverges from $CANON/$file"
        diff "$canon_path" "$mirror_path" >&2 || true
    fi
}

# Partial-mirror set (pr ≡ prs only; pr-codex-review legitimately diverges).
# These files participate in the "ooda-pr is canonical for ooda-prs"
# invariant but NOT the 3-way invariant — pr-codex-review extends the
# decide axis with codex-review-specific logic.
PARTIAL_MIRROR_FILES=(
    "src/comment/render.rs"
    "src/decide/action.rs"
    "src/decide/reviews.rs"
    "src/decide.rs"
    "src/decide/state.rs"
    "src/ids.rs"
    "src/observe.rs"
    "src/orient.rs"
    "src/act.rs"
    "src/runner.rs"
)

# Per-binary divergent set: present in all 3 PR-side binaries but
# legitimately differs across them. Listed explicitly so a future
# contributor (or this contributor) doesn't cp-blast one over another
# during a refactor — that mistake silently breaks the binary whose
# content was clobbered. The diff_one comparator is skipped for these;
# the script only verifies they exist with content in every binary.
PER_BINARY_DIVERGENT_FILES=(
    "src/main.rs"
    "src/recorder.rs"
)

for file in "${STRICT_FILES[@]}"; do
    for mirror in "${MIRRORS[@]}"; do
        diff_one "$file" "$mirror"
    done
done

# Partial: only ooda-pr vs ooda-prs.
for file in "${PARTIAL_MIRROR_FILES[@]}"; do
    diff_one "$file" "ooda-prs"
done

# Per-binary divergent: just verify presence + content in all 3.
for file in "${PER_BINARY_DIVERGENT_FILES[@]}"; do
    for binary in "$CANON" "${MIRRORS[@]}"; do
        path="$ROOT/$binary/$file"
        if [ ! -s "$path" ]; then
            report_fail "$binary/$file missing or empty (per-binary divergent: must exist with content in every binary)"
        fi
    done
done

# Coverage check: every .rs file in canonical ooda-pr/src/ must be
# classified into exactly one of {STRICT, PARTIAL, PER_BINARY_DIVERGENT}.
# Catches "new file added without tier assignment" — the failure mode
# that silently lets cp-blasts clobber per-binary content.
#
# Bare `var=$(cmd)` propagates the subshell exit under `set -euo
# pipefail`, so a missing $ROOT/$CANON or a `find` failure aborts
# before the empty-set vacuously passes the comm/while below.
[ -d "$ROOT/$CANON" ] || { printf '%s: canonical source missing: %s\n' "$0" "$ROOT/$CANON" >&2; exit 1; }
canon_files=$(cd "$ROOT/$CANON" && find src -name '*.rs' -type f | sort)
classified=$(printf '%s\n' \
    "${STRICT_FILES[@]}" \
    "${PARTIAL_MIRROR_FILES[@]}" \
    "${PER_BINARY_DIVERGENT_FILES[@]}" | sort -u)
unclassified=$(comm -23 <(printf '%s\n' "$canon_files") <(printf '%s\n' "$classified"))
if [ -n "$unclassified" ]; then
    while IFS= read -r file; do
        report_fail "$CANON/$file unclassified — add to STRICT_FILES, PARTIAL_MIRROR_FILES, or PER_BINARY_DIVERGENT_FILES"
    done <<< "$unclassified"
fi

# Wire-token coverage: PER_BINARY_DIVERGENT permits the recorder code
# to differ across the 3 PR-side binaries, but every wire symbol
# (`DomainKind::` variant, `EventBody::` variant) emitted by one
# recorder must appear in the other two so the on-disk event stream
# carries the same vocabulary regardless of which binary produced it.
# Multiset equality is the test — duplicate count matters because a
# recorder that emits `IterationExecuted` twice and another that
# emits it once is a wire-shape divergence the script catches.
#
# `ooda-codex-review` participates in `decision_kind` coverage via the
# `DecisionKind::` wire vocabulary lifted into `ooda_state::tokens`
# (see `decision_kind_drift` check below). Its `DomainKind::` /
# `EventBody::` multisets legitimately diverge from the PR trio (its
# event set is smaller and its recorder lives inline in `main.rs` +
# `runner.rs`, not in `recorder.rs`), so it is excluded from those two
# checks.
wire_tokens() {
    local file="$1"
    local pattern="$2"
    grep -oE "$pattern" "$file" | sort
}
diff_wire_tokens() {
    local label="$1"
    local pattern="$2"
    local canon_path="$ROOT/$CANON/src/recorder.rs"
    local canon_tokens
    canon_tokens=$(wire_tokens "$canon_path" "$pattern")
    local mirror
    for mirror in "${MIRRORS[@]}"; do
        local mirror_path="$ROOT/$mirror/src/recorder.rs"
        local mirror_tokens
        mirror_tokens=$(wire_tokens "$mirror_path" "$pattern")
        if [ "$canon_tokens" != "$mirror_tokens" ]; then
            report_fail "$mirror/src/recorder.rs $label multiset diverges from $CANON/src/recorder.rs"
            diff <(printf '%s\n' "$canon_tokens") <(printf '%s\n' "$mirror_tokens") >&2 || true
        fi
    done
}
# `DomainKind::Foo` token table — every PR-side recorder must emit
# the same `kind_suffix` discriminator vocabulary. Pattern matches
# real consumer sites (`DomainKind::Foo,` argument position) so
# docstrings carrying the same identifier do not skew the multiset.
diff_wire_tokens "DomainKind" "DomainKind::[A-Za-z][A-Za-z0-9_]*,"
# `EventBody::Foo {` typed-event coverage — same set of typed events
# constructed across recorders. Pattern matches the constructor form
# only, so `[\`EventBody::Foo\`]` doc-link references in comments do
# not skew the multiset.
#
# `EventBody::DomainSpecific {` is excluded from the check: it is a
# legitimate per-binary escape hatch for `kind_suffix` literals
# outside the shared `DomainKind` vocabulary (e.g.
# `codex_review_config` in `ooda-pr-codex-review`). Anything routed
# through the `DomainKind` enum is already covered by the
# `DomainKind` check above; a raw `DomainSpecific` constructor
# announces an intentional per-binary extra.
diff_wire_tokens_excluding_domain_specific() {
    local label="$1"
    local pattern="$2"
    local canon_path="$ROOT/$CANON/src/recorder.rs"
    local canon_tokens
    canon_tokens=$(wire_tokens "$canon_path" "$pattern" | grep -v 'EventBody::DomainSpecific' || true)
    local mirror
    for mirror in "${MIRRORS[@]}"; do
        local mirror_path="$ROOT/$mirror/src/recorder.rs"
        local mirror_tokens
        mirror_tokens=$(
            wire_tokens "$mirror_path" "$pattern" | grep -v 'EventBody::DomainSpecific' || true
        )
        if [ "$canon_tokens" != "$mirror_tokens" ]; then
            report_fail "$mirror/src/recorder.rs $label multiset diverges from $CANON/src/recorder.rs"
            diff <(printf '%s\n' "$canon_tokens") <(printf '%s\n' "$mirror_tokens") >&2 || true
        fi
    done
}
diff_wire_tokens_excluding_domain_specific "EventBody" "EventBody::[A-Za-z][A-Za-z0-9_]* \{"

# `decision_kind` wire-token coverage spans ALL FOUR binaries (PR trio
# + ooda-codex-review). The literal strings live on
# `ooda_state::tokens::DecisionKind`; every recorder maps its
# `Decision`/`DecisionHalt` value onto that enum and calls `.as_str()`.
# A drift instance — `ooda-codex-review` historically emitting
# `Halt::Terminal::Succeeded` while the PR trio emitted
# `Halt::Terminal(Succeeded)` — was what motivated the lift, and this
# check forecloses its recurrence.
#
# Coverage is structural: every binary must route its `decision_kind`
# through a `DecisionKind::` variant, and the literal wire-string
# forms (`Halt::Terminal::` double-colon, `format!("Halt::Terminal(`
# paren-format) must NOT appear anywhere — those would re-introduce
# the drift the lift eliminated.
check_decision_kind_drift() {
    local binary="$1"
    local recorder_path="$2"
    if [ ! -f "$recorder_path" ]; then
        report_fail "decision_kind: $binary recorder source missing at $recorder_path"
        return
    fi
    # Positive coverage: at least one DecisionKind:: routing site.
    if ! grep -q 'DecisionKind::' "$recorder_path"; then
        report_fail "decision_kind: $binary does not route through DecisionKind:: (lifted vocabulary bypassed in $recorder_path)"
    fi
    # Negative coverage: ban `format!("Halt::...` emission. The lifted
    # vocabulary owns the literal strings; any `format!("Halt::` in a
    # recorder is bypassing the enum and recreating the drift the lift
    # eliminated. Regression-guard tests use `contains`/`assert_eq!`
    # against the literal, never `format!`, so they don't false-fire.
    if grep -nE 'format!\("Halt::' "$recorder_path" >&2; then
        report_fail "decision_kind: $binary uses format!(\"Halt::...\") in $recorder_path — route through DecisionKind:: instead"
    fi
}
check_decision_kind_drift "ooda-pr" "$ROOT/ooda-pr/src/recorder.rs"
check_decision_kind_drift "ooda-prs" "$ROOT/ooda-prs/src/recorder.rs"
check_decision_kind_drift "ooda-pr-codex-review" "$ROOT/ooda-pr-codex-review/src/recorder.rs"
check_decision_kind_drift "ooda-codex-review" "$ROOT/ooda-codex-review/src/runner.rs"

# `domain:` wire-token drift. The literal `domain:` field on
# `EventBody::RunStarted` MUST route through `PrDomain::name()` or
# `CodexReviewDomain::name()`; a hardcoded `domain: "pr"` /
# `domain: "codex-review"` in any recorder source silently drifts the
# on-disk schema (`domain: "PR"`, `domain: "prs"`, `domain:
# "codex_review"` were all on-the-table typos before the lift). Pattern
# matches the struct-literal form `domain: "..."` only; docstring
# mentions and `"domain":"..."` assertion strings in test bodies are
# unaffected.
check_domain_literal_drift() {
    local binary="$1"
    local recorder_path="$2"
    if [ ! -f "$recorder_path" ]; then
        report_fail "domain_literal: $binary recorder source missing at $recorder_path"
        return
    fi
    local hits
    hits=$(grep -nE '^[[:space:]]*domain: "[^"]+"' "$recorder_path" || true)
    if [ -n "$hits" ]; then
        report_fail "domain_literal: $binary hardcodes a domain string in $recorder_path — route through PrDomain::name() / CodexReviewDomain::name() instead"
        printf '%s\n' "$hits" >&2
    fi
}
check_domain_literal_drift "ooda-pr" "$ROOT/ooda-pr/src/recorder.rs"
check_domain_literal_drift "ooda-prs" "$ROOT/ooda-prs/src/recorder.rs"
check_domain_literal_drift "ooda-pr-codex-review" "$ROOT/ooda-pr-codex-review/src/recorder.rs"
check_domain_literal_drift "ooda-codex-review-main" "$ROOT/ooda-codex-review/src/main.rs"
check_domain_literal_drift "ooda-codex-review-runner" "$ROOT/ooda-codex-review/src/runner.rs"

# Each binary's SKILL.md must invoke its OWN `run` wrapper, never a
# sibling's. A copy-paste from one SKILL.md to another silently drops
# the caller onto the wrong binary — for `ooda-pr-codex-review`, that
# meant callers following the doc invoked `ooda-pr` (no codex axis)
# and hit `UsageError` exit 64 on codex-only flags. Pattern matches
# any `~/.claude/skills/<name>/run` path; presence of a non-self name
# is the bug. Informational `/ooda-pr` cross-references (no `/run`
# suffix) are unaffected.
check_skill_md_sibling_invocation() {
    local skill="$1"
    local skill_md="$ROOT/$skill/SKILL.md"
    if [ ! -f "$skill_md" ]; then
        return
    fi
    local foreign
    foreign=$(grep -nE '~/\.claude/skills/[a-z-]+/run' "$skill_md" \
        | grep -v "~/\.claude/skills/$skill/run" || true)
    if [ -n "$foreign" ]; then
        report_fail "$skill/SKILL.md invokes a sibling skill's run script:"
        printf '%s\n' "$foreign" >&2
    fi
}
for skill in ooda-pr ooda-prs ooda-pr-codex-review ooda-codex-review ooda-attest; do
    check_skill_md_sibling_invocation "$skill"
done

# 130/143 are NOT reserved kernel-only codes. Every binary in the
# family catches SIGINT/SIGTERM at an iteration boundary and emits
# `Outcome::SignalInterrupted`; the binary itself prints
# `Interrupted: exit code <N>` and exits 130 / 143 from its own
# main(). Any SKILL.md that still claims those codes are "reserved",
# "kernel kills", or "the binary never returns them" is documenting
# a state that ships truthfully today as SignalInterrupted — the
# wrong contract for callers writing signal-shutdown dispatchers.
# Pattern enumerates the specific shapes of the bad claim, not a
# loose `reserved` keyword (the table-adjacent "held in reserve for
# future Outcome variants" prose is unrelated and must not match).
check_signal_misclaim() {
    local skill_md="$1"
    local hits
    hits=$(grep -nE '130[^A-Za-z0-9]*(reserved|kernel|never returns?)|143[^A-Za-z0-9]*(reserved|kernel|never returns?)|reserved.*(kernel|kill)|kernel.*kill|130/143.*never returns?|never returns? (this|them|130|143)' "$skill_md" || true)
    if [ -n "$hits" ]; then
        report_fail "$skill_md still describes 130/143 as reserved kernel-only; both codes are returned by Outcome::SignalInterrupted"
        printf '%s\n' "$hits" >&2
    fi
}
for skill in ooda-pr ooda-prs ooda-pr-codex-review ooda-codex-review ooda-attest; do
    skill_md="$ROOT/$skill/SKILL.md"
    [ -f "$skill_md" ] || continue
    check_signal_misclaim "$skill_md"
done

if [ "$fail" -ne 0 ]; then
    printf '\nMirror invariant violated. Re-sync the canonical and re-run.\n' >&2
    exit 1
fi

# Witness body length-cap regression is foreclosed at the type level:
# `ooda_core::handoff_prompt::Witness::body` is `SafeBody`, which
# truncates at construction. A bare `String` will fail to compile
# rather than slipping through here; no grep sweep needed.
# Witness URL scheme regression is foreclosed identically by `SafeUrl`.

echo "Mirror invariant: OK"
