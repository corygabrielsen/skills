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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CANON="ooda-pr"
MIRRORS=("ooda-prs" "ooda-pr-codex-review")

# Files that must be byte-identical across all 3 PR-side binaries.
# Keep this list in lock-step with the actual mirror set; add to it
# whenever a new file gains "shared across PR-side binaries" status.
STRICT_FILES=(
    "src/act/address_claude_review.rs"
    "src/act/ci.rs"
    "src/act/closeout.rs"
    "src/axis_impls/ci.rs"
    "src/axis_impls/copilot.rs"
    "src/axis_impls/cursor.rs"
    "src/axis_impls/mod.rs"
    "src/act/copilot.rs"
    "src/act/review_docs.rs"
    "src/act/sync_pull_request_metadata.rs"
    "src/comment/post.rs"
    "src/comment.rs"
    "src/dashboard.rs"
    "src/decide/ci.rs"
    "src/decide/claude_review.rs"
    "src/decide/closeout.rs"
    "src/decide/copilot.rs"
    "src/decide/cursor.rs"
    "src/decide/decision.rs"
    "src/decide/doc_review.rs"
    "src/decide/pull_request_metadata.rs"
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

if [ "$fail" -ne 0 ]; then
    printf '\nMirror invariant violated. Re-sync the canonical and re-run.\n' >&2
    exit 1
fi

echo "Mirror invariant: OK"
