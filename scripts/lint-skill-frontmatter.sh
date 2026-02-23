#!/bin/bash
# Validate that every SKILL.md has YAML frontmatter with required fields.
#
# Required fields: name, description
#
# Usage:
#   lint-skill-frontmatter.sh [file...]    # Check specific files
#   lint-skill-frontmatter.sh              # Check all SKILL.md files
#
# Exit codes:
#   0 - All files valid
#   1 - One or more files invalid

set -o errexit
set -o nounset
set -o pipefail

errors=0

check_file() {
    local file="$1"
    local line1 line2 has_name=false has_description=false closed=false

    line1=$(head -n1 "$file")
    if [ "$line1" != "---" ]; then
        echo "FAIL: $file — missing YAML frontmatter (first line is not '---')"
        errors=$((errors + 1))
        return
    fi

    # Scan frontmatter block for required fields
    local lineno=0
    while IFS= read -r line; do
        lineno=$((lineno + 1))
        [ "$lineno" -eq 1 ] && continue  # skip opening ---
        [ "$line" = "---" ] && closed=true && break
        case "$line" in
            name:*) has_name=true ;;
            description:*) has_description=true ;;
        esac
    done < "$file"

    if ! $closed; then
        echo "FAIL: $file — frontmatter block never closed (missing closing '---')"
        errors=$((errors + 1))
        return
    fi

    local missing=()
    $has_name || missing+=("name")
    $has_description || missing+=("description")

    if [ ${#missing[@]} -gt 0 ]; then
        echo "FAIL: $file — missing fields: ${missing[*]}"
        errors=$((errors + 1))
    fi
}

# Collect files to check
if [ $# -gt 0 ]; then
    files=("$@")
else
    mapfile -t files < <(find . -maxdepth 2 -name 'SKILL.md' | sort)
fi

for file in "${files[@]}"; do
    check_file "$file"
done

if [ "$errors" -gt 0 ]; then
    echo ""
    echo "$errors file(s) failed frontmatter validation."
    exit 1
fi

echo "All ${#files[@]} skill(s) have valid frontmatter."
exit 0
