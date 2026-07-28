#!/usr/bin/env bash
#
# The single definition of "this tree is clean". The pre-commit hook and CI both
# call this, so there is one bar and no way for local and CI to disagree.
#
#   scripts/lint.sh          # check only; fails on the first problem
#   scripts/lint.sh --fix    # apply what can be applied, then re-check
#
# Every clippy warning is an error here (`-D warnings`) while staying a warning
# during normal `cargo build`. That keeps iteration quiet without letting a
# warning reach main. The lint set itself lives in the `[lints]` tables in
# Cargo.toml, not in this file — see the root Cargo.toml for what and why.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

FIX=0
for arg in "$@"; do
  case "$arg" in
    --fix) FIX=1 ;;
    -h | --help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'lint.sh: unknown argument: %s\n' "$arg" >&2
      exit 2
      ;;
  esac
done

# bindgen needs libclang to read the libghostty-vt headers, and clippy has to
# build `ansible-terminal`'s build script like any other build. Same detector as
# run-spike-a.sh: picking the first libclang on the machine lands on ones that
# ship without Clang's builtin headers. If none qualifies, leave it unset and let
# bindgen search for itself, which is the working path on macOS.
if [[ -z "${LIBCLANG_PATH:-}" ]] && dir="$(scripts/detect-libclang.sh)"; then
  export LIBCLANG_PATH="$dir"
fi

step() { printf '\033[1m==> %s\033[0m\n' "$1"; }

if [[ $FIX -eq 1 ]]; then
  step 'cargo fmt --all'
  cargo fmt --all

  # `clippy --fix` only applies suggestions clippy marks machine-applicable, so
  # it cannot silence a lint by rewriting the meaning of the code. Anything left
  # over is a judgement call and shows up in the check below.
  step 'cargo clippy --fix'
  cargo clippy --fix --workspace --all-targets --allow-dirty --allow-staged
fi

step 'cargo fmt --all --check'
cargo fmt --all --check

# --all-targets so tests, examples, and benches are held to the same bar as the
# library. Lint debt hides in test code otherwise.
step 'cargo clippy --workspace --all-targets -- -D warnings'
cargo clippy --workspace --all-targets -- -D warnings

printf '\033[32mlint: clean\033[0m\n'
