#!/usr/bin/env bash
#
# Point this clone's git hooks at the versioned .githooks directory.
#
#   scripts/install-hooks.sh             # install
#   scripts/install-hooks.sh --uninstall # go back to .git/hooks
#
# core.hooksPath is per-clone and cannot be committed, so this has to be run
# once per checkout. The upside of the hooksPath approach over copying files into
# .git/hooks is that the hooks stay under version control and reviewed: an update
# to .githooks/pre-commit takes effect on the next pull, with no reinstall.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ "${1:-}" == "--uninstall" ]]; then
  git config --unset core.hooksPath || true
  printf 'hooks: uninstalled (core.hooksPath cleared)\n'
  exit 0
fi

# Keep the bits right for anyone who checked out on a filesystem that drops them.
chmod +x .githooks/* scripts/*.sh

git config core.hooksPath .githooks

cat <<'EOF'
hooks: installed (core.hooksPath = .githooks)

  pre-commit  runs scripts/lint.sh when a commit touches Rust or Cargo files

  scripts/lint.sh --fix       fix formatting and mechanical lints
  git commit --no-verify      bypass for one commit
  scripts/install-hooks.sh --uninstall
EOF
