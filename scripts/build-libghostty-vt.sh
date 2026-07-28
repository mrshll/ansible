#!/usr/bin/env bash
#
# Build libghostty-vt from a pinned Ghostty revision into vendor/libghostty-vt.
#
# libghostty-vt is the cross-platform C library extracted from Ghostty: VT
# parsing, terminal state, render state, and input encoders. It is NOT the
# macOS-only GUI embedding library in include/ghostty.h. See
# docs/spikes/terminal-embedding.md for why the spike uses this one.
#
# Usage: scripts/build-libghostty-vt.sh [--force]
set -euo pipefail

# Pinned to the exact revision this spike built and verified. The vt API is
# documented upstream as unstable and pre-1.0, so a moving branch is not safe.
GHOSTTY_REV="${GHOSTTY_REV:-a60cd15bb5a197d8e2596e86442031cbece06bcc}"
GHOSTTY_REPO="${GHOSTTY_REPO:-https://github.com/ghostty-org/ghostty}"
ZIG_VERSION="${ZIG_VERSION:-0.16.0}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="$REPO_ROOT/vendor"
PREFIX="$VENDOR/libghostty-vt"
SRC="$VENDOR/ghostty-src"
ZIG_DIR="$VENDOR/zig-$ZIG_VERSION"
ZIG="$ZIG_DIR/zig"

if [[ "${1:-}" == "--force" ]]; then rm -rf "$PREFIX"; fi
if [[ -f "$PREFIX/lib/libghostty-vt.a" && -f "$PREFIX/include/ghostty/vt.h" ]]; then
  echo "libghostty-vt already built at $PREFIX (use --force to rebuild)"
  exit 0
fi

mkdir -p "$VENDOR"

if ! command -v "$ZIG" >/dev/null 2>&1; then
  echo "==> Fetching Zig $ZIG_VERSION"
  arch="$(uname -m)"
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) echo "unsupported host OS: $(uname -s)" >&2; exit 1 ;;
  esac
  url="https://ziglang.org/download/$ZIG_VERSION/zig-$arch-$os-$ZIG_VERSION.tar.xz"
  curl -fsSL --retry 3 -o "$VENDOR/zig.tar.xz" "$url"
  tar -xf "$VENDOR/zig.tar.xz" -C "$VENDOR"
  mv "$VENDOR/zig-$arch-$os-$ZIG_VERSION" "$ZIG_DIR"
  rm -f "$VENDOR/zig.tar.xz"
fi

if [[ ! -d "$SRC/.git" ]]; then
  echo "==> Cloning Ghostty @ $GHOSTTY_REV"
  git init -q "$SRC"
  git -C "$SRC" remote add origin "$GHOSTTY_REPO" 2>/dev/null || true
fi
if ! git -C "$SRC" cat-file -e "$GHOSTTY_REV^{commit}" 2>/dev/null; then
  git -C "$SRC" fetch -q --depth 1 origin "$GHOSTTY_REV"
fi
git -C "$SRC" checkout -q "$GHOSTTY_REV"

echo "==> Building libghostty-vt (this takes several minutes)"
cd "$SRC"
build_args=(-Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "$PREFIX")

if ! "$ZIG" build "${build_args[@]}" 2>"$VENDOR/zig-build.log"; then
  # Zig's package fetcher does not work through every corporate/CONNECT proxy.
  # When it cannot reach the dependency hosts, seed the cache with curl/git —
  # which do honor the proxy — and retry. Hashes are still verified by Zig.
  if grep -qE 'ConnectionResetByPeer|ReadFailed|ConnectionRefused|TemporaryNameServerFailure' "$VENDOR/zig-build.log"; then
    echo "==> Zig could not fetch dependencies directly; seeding cache via curl/git"
    "$REPO_ROOT/scripts/seed-zig-cache.sh" "$SRC" "$ZIG" "${build_args[@]}"
  else
    cat "$VENDOR/zig-build.log" >&2
    exit 1
  fi
fi

echo "==> Built:"
ls -1 "$PREFIX/lib" | sed 's/^/    /'
echo "    headers: $PREFIX/include/ghostty/vt.h"
