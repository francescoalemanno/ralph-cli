#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./to-exp-branch.sh [--branch NAME] [--message MESSAGE]

Creates an experimental branch from the current HEAD, stages every change with
git add -A, and commits them with a Conventional Commits v1 message.

Options:
  --branch NAME    Use a specific branch name instead of exp/<current>-<timestamp>
  --message TEXT   Use an explicit commit message instead of the generated one
  -h, --help       Show this help text
EOF
}

fail() {
  echo "error: $*" >&2
  exit 1
}

sanitize_branch_part() {
  printf '%s' "$1" | sed -E 's#[^A-Za-z0-9._-]+#-#g; s#^[.-]+##; s#[.-]+$##; s#-+#-#g'
}

unique_branch_name() {
  local base="$1"
  local candidate="$base"
  local suffix=1

  while git rev-parse --verify --quiet "refs/heads/$candidate" >/dev/null; do
    candidate="${base}-${suffix}"
    suffix=$((suffix + 1))
  done

  printf '%s\n' "$candidate"
}

scope_for_path() {
  case "$1" in
    crates/ralph-*/*)
      local crate="${1#crates/ralph-}"
      printf '%s\n' "${crate%%/*}"
      ;;
    docs/*|*.md)
      printf 'docs\n'
      ;;
    .github/*)
      printf 'ci\n'
      ;;
    Cargo.toml|Cargo.lock|install|.cargo/*)
      printf 'workspace\n'
      ;;
    *)
      printf 'workspace\n'
      ;;
  esac
}

contains_item() {
  local needle="$1"
  shift
  local item

  for item in "$@"; do
    if [[ "$item" == "$needle" ]]; then
      return 0
    fi
  done

  return 1
}

is_docs_path() {
  case "$1" in
    docs/*|*.md|LICENSE)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_ci_path() {
  case "$1" in
    .github/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_build_path() {
  case "$1" in
    Cargo.toml|Cargo.lock|crates/*/Cargo.toml|install|.cargo/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_test_path() {
  case "$1" in
    tests/*|*/tests/*|*.snap|*.golden|*.fixture)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_source_path() {
  case "$1" in
    crates/*/src/*|src/*|*.rs|*.toml|*.sh)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

detect_commit_type() {
  local docs_only="$1"
  local ci_only="$2"
  local build_only="$3"
  local test_only="$4"
  local has_source="$5"
  local has_additions="$6"
  local has_deletions="$7"
  local additions="$8"
  local deletions="$9"

  if [[ "$docs_only" -eq 1 ]]; then
    printf 'docs\n'
    return
  fi

  if [[ "$ci_only" -eq 1 ]]; then
    printf 'ci\n'
    return
  fi

  if [[ "$build_only" -eq 1 && "$has_source" -eq 0 ]]; then
    printf 'build\n'
    return
  fi

  if [[ "$test_only" -eq 1 && "$has_source" -eq 0 ]]; then
    printf 'test\n'
    return
  fi

  if [[ "$has_source" -eq 1 ]]; then
    if [[ "$has_deletions" -eq 1 && "$deletions" -ge "$additions" ]]; then
      printf 'refactor\n'
      return
    fi

    if [[ "$has_additions" -eq 1 || "$additions" -gt "$deletions" ]]; then
      printf 'feat\n'
      return
    fi

    printf 'refactor\n'
    return
  fi

  printf 'chore\n'
}

subject_for_type() {
  local type="$1"
  local scope="$2"

  case "$type" in
    docs)
      printf 'update documentation\n'
      ;;
    ci)
      printf 'update automation workflows\n'
      ;;
    build)
      if [[ "$scope" == "workspace" ]]; then
        printf 'update workspace metadata\n'
      else
        printf 'update %s build metadata\n' "$scope"
      fi
      ;;
    test)
      printf 'update test coverage\n'
      ;;
    feat)
      if [[ "$scope" == "workspace" ]]; then
        printf 'advance experimental changes\n'
      else
        printf 'advance %s changes\n' "$scope"
      fi
      ;;
    refactor)
      if [[ "$scope" == "workspace" ]]; then
        printf 'rework internal structure\n'
      else
        printf 'rework %s internals\n' "$scope"
      fi
      ;;
    *)
      printf 'checkpoint experimental changes\n'
      ;;
  esac
}

format_commit_message() {
  local type="$1"
  local scope="$2"
  local subject="$3"

  if [[ "$scope" == "workspace" || "$scope" == "$type" ]]; then
    printf '%s: %s\n' "$type" "$subject"
    return
  fi

  printf '%s(%s): %s\n' "$type" "$scope" "$subject"
}

main() {
  local branch_name_override=""
  local commit_message_override=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --branch)
        [[ $# -ge 2 ]] || fail "--branch requires a value"
        branch_name_override="$2"
        shift 2
        ;;
      --message)
        [[ $# -ge 2 ]] || fail "--message requires a value"
        commit_message_override="$2"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "unknown argument: $1"
        ;;
    esac
  done

  git rev-parse --show-toplevel >/dev/null 2>&1 || fail "this script must run inside a git repository"

  local repo_root
  repo_root="$(git rev-parse --show-toplevel)"
  cd "$repo_root"

  if [[ -z "$(git status --short)" ]]; then
    fail "working tree is clean; nothing to branch and commit"
  fi

  local current_branch safe_branch_part timestamp branch_name
  current_branch="$(git branch --show-current)"
  if [[ -z "$current_branch" ]]; then
    current_branch="detached-head"
  fi

  safe_branch_part="$(sanitize_branch_part "$current_branch")"
  [[ -n "$safe_branch_part" ]] || safe_branch_part="worktree"
  timestamp="$(date +%Y%m%d-%H%M%S)"

  if [[ -n "$branch_name_override" ]]; then
    branch_name="$branch_name_override"
  else
    branch_name="$(unique_branch_name "exp/${safe_branch_part}-${timestamp}")"
  fi

  git switch -c "$branch_name"
  git add -A

  local changed_files=()
  local file
  while IFS= read -r -d '' file; do
    changed_files+=("$file")
  done < <(git diff --cached --name-only -z)

  [[ "${#changed_files[@]}" -gt 0 ]] || fail "no staged changes found after git add -A"

  local docs_only=1
  local ci_only=1
  local build_only=1
  local test_only=1
  local has_source=0
  local scopes=()

  for file in "${changed_files[@]}"; do
    if ! is_docs_path "$file"; then
      docs_only=0
    fi

    if ! is_ci_path "$file"; then
      ci_only=0
    fi

    if ! is_build_path "$file"; then
      build_only=0
    fi

    if ! is_test_path "$file"; then
      test_only=0
    fi

    if is_source_path "$file" && ! is_docs_path "$file" && ! is_ci_path "$file"; then
      has_source=1
    fi

    local scope
    scope="$(scope_for_path "$file")"
    if [[ "${#scopes[@]}" -eq 0 ]] || ! contains_item "$scope" "${scopes[@]}"; then
      scopes+=("$scope")
    fi
  done

  local additions=0
  local deletions=0
  local added deleted count_line add_count del_count
  while IFS=$'\t' read -r add_count del_count count_line; do
    if [[ "$add_count" == "-" || "$del_count" == "-" ]]; then
      continue
    fi

    additions=$((additions + add_count))
    deletions=$((deletions + del_count))
  done < <(git diff --cached --numstat)

  added=0
  deleted=0
  if git diff --cached --name-only --diff-filter=A | grep -q '.'; then
    added=1
  fi
  if git diff --cached --name-only --diff-filter=D | grep -q '.'; then
    deleted=1
  fi

  local scope="workspace"
  if [[ "${#scopes[@]}" -eq 1 ]]; then
    scope="${scopes[0]}"
  fi

  local subject commit_type commit_message
  commit_type="$(detect_commit_type "$docs_only" "$ci_only" "$build_only" "$test_only" "$has_source" "$added" "$deleted" "$additions" "$deletions")"
  subject="$(subject_for_type "$commit_type" "$scope")"

  if [[ -n "$commit_message_override" ]]; then
    commit_message="$commit_message_override"
  else
    commit_message="$(format_commit_message "$commit_type" "$scope" "$subject")"
  fi

  local commit_file
  commit_file="$(mktemp)"

  {
    printf '%s\n' "$commit_message"
    printf '\n'
    printf 'Touched files:\n'

    local index=0
    for file in "${changed_files[@]}"; do
      if [[ "$index" -ge 8 ]]; then
        printf -- '- ... and %d more\n' "$(( ${#changed_files[@]} - index ))"
        break
      fi

      printf -- '- %s\n' "$file"
      index=$((index + 1))
    done

    printf '\n'
    printf 'Stats: %d files changed, %d insertions(+), %d deletions(-)\n' "${#changed_files[@]}" "$additions" "$deletions"
  } >"$commit_file"

  git commit -F "$commit_file"
  rm -f "$commit_file"

  echo "Created branch: $branch_name"
  echo "Committed with: $commit_message"
}

main "$@"
