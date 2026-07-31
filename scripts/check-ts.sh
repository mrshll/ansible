#!/usr/bin/env bash
#
# The single definition of "the TypeScript in this tree is clean". Mirrors
# scripts/lint.sh, which is the same thing for the Rust half, and lint.sh calls
# this so there is one bar and no way for local and CI to disagree.
#
#   scripts/check-ts.sh          # check only; fails on the first problem
#   scripts/check-ts.sh --fix    # apply what can be applied, then re-check
#
# Three tools, three jobs:
#
#   oxfmt   formatting, code only — prose in this repo is hand-wrapped, so
#           markdown is excluded in .oxfmtrc.json
#   oxlint  correctness, suspicious, perf, and pedantic as errors; see
#           .oxlintrc.json for the handful of rules turned off and why
#   tsc     TypeScript 7, per package, because the three packages target three
#           different runtimes and cannot share a module resolution mode:
#             services/hub    bundled to wasm by `spacetime publish`  → bundler
#             transcript-worker  bundled by wrangler                  → bundler
#             plugins/herd    run by node as argv commands            → nodenext
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

FIX=0
for arg in "$@"; do
  case "$arg" in
    --fix) FIX=1 ;;
    -h | --help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'check-ts.sh: unknown argument: %s\n' "$arg" >&2
      exit 2
      ;;
  esac
done

if [[ ! -d node_modules ]]; then
  printf 'check-ts.sh: run `npm install` first\n' >&2
  exit 1
fi

step() { printf '\033[1m==> %s\033[0m\n' "$1"; }

if [[ $FIX -eq 1 ]]; then
  step 'oxfmt'
  npx oxfmt .
  step 'oxlint --fix'
  npx oxlint --fix
fi

step 'oxfmt --check'
npx oxfmt --check .

step 'oxlint'
npx oxlint

for project in services/hub services/transcript-worker plugins/herd; do
  if [[ -f "$project/tsconfig.json" ]]; then
    step "tsc -p $project"
    npx tsc -p "$project"
  fi
done

step 'vitest run'
npx vitest run --passWithNoTests

printf '\033[32mtypescript: clean\033[0m\n'
