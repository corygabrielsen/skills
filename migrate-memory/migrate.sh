#!/usr/bin/env bash
#
# /migrate-memory — push current session's memory/ to a target project dir.
# See ~/.claude/skills/migrate-memory/SKILL.md for full contract.

set -o errexit
set -o nounset
set -o pipefail

usage() {
  printf 'Usage: %s <target-cwd> [--source <abs-path>] [--force] [--dry-run]\n' "$0" >&2
  exit 64
}

# slugify: absolute path → project-dir slug (forward slashes → hyphens).
# Reads:   $1 absolute path
# Stdout:  slug
slugify() {
  printf '%s' "$1" | sed 's|/|-|g'
}

# main: parse args, validate, copy memory/.
# Reads:   argv, $CLAUDE_PROJECT_DIR, $PWD, $HOME
# Stdout:  progress + summary
# Returns: 0 ok | 1 empty source | 2 conflict | 64 usage
main() {
  local target_cwd=""
  local source_override=""
  local force=0
  local dry_run=0

  while (($#)); do
    case "$1" in
      --force)   force=1 ;;
      --dry-run) dry_run=1 ;;
      --source)
        shift
        [[ $# -gt 0 ]] || { printf '%s: --source needs a path\n' "$0" >&2; usage; }
        source_override="$1"
        ;;
      -h|--help) usage ;;
      --*)
        printf '%s: unknown flag: %s\n' "$0" "$1" >&2
        usage
        ;;
      *)
        [[ -z "$target_cwd" ]] || { printf '%s: unexpected positional: %s\n' "$0" "$1" >&2; usage; }
        target_cwd="$1"
        ;;
    esac
    shift
  done

  [[ -n "$target_cwd" ]] || usage
  [[ "${target_cwd:0:1}" = "/" ]] || {
    printf '%s: target must be absolute: %s\n' "$0" "$target_cwd" >&2
    exit 64
  }

  local source_cwd
  source_cwd="${source_override:-${CLAUDE_PROJECT_DIR:-$PWD}}"
  [[ "${source_cwd:0:1}" = "/" ]] || {
    printf '%s: source resolved to non-absolute path: %s\n' "$0" "$source_cwd" >&2
    exit 64
  }

  # Canonicalize before slugify so trailing slashes, `..`, and symlink
  # variants don't produce different slugs for the same project dir.
  # `realpath -m` accepts non-existent paths (target may not yet exist
  # in the target project layout, though source must).
  target_cwd=$(realpath -m -- "$target_cwd")
  source_cwd=$(realpath -m -- "$source_cwd")

  local source_slug target_slug source_dir target_dir
  source_slug=$(slugify "$source_cwd")
  target_slug=$(slugify "$target_cwd")
  source_dir="$HOME/.claude/projects/$source_slug/memory"
  target_dir="$HOME/.claude/projects/$target_slug/memory"

  [[ "$source_slug" != "$target_slug" ]] || {
    printf '%s: source and target resolve to the same slug: %s\n' "$0" "$source_slug" >&2
    exit 64
  }

  [[ -d "$source_dir" ]] || {
    printf '%s: no source memory dir: %s\n' "$0" "$source_dir" >&2
    exit 1
  }

  local source_files
  source_files=$(find "$source_dir" -maxdepth 1 -type f -name '*.md' | wc -l | tr -d ' ')
  [[ "$source_files" -gt 0 ]] || {
    printf '%s: source has no .md memory files: %s\n' "$0" "$source_dir" >&2
    exit 1
  }

  printf 'source: %s (%s files)\n' "$source_dir" "$source_files"
  printf 'target: %s\n' "$target_dir"

  if [[ -d "$target_dir" ]]; then
    local conflicts
    conflicts=$(comm -12 \
      <(cd "$source_dir" && find . -maxdepth 1 -type f -name '*.md' -printf '%f\n' | sort) \
      <(cd "$target_dir" && find . -maxdepth 1 -type f -name '*.md' -printf '%f\n' | sort))
    if [[ -n "$conflicts" ]] && [[ "$force" -eq 0 ]]; then
      printf '\nCONFLICT: target has overlapping files:\n%s\n\n' "$conflicts" >&2
      printf 're-run with --force to overwrite\n' >&2
      exit 2
    fi
  fi

  mkdir -p "$target_dir"

  if [[ "$dry_run" -eq 1 ]]; then
    rsync -a --dry-run --itemize-changes "$source_dir/" "$target_dir/"
    printf '\ndry-run complete\n'
    return 0
  fi

  rsync -a "$source_dir/" "$target_dir/"

  local breadcrumb="$target_dir/.migrated-from"
  local breadcrumb_tmp="$breadcrumb.tmp.$$"
  {
    printf 'source_slug: %s\n' "$source_slug"
    printf 'source_cwd: %s\n' "$source_cwd"
    printf 'target_cwd: %s\n' "$target_cwd"
    printf 'timestamp: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  } > "$breadcrumb_tmp"
  mv -f "$breadcrumb_tmp" "$breadcrumb"

  printf '\nmigrated %s files; breadcrumb: %s\n' "$source_files" "$breadcrumb"
}

main "$@"
