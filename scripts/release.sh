#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/release.sh <patch|minor|major|X.Y.Z> [--no-push]

Creates a release commit and tag from a clean main branch by:
1. Fetching origin and tags
2. Bumping the workspace version
3. Syncing internal crate dependency versions
4. Refreshing Cargo.lock
5. Running formatting, clippy, tests, and a locked CLI check
6. Creating the release commit and tag
7. Pushing main and the tag to origin

Examples:
  scripts/release.sh patch
  scripts/release.sh minor
  scripts/release.sh 0.2.0
  scripts/release.sh patch --no-push
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

log() {
  echo "==> $*"
}

require() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

run() {
  "$@"
}

current_workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && /^version = / {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.toml
}

normalize_version() {
  local raw="$1"
  raw="${raw#v}"
  [[ "$raw" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid version: $1"
  printf '%s\n' "$raw"
}

version_gt() {
  local left="$1" right="$2"
  local la lb lc ra rb rc
  IFS=. read -r la lb lc <<<"$left"
  IFS=. read -r ra rb rc <<<"$right"

  if (( la != ra )); then
    (( la > ra ))
    return
  fi
  if (( lb != rb )); then
    (( lb > rb ))
    return
  fi
  (( lc > rc ))
}

bump_version() {
  local current="$1" spec="$2"
  local major minor patch
  IFS=. read -r major minor patch <<<"$current"

  case "$spec" in
    patch)
      printf '%s.%s.%s\n' "$major" "$minor" "$((patch + 1))"
      ;;
    minor)
      printf '%s.%s.0\n' "$major" "$((minor + 1))"
      ;;
    major)
      printf '%s.0.0\n' "$((major + 1))"
      ;;
    *)
      normalize_version "$spec"
      ;;
  esac
}

ensure_clean_worktree() {
  if ! git diff --quiet --ignore-submodules -- || ! git diff --cached --quiet --ignore-submodules --; then
    git status --short >&2 || true
    die "working tree must be clean before creating a release"
  fi

  if [[ -n "$(git ls-files --others --exclude-standard)" ]]; then
    git status --short >&2 || true
    die "untracked files present; clean the working tree before creating a release"
  fi
}

ensure_main_branch() {
  local branch
  branch="$(git branch --show-current)"
  [[ "$branch" == "main" ]] || die "release script must run from main; current branch is '$branch'"
}

ensure_origin_state() {
  local behind ahead

  log "Fetching origin"
  run git fetch origin main --tags

  read -r behind ahead <<<"$(git rev-list --left-right --count origin/main...HEAD)"
  if (( behind > 0 )); then
    die "local main is behind origin/main; rebase or fast-forward before releasing"
  fi

  if (( ahead > 0 )); then
    log "main is ahead of origin/main by ${ahead} commit(s); the release push will include them"
  fi
}

ensure_tag_is_free() {
  local tag="$1"

  if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
    die "tag already exists locally: ${tag}"
  fi

  if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
    die "tag already exists on origin: ${tag}"
  fi
}

update_workspace_version() {
  local next_version="$1"

  NEXT_VERSION="$next_version" perl -0pi -e '
    s/(\[workspace\.package\]\n(?:.*\n)*?version = ")([^"]+)(")/$1$ENV{NEXT_VERSION}$3/s
      or die "failed to update workspace version in Cargo.toml\n";
  ' Cargo.toml
}

sync_internal_dependency_versions() {
  local next_version="$1"

  NEXT_VERSION="$next_version" perl -pi -e '
    s/(ralph-[a-z-]+\s*=\s*\{[^}]*version = ")([^"]+)(".*path = "\.\.\/[^"]+".*\})/$1$ENV{NEXT_VERSION}$3/
  ' crates/*/Cargo.toml
}

main() {
  local version_spec="${1:-}"
  local push_remote=1
  local current_version next_version tag

  [[ -n "$version_spec" ]] || {
    usage >&2
    exit 1
  }

  shift || true
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-push)
        push_remote=0
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
    shift
  done

  require git
  require cargo
  require perl

  local script_dir repo_root
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  repo_root="$(cd "${script_dir}/.." && pwd)"
  cd "$repo_root"

  ensure_clean_worktree
  ensure_main_branch
  ensure_origin_state

  current_version="$(current_workspace_version)"
  [[ -n "$current_version" ]] || die "failed to read workspace version from Cargo.toml"

  next_version="$(bump_version "$current_version" "$version_spec")"
  version_gt "$next_version" "$current_version" || die "new version must be greater than current version (${current_version})"
  tag="v${next_version}"

  ensure_tag_is_free "$tag"

  log "Bumping version ${current_version} -> ${next_version}"
  update_workspace_version "$next_version"
  sync_internal_dependency_versions "$next_version"

  log "Refreshing Cargo.lock"
  run cargo check -p ralph-cli >/dev/null

  log "Running cargo fmt --check"
  run cargo fmt --check

  log "Running cargo clippy"
  run cargo clippy --workspace --all-targets --all-features -- -D warnings

  log "Running cargo test --locked"
  run cargo test --locked

  log "Running cargo check --locked -p ralph-cli"
  run cargo check --locked -p ralph-cli

  log "Creating release commit"
  run git add Cargo.toml Cargo.lock crates/*/Cargo.toml
  run git commit \
    -m "chore(release): bump version to ${next_version}" \
    -m "Bump version from ${current_version} to ${next_version} for release."

  log "Creating tag ${tag}"
  run git tag "${tag}"

  if (( push_remote == 1 )); then
    log "Pushing main"
    run git push origin main
    log "Pushing ${tag}"
    run git push origin "${tag}"
  else
    log "Skipping push because --no-push was provided"
  fi

  log "Release ${tag} is ready"
}

main "$@"
