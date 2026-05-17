#!/bin/sh
# Property 1: Message Well-Formedness — validation script
#
# Decision procedure for the commit message subject line constraint.
# Given a commit message M = (subject, body):
#   valid(M) ⟺ |subject| ≤ <max>
#
# The subject limit depends on context:
#   Default (PR-suffix-aware mode):
#     With trailing " (#NNN)" suffix:   |subject| ≤ 50
#     Without:                          |subject| ≤ 42  (room for GitHub's " (#NNNN)")
#   With --max N:                       |subject| ≤ N   (suffix logic skipped)
#
# Use `--max 50` in repos that don't squash-merge through GitHub PRs:
# no suffix is ever appended, so the 42-char overhead is unjustified.
#
# Authoritative source: CONTRIBUTING.md "Commit Messages" section (when present),
# or the per-repo .pre-commit-config.yaml hook args.
#
# Usage:
#   validate-commit-message.sh [--max N] <commit-msg-file>
#   validate-commit-message.sh [--max N] -        # stdin
#
# Exit codes:
#   0 - Valid
#   1 - Invalid (or runtime failure: file not found)
#   2 - Usage error (missing or invalid argument)

set -e
set -u

usage() {
    printf 'Usage: %s [--max N] <commit-msg-file|->\n' "$0" >&2
    exit 2
}

max_override=""
while [ $# -gt 0 ]; do
    case "$1" in
        --max)
            shift
            [ $# -gt 0 ] || usage
            max_override="$1"
            shift
            ;;
        --max=*)
            max_override="${1#--max=}"
            shift
            ;;
        --)
            shift
            break
            ;;
        -)
            break
            ;;
        --*)
            printf 'Unknown option: %s\n' "$1" >&2
            usage
            ;;
        *)
            break
            ;;
    esac
done

if [ -n "$max_override" ]; then
    case "$max_override" in
        ''|*[!0-9]*)
            printf 'Error: --max requires a positive integer, got: %s\n' "$max_override" >&2
            exit 2
            ;;
    esac
    [ "$max_override" -gt 0 ] || {
        printf 'Error: --max must be > 0, got: %s\n' "$max_override" >&2
        exit 2
    }
fi

[ $# -ge 1 ] || usage
input="$1"

if [ "$input" = "-" ]; then
    message=$(cat)
elif [ -f "$input" ] && [ -r "$input" ]; then
    # Strip comment lines (git's default commit template has them).
    # sed exits 0 except on real errors (vs `grep -v` which exits
    # 1 for no matches, hiding actual I/O errors under `|| true`).
    # Both -f and -r required: -r alone would accept FIFOs and
    # device files (e.g. /dev/zero would read forever).
    message=$(sed '/^#/d' "$input")
elif [ -e "$input" ]; then
    printf 'Error: not a readable regular file: %s\n' "$input" >&2
    exit 1
else
    printf 'Error: File not found: %s\n' "$input" >&2
    exit 1
fi

# Extract subject (first line). printf instead of echo so a
# dash-prefixed subject isn't interpreted as a flag.
subject=$(printf '%s\n' "$message" | head -n1)

fail() {
    {
        printf 'Commit message validation failed:\n\n'
        printf '  %s\n' "$1"
        printf '  Text: %s\n\n' "$subject"
        printf 'See the commit message policy for this repository.\n'
    } >&2
    exit 1
}

if [ -z "$subject" ]; then
    fail "Subject line cannot be empty"
fi

if [ -n "$max_override" ]; then
    # Fixed-limit mode: no PR-suffix logic.
    max="$max_override"
    suffix_note=""
elif printf '%s\n' "$subject" | grep -qE ' [(]#[0-9]+[)]$'; then
    # PR-suffix-aware mode, trailing " (#NNN)" present: full 50.
    # Use bracket expressions `[(]` and `[)]` for literal parens —
    # POSIX ERE treats backslash-escapes of ordinary characters as
    # undefined, and `\(` / `\)` mean group-anchors in BRE.
    max=50
    suffix_note=""
else
    # PR-suffix-aware mode, no suffix yet: leave 8 chars for GitHub.
    max=42
    suffix_note=" (room for PR suffix)"
fi

len=${#subject}
if [ "$len" -gt "$max" ]; then
    overage=$((len - max))
    fail "Subject: $len chars, limit $max$suffix_note ($overage over)"
fi

printf 'Commit message OK (subject: %s/%s chars)\n' "$len" "$max" >&2
