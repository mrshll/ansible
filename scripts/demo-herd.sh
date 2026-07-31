#!/usr/bin/env bash
#
# The whole herd plugin, in one terminal, in about ten seconds. No Herdr, no
# GitHub, no shared filesystem, no configuration.
#
#   scripts/demo-herd.sh            # run it
#   scripts/demo-herd.sh --keep     # leave the sandbox behind to poke at
#
# What it shows, in order: two members on one hub, the roster's ordering, a
# teleport stream arriving byte for byte, a comment crossing between machines,
# and the receiving side turning that comment into an inbox item. Every step goes
# through the real hub, the real serialization, and the real redactor — the only
# fiction is that the second member's agent sessions are synthetic.
#
# What it cannot show: the parts that need a running Herdr server — status coming
# out of Herdr's own detection, watchers appearing on your Agent sidebar rows, and
# a comment being typed into a live pane. For those, see the walkthrough in
# plugins/herdr-presence/README.md.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

BIN="target/debug/ansible-herd"
if [[ ! -x "$BIN" ]]; then
  printf '==> building\n'
  cargo build -p ansible-herd
fi
BIN="$REPO_ROOT/$BIN"

SANDBOX="$(mktemp -d)"
cleanup() {
  if [[ $KEEP -eq 1 ]]; then
    printf '\nsandbox kept at %s\n' "$SANDBOX"
  else
    rm -rf "$SANDBOX"
  fi
}
trap cleanup EXIT

HUB="$SANDBOX/hub"
mkdir -p "$HUB"

# Two members, each with their own config and state, sharing one hub directory.
# This is exactly the two-machine setup, with the network replaced by /tmp.
member_config() {
  local dir="$1" login="$2" name="$3"
  mkdir -p "$dir/config" "$dir/state"
  cat > "$dir/config/config.toml" <<EOF
login = "$login"
display_name = "$name"
host = "demo-box"

[hub]
kind = "dir"
path = "$HUB"

[share]
default = "title"
allow_submit = false
EOF
}

member_config "$SANDBOX/sam" mrshll Sam
member_config "$SANDBOX/alice" alice Alice

as() {
  local who="$1"
  shift
  HERDR_PLUGIN_CONFIG_DIR="$SANDBOX/$who/config" \
  HERDR_PLUGIN_STATE_DIR="$SANDBOX/$who/state" \
    "$BIN" "$@"
}

# `timeout` runs a program, not a shell function, so the viewer gets its own
# invocation. A viewer runs until its pane is closed — there is no "end of
# session" a watcher can observe — so a bounded run is the demo's stand-in for
# closing the pane.
as_briefly() {
  local who="$1" secs="$2"
  shift 2
  timeout "$secs" env \
    HERDR_PLUGIN_CONFIG_DIR="$SANDBOX/$who/config" \
    HERDR_PLUGIN_STATE_DIR="$SANDBOX/$who/state" \
    "$BIN" "$@" || true
}

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

step "alice publishes three sessions (one blocked with a raised hand, one live)"
as alice demo alice

step "sam's roster — ordering is the product, so read the order"
echo q | as sam roster

step "sam teleports into alice's live session (3s of real frames)"
as_briefly sam 3 watch 3

step "sam comments on the blocked one, anchored at a line"
as sam comment 1 "the RLS enum limitation is in ADR 0003 — carry a bool beside the enum" --line 42

step "alice's daemon runs one tick and delivers it"
# Herdr is not running here, so reconcile and the toast both fail and say so. The
# hub half is independent by design, which is why the mail still lands.
as alice daemon --once || true

step "alice's inbox"
as alice inbox

step "the hub, on disk — one writer per path, and that is the whole design"
find "$HUB" -type f | sed "s#$HUB#<hub>#" | sort

cat <<'EOF'

That was: presence, ordering, a raised hand, live teleport, a comment, and
delivery. With Herdr running, the roster's rows come from its own agent detection
instead of `demo`, the watcher count appears on the owner's sidebar row, and
`inbox <n>` types the comment into the pane.

  plugins/herdr-presence/README.md   install and keybind it
  docs/plan/herdr-plugin.md          why it is shaped like this
EOF
