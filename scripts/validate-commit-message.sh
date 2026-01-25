#!/bin/bash
# Validate commit message follows the 50/72 rule.
#
# Rules enforced:
#   - Subject line ≤50 characters
#   - Body wrapped at 72 characters
#     (Exempt: code blocks, tables, URLs, indented code, blockquotes)
#
# Usage:
#   validate-commit-message.sh <file>   # Read from file (strips # comments)
#   validate-commit-message.sh -        # Read from stdin (no comment stripping)
#
# Exit codes:
#   0 - Valid
#   1 - Invalid

set -o errexit
set -o nounset
set -o pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <commit-msg-file|->" >&2
    exit 1
fi

INPUT="$1"

# Read commit message
if [ "$INPUT" = "-" ]; then
    COMMIT_MSG=$(cat)
elif [ -f "$INPUT" ]; then
    # Strip comment lines (git's default commit template has them)
    COMMIT_MSG=$(grep -v '^#' "$INPUT" || true)
else
    echo "Error: File not found: $INPUT" >&2
    exit 1
fi

# Split into subject and body
SUBJECT=$(printf '%s\n' "$COMMIT_MSG" | head -n1)
BODY=$(printf '%s\n' "$COMMIT_MSG" | tail -n +3)  # Skip subject and blank line

subject_errors=()
body_errors=()

# === SUBJECT LINE CHECK ===
subject_len=${#SUBJECT}
max_len=50

if [ -z "$SUBJECT" ]; then
    subject_errors+=("Subject line cannot be empty")
elif [ "$subject_len" -gt "$max_len" ]; then
    overage=$((subject_len - max_len))
    subject_errors+=("Subject: $subject_len chars, limit $max_len ($overage over)")
fi

# === BODY LINE CHECK ===
if [ -n "$BODY" ]; then
    line_num=0
    in_code_block=false

    while IFS= read -r line || [ -n "$line" ]; do
        line_num=$((line_num + 1))
        line_len=${#line}

        # Track code block state
        if [[ "$line" =~ ^\`\`\` ]]; then
            if $in_code_block; then in_code_block=false; else in_code_block=true; fi
            continue
        fi

        # Skip exempt lines
        $in_code_block && continue
        [[ "$line" =~ ^[[:space:]]*\| ]] && continue      # Markdown table
        [[ "$line" =~ https?:// ]] && continue            # Contains URL
        [[ "$line" =~ ^[[:space:]]{4} ]] && continue      # Indented code
        [[ "$line" =~ ^$'\t' ]] && continue               # Tab-indented code
        [[ "$line" =~ ^\> ]] && continue                  # Blockquote

        if [ "$line_len" -gt 72 ]; then
            body_errors+=("Line $line_num: $line_len chars")
        fi
    done <<< "$BODY"
fi

# === OUTPUT ===
if [ ${#subject_errors[@]} -eq 0 ] && [ ${#body_errors[@]} -eq 0 ]; then
    echo "Commit message OK (subject: $subject_len/$max_len chars)"
    exit 0
fi

echo "Commit message validation failed (50/72 rule):"
echo ""

if [ ${#subject_errors[@]} -gt 0 ]; then
    for err in "${subject_errors[@]}"; do
        echo "  $err"
    done
    echo "  Text: $SUBJECT"
    echo ""
fi

if [ ${#body_errors[@]} -gt 0 ]; then
    echo "  Body lines exceeding 72 chars:"
    for err in "${body_errors[@]}"; do
        echo "    $err"
    done
    echo ""
fi

exit 1
