#!/usr/bin/env bash
#
# Record real Herdr socket responses, so the parsers in crates/ansible-herd stop
# being doc-derived.
#
#   scripts/capture-herdr-fixtures.sh [outdir]
#
# Produces, in outdir (default: a temp dir):
#   api-schema.json        the whole protocol schema the installed binary carries
#   session-snapshot.json  the bootstrap response the daemon reconciles from
#   agent-list.json        the fallback the daemon uses when snapshot is missing
#   pane-list.json         pane records, including tokens and terminal titles
#   agent-explain.json     how Herdr classified the focused pane, and why
#   events.ndjson          ~10s of pushed events from the subscriptions we use
#   observe.ndjson         ~3s of terminal.frame records, the teleport source
#   versions.txt           herdr --version and status, so a diff has provenance
#
# Why this exists: `crates/ansible-herd/src/herdr.rs` and `teleport.rs` were
# written against herdr.dev's documentation, not against a running server. They
# probe field names with fallbacks and tolerate absence for exactly that reason,
# and their tests use fixtures shaped from prose. This script is how the first
# person with Herdr installed replaces prose with observation — the same move
# scripts/capture-hook-payloads.sh made for Claude Code's hook payloads, where
# recording found that four of twelve assumptions were wrong.
#
# Requires: a running Herdr server with at least one agent pane, `herdr` on PATH,
# and python3 for the socket reads.
set -euo pipefail

OUT="${1:-$(mktemp -d)}"
mkdir -p "$OUT"

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"
command -v "$HERDR_BIN" >/dev/null || {
  printf 'capture: %s is not on PATH\n' "$HERDR_BIN" >&2
  exit 1
}

SOCKET="${HERDR_SOCKET_PATH:-}"
if [[ -z "$SOCKET" ]]; then
  CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
  if [[ -n "${HERDR_SESSION:-}" ]]; then
    SOCKET="$CONFIG_HOME/herdr/sessions/$HERDR_SESSION/herdr.sock"
  else
    SOCKET="$CONFIG_HOME/herdr/herdr.sock"
  fi
fi
[[ -S "$SOCKET" ]] || {
  printf 'capture: no Herdr socket at %s — is the server running?\n' "$SOCKET" >&2
  exit 1
}
printf 'socket: %s\noutput: %s\n\n' "$SOCKET" "$OUT"

# One request, one response, pretty-printed. Written in python rather than with a
# socket CLI so this has no dependency beyond python3.
call() {
  local name="$1" method="$2" params="${3:-{\}}"
  python3 - "$SOCKET" "$method" "$params" > "$OUT/$name" <<'PY'
import json, socket, sys

sock_path, method, params = sys.argv[1], sys.argv[2], sys.argv[3]
request = {"id": "capture", "method": method, "params": json.loads(params)}
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.settimeout(10)
    s.connect(sock_path)
    s.sendall((json.dumps(request) + "\n").encode())
    buffered = b""
    while b"\n" not in buffered:
        chunk = s.recv(65536)
        if not chunk:
            break
        buffered += chunk
line = buffered.split(b"\n", 1)[0]
print(json.dumps(json.loads(line), indent=2, sort_keys=True))
PY
  printf '  %-24s %s\n' "$name" "$(wc -c < "$OUT/$name") bytes"
}

# Everything the daemon reads on a normal tick.
printf 'requests:\n'
call session-snapshot.json session.snapshot
call agent-list.json agent.list
call pane-list.json pane.list
call ping.json ping

# `agent.explain` is the one that says *why* a pane is blocked or idle, which is
# the most useful thing in the whole capture when a status looks wrong.
FOCUSED="$(python3 -c '
import json,sys
snap=json.load(open(sys.argv[1]))
result=snap.get("result",{})
print(result.get("focused_pane_id") or "")
' "$OUT/session-snapshot.json" 2>/dev/null || true)"
if [[ -n "$FOCUSED" ]]; then
  call agent-explain.json agent.explain "{\"target\":\"$FOCUSED\"}"
else
  printf '  %-24s skipped (no focused pane id in the snapshot)\n' agent-explain.json
fi

# The full protocol schema, which is the authority the parsers should eventually
# be generated from rather than probed against.
"$HERDR_BIN" api schema --json > "$OUT/api-schema.json" 2>/dev/null \
  && printf '  %-24s %s bytes\n' api-schema.json "$(wc -c < "$OUT/api-schema.json")" \
  || printf '  %-24s unavailable\n' api-schema.json

{
  "$HERDR_BIN" --version || true
  "$HERDR_BIN" status || true
} > "$OUT/versions.txt" 2>&1

# The subscription the daemon opens. Ten seconds is long enough to catch a status
# transition if you drive an agent in another pane while this runs.
printf '\nsubscribing for 10s — drive an agent now to catch transitions\n'
python3 - "$SOCKET" > "$OUT/events.ndjson" <<'PY'
import json, socket, sys, time

request = {
    "id": "capture-sub",
    "method": "events.subscribe",
    "params": {"subscriptions": [
        {"type": "pane.agent_status_changed"},
        {"type": "pane.updated"},
        {"type": "pane.created"},
        {"type": "pane.closed"},
    ]},
}
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(sys.argv[1])
    s.sendall((json.dumps(request) + "\n").encode())
    s.settimeout(1.0)
    deadline = time.time() + 10
    buffered = b""
    while time.time() < deadline:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            continue
        if not chunk:
            break
        buffered += chunk
        while b"\n" in buffered:
            line, buffered = buffered.split(b"\n", 1)
            if line.strip():
                print(line.decode("utf-8", "replace"), flush=True)
PY
printf '  %-24s %s line(s)\n' events.ndjson "$(wc -l < "$OUT/events.ndjson")"

# The teleport source. NOT redacted: this is raw terminal output from a real
# session, so treat the file like terminal history and do not commit it as-is.
if [[ -n "$FOCUSED" ]]; then
  printf '\nobserving %s for 3s\n' "$FOCUSED"
  timeout 3 "$HERDR_BIN" terminal session observe "$FOCUSED" > "$OUT/observe.ndjson" 2>/dev/null || true
  printf '  %-24s %s line(s)\n' observe.ndjson "$(wc -l < "$OUT/observe.ndjson")"
fi

cat <<EOF

done: $OUT

Next:
  * Compare the field names against crates/ansible-herd/src/herdr.rs — the
    fixtures in its test module are shaped from documentation and every one that
    differs is a bug waiting to happen.
  * observe.ndjson decides the frame field name teleport.rs probes for.
  * observe.ndjson and events.ndjson can contain anything that was on screen.
    Scrub before committing, or commit only the field shapes.
EOF
