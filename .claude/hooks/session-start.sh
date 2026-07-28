#!/usr/bin/env bash
#
# SessionStart hook: leave a cloud session with a workspace that can run
# `cargo test --workspace`.
#
# Without this, a fresh container fails at `cargo build` on a missing gdk-3.0,
# and has no libghostty-vt — so the terminal crate silently drops its native
# backend and 16 PTY tests do not run. Both are easy to mistake for "the tests
# pass".
#
# Local machines are left alone: a developer who has set their environment up
# some other way should not have this run behind their back. Run
# scripts/setup-dev-env.sh directly instead.
set -euo pipefail

if [[ "${CLAUDE_CODE_REMOTE:-}" != "true" ]]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
exec ./scripts/setup-dev-env.sh
