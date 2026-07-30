#!/usr/bin/env bash
#
# Prove the AwaitingApproval producer against a real Claude Code session.
#
#   scripts/probe-approval.sh [outdir]
#
# Drives a real interactive `claude` to a real permission prompt through the
# embedded terminal, feeds both a real hook overlay and the screen detector into
# one StatusMachine, and asserts the three claims the grid's highest-value signal
# rests on: a prompt is seen promptly, answering it clears, and a slow legitimate
# tool never trips it. Exit status is the result.
#
# It also records the screens it saw. Those are the fixtures
# crates/ansible-hooks/tests/fixtures/screens/ holds, and re-running this is how
# you re-record them after a Claude Code upgrade — the diff then shows exactly
# which part of the TUI moved.
#
# Requires `claude` on PATH with interactive credentials, python3 for the hook
# receiver, and a built libghostty-vt (scripts/build-libghostty-vt.sh).
#
# NOTE: this needs a machine where a permission prompt actually appears. A
# container that forces a permissive permission mode cannot run it — see
# docs/plan/w1-provisioning.md §7 and docs/spikes/hook-coverage.md §5.
set -euo pipefail

OUT="${1:-$(mktemp -d)}"
mkdir -p "$OUT/work"

for tool in claude python3; do
  command -v "$tool" >/dev/null || {
    echo "$tool is not on PATH" >&2
    exit 1
  }
done

# A fresh working directory, so the session has no allowlist of its own and the
# file it is asked to create does not already exist. Both would remove the
# prompt this probe exists to observe.
rm -f "$OUT/work/probe.txt"

echo "==> driving a real session (this takes a few minutes)"
# The example prints the re-recording instructions itself, so they cannot drift
# from what it actually wrote.
exec cargo run -q -p ansible-terminal --example approval-probe -- \
  --out "$OUT" --cwd "$OUT/work"
