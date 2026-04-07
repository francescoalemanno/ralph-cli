#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

section() {
  printf '\n==> %s\n' "$*"
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

shell_files=(
  "test.sh"
  "install"
  "scripts/release.sh"
)

section "Shell syntax"
for file in "${shell_files[@]}"; do
  run bash -n "$file"
done

if command -v shellcheck >/dev/null 2>&1; then
  section "Shell lint"
  run shellcheck "${shell_files[@]}"
else
  section "Shell lint"
  printf 'shellcheck not found; skipping shell lint\n'
fi

section "Format"
run cargo fmt --all --check

section "Typecheck"
run cargo check --workspace --all-targets --all-features

section "Clippy"
run cargo clippy --workspace --all-targets --all-features -- -D warnings

section "Tests"
run cargo test --workspace --all-targets
run cargo test --workspace --doc

section "Docs"
run env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

section "All checks passed"
