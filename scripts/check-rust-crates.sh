#!/bin/bash
# Run a Cargo check against the Rust crates touched by pre-commit.
#
# Usage:
#   check-rust-crates.sh <fmt|clippy|test> [file...]
#
# With file arguments, each path is mapped to its nearest ancestor Cargo.toml.
# With no file arguments, every Cargo.toml within two levels is checked.

set -o errexit
set -o nounset
set -o pipefail

usage() {
    echo "Usage: $0 <fmt|clippy|test> [file...]" >&2
    exit 2
}

if [ $# -lt 1 ]; then
    usage
fi

mode="$1"
shift

case "$mode" in
    fmt | clippy | test) ;;
    *) usage ;;
esac

find_crate_for_path() {
    local path="${1#./}"
    local dir

    if [ -d "$path" ]; then
        dir="$path"
    else
        dir=$(dirname "$path")
    fi

    while [ "$dir" != "." ] && [ "$dir" != "/" ]; do
        if [ -f "$dir/Cargo.toml" ]; then
            printf '%s\n' "$dir"
            return 0
        fi
        dir=$(dirname "$dir")
    done

    return 1
}

declare -A crates=()

if [ $# -gt 0 ]; then
    for path in "$@"; do
        crate=$(find_crate_for_path "$path" || true)
        if [ -n "${crate:-}" ]; then
            crates["$crate"]=1
        fi
    done
else
    while IFS= read -r manifest; do
        crate="${manifest%/Cargo.toml}"
        crate="${crate#./}"
        crates["$crate"]=1
    done < <(find . -maxdepth 2 -name Cargo.toml | sort)
fi

if [ ${#crates[@]} -eq 0 ]; then
    echo "No Rust crates to check."
    exit 0
fi

mapfile -t sorted_crates < <(printf '%s\n' "${!crates[@]}" | sort)

for crate in "${sorted_crates[@]}"; do
    case "$mode" in
        fmt)
            echo "==> $crate: cargo fmt --check"
            (cd "$crate" && cargo fmt --check)
            ;;
        clippy)
            echo "==> $crate: cargo clippy --all-targets --all-features -- -D warnings"
            (cd "$crate" && cargo clippy --all-targets --all-features -- -D warnings)
            ;;
        test)
            echo "==> $crate: cargo test --quiet"
            (cd "$crate" && cargo test --quiet)
            ;;
    esac
done
