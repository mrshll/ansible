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

# The terminal child should start where the user invoked us, not where cargo
# has to run from. Capture that before the cd below; without it portable-pty
# falls back to $HOME, so `run-spike-a.sh claude` would open a session against
# the home directory instead of the project the user is sitting in.
export ANSIBLE_TERMINAL_CWD="${ANSIBLE_TERMINAL_CWD:-$PWD}"

cd "$REPO_ROOT"

"$REPO_ROOT/scripts/build-libghostty-vt.sh"

# bindgen needs to find libclang to read the libghostty-vt headers. Leave the
# choice to the detector: picking the first libclang on the machine lands on
# ones that ship without Clang's builtin headers, and bindgen then dies on
# glibc's <limits.h>. If nothing qualifies, leave LIBCLANG_PATH unset and let
# bindgen search for itself; that is the working path on macOS.
if [[ -z "${LIBCLANG_PATH:-}" ]] && dir="$("$REPO_ROOT/scripts/detect-libclang.sh")"; then
  export LIBCLANG_PATH="$dir"
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
