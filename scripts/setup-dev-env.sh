#!/usr/bin/env bash
#
# Make a fresh checkout able to build and test the whole workspace.
#
# One definition of "what this repo needs to compile", used by three callers:
# a developer on a new machine, CI (.github/workflows/ci.yml), and the
# SessionStart hook (.claude/hooks/session-start.sh). If they diverged, CI would
# stop predicting whether a laptop can build the tree.
#
# Two crates need system libraries the pure ones do not:
#   ansible-terminal  libclang for bindgen, and libghostty-vt built from source
#   ansible-spike-a   GTK3 + WebKitGTK 4.1 (Tauri v2 on Linux is GTK3)
#
# Idempotent: each step checks for its own result first, so a re-run on a warm
# container is seconds rather than minutes.
#
# Usage: scripts/setup-dev-env.sh [--skip-libghostty]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKIP_LIBGHOSTTY=0
[[ "${1:-}" == "--skip-libghostty" ]] && SKIP_LIBGHOSTTY=1

say() { printf '==> %s\n' "$1"; }

# Debian/Ubuntu only. macOS needs Homebrew equivalents and, per ADR 0001, a
# different surface entirely — see docs/spikes/terminal-embedding.md §6.
if [[ "$(uname -s)" != "Linux" ]] || ! command -v apt-get >/dev/null 2>&1; then
  say "not a Debian-family Linux; skipping package install"
else
  # pkg-config is the honest test of "can the build find these", which is what
  # `cargo build` will ask a moment later. A dpkg query would pass on a machine
  # whose .pc files are missing.
  need_packages=0
  for pkg in gtk+-3.0 webkit2gtk-4.1 cairo pangocairo; do
    pkg-config --exists "$pkg" 2>/dev/null || need_packages=1
  done
  # bindgen reads the libghostty-vt headers through libclang.
  ls /usr/lib/llvm-*/lib/libclang.so* >/dev/null 2>&1 \
    || ls /usr/lib/*/libclang.so* >/dev/null 2>&1 \
    || need_packages=1

  if [[ $need_packages -eq 0 ]]; then
    say "system libraries already present"
  else
    SUDO=""
    [[ "$(id -u)" -ne 0 ]] && SUDO="sudo"
    say "installing system libraries (this is the slow step on a cold container)"
    export DEBIAN_FRONTEND=noninteractive
    $SUDO apt-get update -qq
    $SUDO apt-get install -y --no-install-recommends \
      build-essential pkg-config curl git file xz-utils \
      libgtk-3-dev libwebkit2gtk-4.1-dev libssl-dev librsvg2-dev \
      libclang-dev
  fi
fi

# libghostty-vt is built from a pinned Ghostty revision, not packaged. Without
# it ansible-terminal still compiles — build.rs degrades to a warning and gates
# the native backend behind cfg(have_libghostty_vt) — but the 16 PTY matrix
# tests, which are the ones most likely to regress silently, do not run.
if [[ $SKIP_LIBGHOSTTY -eq 1 ]]; then
  say "skipping libghostty-vt (--skip-libghostty)"
elif [[ -f "$REPO_ROOT/vendor/libghostty-vt/lib/libghostty-vt.a" ]]; then
  say "libghostty-vt already built"
else
  say "building libghostty-vt (several minutes, once per container)"
  "$REPO_ROOT/scripts/build-libghostty-vt.sh"
fi

say "verifying"
"$REPO_ROOT/scripts/check-spike-a-prerequisites.sh"
