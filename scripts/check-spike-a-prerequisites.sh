#!/usr/bin/env bash
#
# Report whether this machine can build and run the Spike A harness.
#
# Exits non-zero only when something required is missing. Optional items are
# reported but never fail the check, so the script is usable in CI and on a
# developer laptop alike.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
missing=0

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
ok()   { printf '  \033[32m✓\033[0m %-26s %s\n' "$1" "${2-}"; }
warn() { printf '  \033[33m!\033[0m %-26s %s\n' "$1" "${2-}"; }
bad()  { printf '  \033[31m✗\033[0m %-26s %s\n' "$1" "${2-}"; missing=1; }

need_command() {
  local name="$1" hint="${2-}"
  if command -v "$name" >/dev/null 2>&1; then
    ok "$name" "$("$name" --version 2>&1 | head -1)"
  else
    bad "$name" "$hint"
  fi
}

want_command() {
  local name="$1" hint="${2-}"
  if command -v "$name" >/dev/null 2>&1; then
    ok "$name" "$("$name" --version 2>&1 | head -1)"
  else
    warn "$name" "$hint"
  fi
}

need_pkg() {
  local pkg="$1" hint="${2-}"
  if pkg-config --exists "$pkg" 2>/dev/null; then
    ok "$pkg" "$(pkg-config --modversion "$pkg")"
  else
    bad "$pkg" "$hint"
  fi
}

# bindgen needs libclang *and* Clang's builtin headers, because glibc's
# <limits.h> uses #include_next to reach Clang's copy. They travel separately
# on Ubuntu, so check the pair on whichever directory the build will load —
# not on any directory that happens to have one half.
need_libclang() {
  local dir include
  if [[ -n "${LIBCLANG_PATH:-}" ]]; then
    for include in "$LIBCLANG_PATH"/clang/*/include; do
      if [[ -f "$include/limits.h" && -f "$include/stddef.h" ]]; then
        ok "libclang" "$LIBCLANG_PATH (from LIBCLANG_PATH)"
        return
      fi
    done
    bad "libclang" "LIBCLANG_PATH=$LIBCLANG_PATH has no Clang builtin headers"
    return
  fi
  if dir="$("$REPO_ROOT/scripts/detect-libclang.sh")"; then
    ok "libclang" "$dir"
  else
    bad "libclang" "apt install libclang-dev (or set LIBCLANG_PATH)"
  fi
}

bold "Toolchain"
need_command cargo "install from https://rustup.rs"
need_command rustc "install from https://rustup.rs"
need_command git   "required to fetch the pinned Ghostty revision"
need_command curl  "required to fetch the Zig toolchain"

# Zig builds libghostty-vt. scripts/build-libghostty-vt.sh vendors its own copy,
# so a system Zig is convenient but not required.
if [[ -x "$REPO_ROOT/vendor/zig-0.16.0/zig" ]]; then
  ok "zig (vendored)" "$("$REPO_ROOT/vendor/zig-0.16.0/zig" version 2>&1)"
else
  want_command zig "scripts/build-libghostty-vt.sh will fetch Zig 0.16.0 if absent"
fi

bold "Native libraries"
# On macOS bindgen finds Xcode's libclang on its own; the detector only knows
# where Linux distributions put theirs.
if [[ "$(uname -s)" == "Linux" ]]; then
  need_libclang
fi

case "$(uname -s)" in
  Linux)
    # Tauri v2 on Linux is GTK3 + WebKitGTK 4.1. The terminal surface is a GTK3
    # widget drawn with Cairo and Pango.
    need_pkg gtk+-3.0        "apt install libgtk-3-dev"
    need_pkg webkit2gtk-4.1  "apt install libwebkit2gtk-4.1-dev"
    need_pkg cairo           "apt install libcairo2-dev"
    need_pkg pangocairo      "apt install libpango1.0-dev"
    want_command Xvfb        "needed only to run the GUI harness headlessly"
    ;;
  Darwin)
    warn "macOS" "the GTK harness is Linux-only; see docs/spikes/terminal-embedding.md"
    ;;
esac

bold "libghostty-vt"
PREFIX="${LIBGHOSTTY_VT_DIR:-$REPO_ROOT/vendor/libghostty-vt}"
if [[ -f "$PREFIX/include/ghostty/vt.h" ]]; then
  ok "headers" "$PREFIX/include/ghostty/vt.h"
else
  warn "headers" "run scripts/build-libghostty-vt.sh"
fi
if [[ -f "$PREFIX/lib/libghostty-vt.a" || -f "$PREFIX/lib/libghostty-vt.so" ]]; then
  ok "library" "$PREFIX/lib"
else
  warn "library" "run scripts/build-libghostty-vt.sh"
fi

bold "Session command"
if command -v claude >/dev/null 2>&1; then
  ok "claude" "the harness can run a real Claude Code session"
else
  warn "claude" "not installed; the harness falls back to \$SHELL"
fi

echo
if [[ $missing -ne 0 ]]; then
  echo "Missing required prerequisites. See the ✗ entries above."
  exit 1
fi
echo "All required prerequisites are present."
