#!/usr/bin/env bash
#
# Build and run the Spike A harness.
#
#   scripts/run-spike-a.sh                      # runs $SHELL in the terminal
#   scripts/run-spike-a.sh claude               # runs a real Claude Code session
#   scripts/run-spike-a.sh /bin/sh /tmp/demo.sh # runs a fixture script
#
# On a machine without a display, wrap it: xvfb-run -a scripts/run-spike-a.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

"$REPO_ROOT/scripts/build-libghostty-vt.sh"

# bindgen needs to find libclang to read the libghostty-vt headers.
if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  for dir in /usr/lib/llvm-*/lib /usr/lib/*/; do
    if [[ -e "$dir/libclang.so" || -e "$dir/libclang.so.1" ]]; then
      export LIBCLANG_PATH="$dir"
      break
    fi
  done
fi

if [[ $# -gt 0 ]]; then
  export ANSIBLE_TERMINAL_COMMAND="$1"
  shift
  export ANSIBLE_TERMINAL_ARGS="$*"
fi

# WebKitGTK's accelerated compositing needs a GPU; without one it spams EGL
# errors and can fail to paint. Harmless to set unconditionally for a spike.
export WEBKIT_DISABLE_COMPOSITING_MODE="${WEBKIT_DISABLE_COMPOSITING_MODE:-1}"
export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"

exec cargo run -p ansible-spike-a "$@"
