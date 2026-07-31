#!/usr/bin/env bash
#
# Validate every assumption this repo makes about Herdr, and emit a telemetry
# bundle from a machine that actually has it installed.
#
#   scripts/probe-herdr.sh [outdir]
#   scripts/probe-herdr.sh --non-interactive [outdir]   # skip the human checks
#
# Why this exists: `crates/ansible-herd/src/herdr.rs`, `.../teleport.rs`, and
# `plugins/herd/src/*` were written against herdr.dev's documentation, not against
# a running server. That is a real departure from this repo's convention — the hook
# work only discovered that a denied tool is byte-for-byte indistinguishable from a
# slow one *by recording*, and the redaction rules only reached 12 of 12 because a
# recording showed vendor-prefix rules catching 4. So every parser probes field
# names with fallbacks and degrades rather than failing, and this script is how the
# guesswork gets replaced with observation.
#
# It produces, in outdir:
#   report.md          the human summary: what passed, what failed, what to change
#   assumptions.jsonl  one line per check, machine-readable
#   raw/               captured responses and streams, scrubbed
#   env.txt            versions, so a later diff has provenance
#
# Everything under raw/ is piped through the plugin's own redactor when it is
# built (`npm run --workspace @ansible/herd build`). Build it first if you can —
# it makes the bundle safe to hand over, and a secret appearing in a capture would
# itself be the most valuable finding here. Without it the script still runs and
# says loudly that the captures are unscrubbed.
#
# Safe to run against a working session: every write it makes is a display-only
# metadata token on a scratch pane, one toast, and one Agents-view sort, and it
# undoes all three. It never sends input to an agent without asking.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

INTERACTIVE=1
OUT=""
for arg in "$@"; do
  case "$arg" in
    --non-interactive | --yes) INTERACTIVE=0 ;;
    -h | --help)
      sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) OUT="$arg" ;;
  esac
done
[[ -t 0 ]] || INTERACTIVE=0

OUT="${OUT:-herdr-telemetry-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT/raw"
OUT="$(cd "$OUT" && pwd)"
JSONL="$OUT/assumptions.jsonl"
: > "$JSONL"

HERDR="${HERDR_BIN_PATH:-herdr}"
SCRUB="$REPO_ROOT/plugins/herd/dist/main.js"

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
note() { printf '     %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Recording
# ---------------------------------------------------------------------------

# check <id> <area> <status> <assumption> <observed> [evidence] [code_ref]
#
# status is pass | fail | unknown | info. `unknown` is a first-class outcome and
# not a failure: it means the probe could not reach the thing, which is itself
# worth reporting.
check() {
  local id="$1" area="$2" status="$3" assumption="$4" observed="$5"
  local evidence="${6:-}" code_ref="${7:-}"
  python3 - "$JSONL" "$id" "$area" "$status" "$assumption" "$observed" "$evidence" "$code_ref" <<'PY'
import json, sys
path, id_, area, status, assumption, observed, evidence, code_ref = sys.argv[1:9]
with open(path, "a", encoding="utf-8") as f:
    f.write(json.dumps({
        "id": id_, "area": area, "status": status, "assumption": assumption,
        "observed": observed, "evidence": evidence, "code_ref": code_ref,
    }) + "\n")
PY
  local mark
  case "$status" in
    pass) mark=$'\033[32mPASS\033[0m' ;;
    fail) mark=$'\033[31mFAIL\033[0m' ;;
    unknown) mark=$'\033[33m????\033[0m' ;;
    *) mark="info" ;;
  esac
  printf '  %-5s %-5s %s\n' "$id" "$mark" "$assumption"
  [[ -n "$observed" ]] && note "$observed"
  return 0
}

# Ask the human something a socket cannot answer. Records yes/no/skipped.
ask() {
  local id="$1" area="$2" assumption="$3" prompt="$4" code_ref="${5:-}"
  if [[ $INTERACTIVE -eq 0 ]]; then
    check "$id" "$area" unknown "$assumption" "skipped (non-interactive)" "" "$code_ref"
    return
  fi
  printf '\n  \033[1m%s\033[0m %s\n' "$id" "$prompt"
  printf '     [y]es / [n]o / [s]kip > '
  local answer
  read -r answer </dev/tty
  case "$answer" in
    y | Y) check "$id" "$area" pass "$assumption" "confirmed by hand" "" "$code_ref" ;;
    n | N)
      printf '     what happened instead? > '
      local detail
      read -r detail </dev/tty
      check "$id" "$area" fail "$assumption" "${detail:-reported not working}" "" "$code_ref"
      ;;
    *) check "$id" "$area" unknown "$assumption" "skipped by operator" "" "$code_ref" ;;
  esac
}

# Scrub a file in place when the redactor is available.
scrub() {
  local file="$1"
  if [[ -f "$SCRUB" ]] && command -v node >/dev/null 2>&1; then
    node "$SCRUB" redact < "$file" > "$file.scrubbed" 2>>"$OUT/raw/redactor.log" &&
      mv "$file.scrubbed" "$file"
  fi
}

# ---------------------------------------------------------------------------
# One socket round trip
# ---------------------------------------------------------------------------

SOCKET=""

# call <name> <method> [params-json]  → writes raw/<name>.json, echoes it
call() {
  # Two lines rather than `${3:-{\}}`, because that expansion is not portable: bash
  # 5 drops the backslash and yields `{}`, and macOS's /bin/bash 3.2 keeps it and
  # yields `{\}`. The first telemetry bundle came off a bash 5 box and passed; the
  # first macOS run failed five checks — ping, session.snapshot, pane.list,
  # agent.list, unknown-method, every call that relied on the default — with an
  # empty response file and a json.loads traceback in the .err beside it. A probe
  # that reports the host as broken when it is the probe that is broken is worse
  # than no probe.
  local name="$1" method="$2" params="${3-}"
  [[ -n "$params" ]] || params='{}'
  python3 - "$SOCKET" "$method" "$params" > "$OUT/raw/$name.json" 2>"$OUT/raw/$name.err" <<'PY'
import json, socket, sys

sock_path, method, params = sys.argv[1], sys.argv[2], sys.argv[3]
request = {"id": "probe", "method": method, "params": json.loads(params)}
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(10)
        s.connect(sock_path)
        s.sendall((json.dumps(request) + "\n").encode())
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    line = buf.split(b"\n", 1)[0]
    print(json.dumps(json.loads(line), indent=2, sort_keys=True))
except Exception as exc:  # noqa: BLE001 - the probe reports failures, it does not raise
    print(json.dumps({"_probe_error": f"{type(exc).__name__}: {exc}"}, indent=2))
PY
  cat "$OUT/raw/$name.json"
}

# Does a JSON file contain a value at a jq-ish dotted path?
has() {
  python3 - "$1" "$2" <<'PY'
import json, sys
try:
    node = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
for part in sys.argv[2].split("."):
    if isinstance(node, list):
        node = node[0] if node else None
    if not isinstance(node, dict) or part not in node:
        sys.exit(1)
    node = node[part]
sys.exit(0 if node not in (None, "", [], {}) else 1)
PY
}

get() {
  python3 - "$1" "$2" <<'PY'
import json, sys
try:
    node = json.load(open(sys.argv[1]))
except Exception:
    print(""); sys.exit(0)
for part in sys.argv[2].split("."):
    if isinstance(node, list):
        node = node[0] if node else None
    if not isinstance(node, dict) or part not in node:
        print(""); sys.exit(0)
    node = node[part]
print("" if node is None else (node if isinstance(node, str) else json.dumps(node)))
PY
}

# ---------------------------------------------------------------------------
# 0 — preflight
# ---------------------------------------------------------------------------

bold "0. preflight"

if ! command -v "$HERDR" >/dev/null 2>&1; then
  printf '\033[31mprobe-herdr: %s is not on PATH.\033[0m\n' "$HERDR" >&2
  printf 'Install Herdr, or set HERDR_BIN_PATH, then run this again.\n' >&2
  exit 1
fi
command -v python3 >/dev/null || { printf 'probe-herdr: needs python3\n' >&2; exit 1; }

{
  printf 'probe run: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'uname: %s\n' "$(uname -a)"
  printf 'herdr --version: %s\n' "$("$HERDR" --version 2>&1 | head -1)"
  printf 'node: %s\n' "$(node --version 2>&1 || echo absent)"
  printf '\n--- herdr status ---\n'
  "$HERDR" status 2>&1 || true
  printf '\n--- herdr plugin list ---\n'
  "$HERDR" plugin list 2>&1 || true
  printf '\n--- herdr integration status ---\n'
  "$HERDR" integration status 2>&1 || true
} > "$OUT/env.txt" 2>&1

HERDR_VERSION="$("$HERDR" --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
note "herdr version: ${HERDR_VERSION:-unknown}"

if [[ -f "$SCRUB" ]]; then
  note "captures will be scrubbed through the plugin redactor"
else
  printf '\033[33m     WARNING: %s is missing, so raw/ will NOT be scrubbed.\033[0m\n' "$SCRUB"
  printf '\033[33m     Build it first:  npm install && npm run --workspace @ansible/herd build\033[0m\n'
  check WARN meta info "captures are scrubbed before being written" \
    "redactor not built; raw/ contains unscrubbed terminal output — review before sharing"
fi

# --- A. socket path resolution -------------------------------------------------

CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
if [[ -n "${HERDR_SOCKET_PATH:-}" ]]; then
  SOCKET="$HERDR_SOCKET_PATH"
  SOCKET_FROM="HERDR_SOCKET_PATH"
elif [[ -n "${HERDR_SESSION:-}" ]]; then
  SOCKET="$CONFIG_HOME/herdr/sessions/$HERDR_SESSION/herdr.sock"
  SOCKET_FROM="HERDR_SESSION"
else
  SOCKET="$CONFIG_HOME/herdr/herdr.sock"
  SOCKET_FROM="default"
fi

if [[ -S "$SOCKET" ]]; then
  check A1 socket pass "the socket is at the documented path (\$HERDR_SOCKET_PATH, else \$XDG_CONFIG_HOME/herdr[/sessions/<name>]/herdr.sock)" \
    "found via $SOCKET_FROM at $SOCKET" "env.txt" "crates/ansible-herd/src/herdr.rs:resolve_socket_path"
else
  FOUND="$(find "$CONFIG_HOME/herdr" -name '*.sock' 2>/dev/null | head -3 | tr '\n' ' ')"
  check A1 socket fail "the socket is at the documented path" \
    "nothing at $SOCKET; sockets actually present: ${FOUND:-none}" "env.txt" \
    "crates/ansible-herd/src/herdr.rs:resolve_socket_path"
  printf '\033[31mprobe-herdr: no socket to talk to. Start Herdr and re-run.\033[0m\n' >&2
  [[ -n "$FOUND" ]] && printf 'Try: HERDR_SOCKET_PATH=%s %s\n' "${FOUND%% *}" "$0" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 1 — protocol basics
# ---------------------------------------------------------------------------

bold "1. protocol"

call ping ping >/dev/null
if [[ "$(get "$OUT/raw/ping.json" id)" == "probe" ]]; then
  check A2 socket pass "a response echoes the request id, and the payload is under \`result\`" \
    "result.type=$(get "$OUT/raw/ping.json" result.type)" "raw/ping.json" \
    "crates/ansible-herd/src/herdr.rs:call"
else
  check A2 socket fail "a response echoes the request id, and the payload is under \`result\`" \
    "got $(head -c 200 "$OUT/raw/ping.json" | tr '\n' ' ')" "raw/ping.json" \
    "crates/ansible-herd/src/herdr.rs:call"
fi

# Observed: `version` is a string, `protocol` is a number, and there is a
# `capabilities` object. `live_handoff` in particular is worth reading — it means a
# server can be replaced under a running daemon.
PROTO="$(get "$OUT/raw/ping.json" result.protocol)"
VER="$(get "$OUT/raw/ping.json" result.version)"
CAPS="$(get "$OUT/raw/ping.json" result.capabilities)"
if [[ -n "$PROTO" || -n "$VER" ]]; then
  check A3 socket pass "\`ping\` reports \`version\` and a numeric \`protocol\`" \
    "version=$VER protocol=$PROTO capabilities=$CAPS" "raw/ping.json" \
    "crates/ansible-herd/src/herdr.rs:ping"
else
  check A3 socket fail "\`ping\` reports \`version\` and a numeric \`protocol\`" \
    "neither present; whole result: $(get "$OUT/raw/ping.json" result)" "raw/ping.json" \
    "crates/ansible-herd/src/herdr.rs:ping"
fi

call unknown-method herd.no_such_method >/dev/null
if has "$OUT/raw/unknown-method.json" error.code; then
  check A4 socket pass "an error comes back as \`error.code\` plus \`error.message\`" \
    "code=$(get "$OUT/raw/unknown-method.json" error.code)" "raw/unknown-method.json" \
    "crates/ansible-herd/src/herdr.rs:call"
else
  check A4 socket fail "an error comes back as \`error.code\` plus \`error.message\`" \
    "$(head -c 200 "$OUT/raw/unknown-method.json" | tr '\n' ' ')" "raw/unknown-method.json" \
    "crates/ansible-herd/src/herdr.rs:call"
fi

# ---------------------------------------------------------------------------
# 2 — the snapshot the daemon reconciles from
# ---------------------------------------------------------------------------

bold "2. session.snapshot and agent records"

call session-snapshot session.snapshot >/dev/null
call agent-list agent.list >/dev/null
call pane-list pane.list >/dev/null
scrub "$OUT/raw/session-snapshot.json"
scrub "$OUT/raw/pane-list.json"

if has "$OUT/raw/session-snapshot.json" result; then
  check B1 snapshot pass "\`session.snapshot\` exists and takes empty params" \
    "result.type=$(get "$OUT/raw/session-snapshot.json" result.type)" "raw/session-snapshot.json" \
    "crates/ansible-herd/src/herdr.rs:read_agents"
else
  check B1 snapshot fail "\`session.snapshot\` exists and takes empty params" \
    "$(get "$OUT/raw/session-snapshot.json" error.message)" "raw/session-snapshot.json" \
    "crates/ansible-herd/src/herdr.rs:read_agents"
fi

# Every field name the parsers probe for, audited in one pass. This table is the
# single highest-value output of the whole script: each miss is a silently blank
# column in the roster.
python3 - "$OUT/raw/session-snapshot.json" "$OUT/raw/agent-list.json" > "$OUT/raw/field-audit.txt" <<'PY'
import json, sys

def load(path):
    """The payload, unwrapped.

    Observed on 0.7.5: `session.snapshot` nests everything under
    `result.snapshot`, while `agent.list` and `pane.list` put their array
    directly on `result`. Handling both means this audit keeps working whichever
    shape a future release picks.
    """
    try:
        result = json.load(open(path)).get("result") or {}
    except Exception:
        return {}
    inner = result.get("snapshot")
    return inner if isinstance(inner, dict) else result

snap, agents_only = load(sys.argv[1]), load(sys.argv[2])

# (probe order, as written in herdr.rs / model.ts)
COLLECTIONS = {
    "workspaces": ["workspaces", "workspace_records"],
    "tabs": ["tabs", "tab_records"],
    "panes": ["panes", "pane_records"],
    "agents": ["agents", "agent_records"],
}
# Each entry is (candidate names in probe order, whether the first name is an
# optional override). Where it is, falling through is normal and not a finding:
# `display_agent` only exists when a user renamed the agent, and `foreground_cwd`
# only when the platform exposes it. Where it is not, a fallback means the name the
# code reaches for first is the wrong one.
AGENT_FIELDS = [
    (["pane_id"], False),
    (["workspace_id"], False),
    (["tab_id"], False),
    (["display_agent", "agent", "kind", "agent_kind"], True),
    (["agent_status", "status", "state"], False),
    (["terminal_title_stripped", "terminal_title"], True),
    (["foreground_cwd", "cwd"], True),
]
WORKSPACE_FIELDS = [
    (["workspace_id", "id"], False),
    (["label", "name"], False),
    # Optional for the same reason as its two neighbours below: a workspace row
    # carries no cwd. `herdr.rs:workspace_labels` records that as an observation and
    # reads it with `if let Some`, and the agent record — which does carry both
    # `foreground_cwd` and `cwd` — is where the answer actually comes from, so the
    # workspace fallback never fires. Required here, this entry failed B2 forever
    # over a field nothing needs.
    (["cwd"], True),
    (["worktree"], True),
    (["branch"], True),
]
TAB_FIELDS = [(["tab_id", "id"], False), (["label", "name"], False), (["workspace_id"], False)]

def report(title, obj, keysets):
    print(f"\n[{title}]")
    if not obj:
        print("  (no rows to inspect)")
        return
    keys = sorted(obj.keys())
    print(f"  keys present: {', '.join(keys)}")
    for group, first_optional in keysets:
        hit = next((k for k in group if k in obj and obj[k] not in (None, "")), None)
        if hit is None:
            status = "ABSENT (optional)" if first_optional else "MISSING"
        elif hit == group[0] or first_optional:
            status = f"OK via '{hit}'"
        else:
            status = f"OK via '{hit}'  <-- WRONG-FIRST-GUESS: code tries '{group[0]}' first"
        print(f"  {'/'.join(group):45s} {status}")

print("=== collections on session.snapshot ===")
for name, group in COLLECTIONS.items():
    hit = next((k for k in group if isinstance(snap.get(k), list)), None)
    count = len(snap.get(hit, [])) if hit else 0
    print(f"  {'/'.join(group):45s} {'OK via ' + hit + f' ({count} rows)' if hit else 'MISSING'}")

print(f"\n  focused_pane_id: {snap.get('focused_pane_id', 'MISSING')}")

def first_row(obj, group):
    for k in group:
        rows = obj.get(k)
        if isinstance(rows, list) and rows:
            return rows[0]
    return {}

report("agent record (from session.snapshot)", first_row(snap, COLLECTIONS["agents"]), AGENT_FIELDS)
report("agent record (from agent.list)", first_row(agents_only, COLLECTIONS["agents"]), AGENT_FIELDS)
report("workspace record", first_row(snap, COLLECTIONS["workspaces"]), WORKSPACE_FIELDS)
report("tab record", first_row(snap, COLLECTIONS["tabs"]), TAB_FIELDS)

statuses = set()
for src in (snap, agents_only):
    for k in COLLECTIONS["agents"]:
        for row in src.get(k, []) or []:
            for f in ("agent_status", "status", "state"):
                if isinstance(row, dict) and row.get(f):
                    statuses.add(f"{f}={row[f]}")
print(f"\n=== agent_status values observed ===\n  {', '.join(sorted(statuses)) or '(none)'}")
PY
cat "$OUT/raw/field-audit.txt"

MISSING="$(grep -c 'MISSING' "$OUT/raw/field-audit.txt" || true)"
PREFERS="$(grep -c 'WRONG-FIRST-GUESS' "$OUT/raw/field-audit.txt" || true)"
if [[ "$MISSING" -eq 0 && "$PREFERS" -eq 0 ]]; then
  check B2 snapshot pass "every field name the parsers probe for is present under its first-choice name" \
    "no misses" "raw/field-audit.txt" "crates/ansible-herd/src/herdr.rs:parse_agent"
else
  check B2 snapshot fail "every field name the parsers probe for is present under its first-choice name" \
    "$MISSING required field(s) missing, $PREFERS reached only via a fallback — see the audit" "raw/field-audit.txt" \
    "crates/ansible-herd/src/herdr.rs:parse_agent"
fi

OBSERVED_STATUSES="$(sed -n '/agent_status values observed/,$p' "$OUT/raw/field-audit.txt" | tail -1 | tr -d ' ')"
if [[ -n "$OBSERVED_STATUSES" && "$OBSERVED_STATUSES" != "(none)" ]]; then
  UNEXPECTED="$(printf '%s' "$OBSERVED_STATUSES" | tr ',' '\n' | sed 's/.*=//' |
    grep -vE '^(idle|working|blocked|done|unknown)$' | tr '\n' ' ')"
  if [[ -z "$UNEXPECTED" ]]; then
    check C1 status pass "\`agent_status\` is one of idle|working|blocked|done|unknown" \
      "$OBSERVED_STATUSES" "raw/field-audit.txt" "plugins/herd/src/model.ts:statusFromHerdr"
  else
    check C1 status fail "\`agent_status\` is one of idle|working|blocked|done|unknown" \
      "also saw: $UNEXPECTED" "raw/field-audit.txt" "plugins/herd/src/model.ts:statusFromHerdr"
  fi
else
  check C1 status unknown "\`agent_status\` is one of idle|working|blocked|done|unknown" \
    "no agent panes were running; start one and re-run" "raw/field-audit.txt" \
    "plugins/herd/src/model.ts:statusFromHerdr"
fi

FOCUSED="$(get "$OUT/raw/session-snapshot.json" result.snapshot.focused_pane_id)"
[[ -n "$FOCUSED" ]] || FOCUSED="$(get "$OUT/raw/session-snapshot.json" result.focused_pane_id)"
# Look in both envelopes, and fall back to agent.list, so one shape change cannot
# empty every later check the way it did on the first run.
AGENT_PANE="$(python3 - "$OUT/raw/session-snapshot.json" "$OUT/raw/agent-list.json" <<'PY'
import json, sys

def payloads(path):
    try:
        result = json.load(open(path)).get("result") or {}
    except Exception:
        return []
    out = [result]
    if isinstance(result.get("snapshot"), dict):
        out.append(result["snapshot"])
    return out

for path in sys.argv[1:]:
    for body in payloads(path):
        for key in ("agents", "agent_records", "panes", "pane_records"):
            for row in body.get(key, []) or []:
                if isinstance(row, dict) and row.get("pane_id") and row.get("agent"):
                    print(row["pane_id"]); raise SystemExit
print("")
PY
)"
note "focused pane: ${FOCUSED:-none}; first agent pane: ${AGENT_PANE:-none}"

if [[ -n "${FOCUSED:-}" ]]; then
  call agent-explain agent.explain "{\"target\":\"$FOCUSED\"}"  >/dev/null
  scrub "$OUT/raw/agent-explain.json"
  if has "$OUT/raw/agent-explain.json" result; then
    check B3 snapshot pass "\`agent.explain\` reports the matched rule, so a wrong status is diagnosable" \
      "captured" "raw/agent-explain.json" "docs/plan/herdr-plugin.md"
  else
    check B3 snapshot fail "\`agent.explain\` reports the matched rule" \
      "$(get "$OUT/raw/agent-explain.json" error.message)" "raw/agent-explain.json" ""
  fi
fi

# ---------------------------------------------------------------------------
# 3 — events: the one that decides polling vs. push
# ---------------------------------------------------------------------------

bold "3. events.subscribe"

# The single most load-bearing unverified guess: whether a subscription may omit
# `pane_id`. If it may not, the daemon polls — designed for, but a full second of
# extra latency on the status that matters most.
python3 - "$SOCKET" > "$OUT/raw/events-wildcard.ndjson" 2>&1 <<'PY'
import json, socket, sys, time

req = {"id": "probe-sub", "method": "events.subscribe", "params": {"subscriptions": [
    {"type": "pane.agent_status_changed"},
    {"type": "pane.updated"},
    {"type": "pane.created"},
    {"type": "pane.closed"},
]}}
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(sys.argv[1])
        s.sendall((json.dumps(req) + "\n").encode())
        s.settimeout(1.0)
        deadline = time.time() + 12
        buf = b""
        while time.time() < deadline:
            try:
                chunk = s.recv(65536)
            except socket.timeout:
                continue
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if line.strip():
                    print(line.decode("utf-8", "replace"), flush=True)
except Exception as exc:  # noqa: BLE001
    print(json.dumps({"_probe_error": f"{type(exc).__name__}: {exc}"}))
PY

if [[ $INTERACTIVE -eq 1 ]]; then
  note "subscribed for 12s — drive an agent in another pane now to catch transitions"
fi
ACK="$(head -1 "$OUT/raw/events-wildcard.ndjson" 2>/dev/null || true)"
if printf '%s' "$ACK" | grep -q '"error"'; then
  check D1 events fail "\`events.subscribe\` accepts a subscription with no \`pane_id\` filter" \
    "rejected: $(printf '%s' "$ACK" | head -c 200)" "raw/events-wildcard.ndjson" \
    "crates/ansible-herd/src/herdr.rs:Events::spawn"
  # If the wildcard is refused, does a scoped one work? That is the fallback shape.
  if [[ -n "${AGENT_PANE:-}" ]]; then
    python3 - "$SOCKET" "$AGENT_PANE" > "$OUT/raw/events-scoped.ndjson" 2>&1 <<'PY'
import json, socket, sys, time
req = {"id": "probe-sub2", "method": "events.subscribe", "params": {"subscriptions": [
    {"type": "pane.agent_status_changed", "pane_id": sys.argv[2]},
]}}
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(sys.argv[1]); s.sendall((json.dumps(req) + "\n").encode()); s.settimeout(3)
    try:
        print(s.recv(65536).decode("utf-8", "replace").strip())
    except socket.timeout:
        print("(no ack within 3s)")
PY
    if grep -q '"error"' "$OUT/raw/events-scoped.ndjson"; then
      check D2 events fail "a per-pane subscription is accepted" "also rejected" "raw/events-scoped.ndjson" ""
    else
      check D2 events pass "a per-pane subscription is accepted, so the daemon can subscribe per pane instead" \
        "$(head -c 160 "$OUT/raw/events-scoped.ndjson")" "raw/events-scoped.ndjson" \
        "crates/ansible-herd/src/herdr.rs:Events::spawn"
    fi
  fi
elif [[ -n "$ACK" ]]; then
  check D1 events pass "\`events.subscribe\` accepts a subscription with no \`pane_id\` filter" \
    "ack: $(printf '%s' "$ACK" | head -c 160)" "raw/events-wildcard.ndjson" \
    "crates/ansible-herd/src/herdr.rs:Events::spawn"
else
  check D1 events unknown "\`events.subscribe\` accepts a subscription with no \`pane_id\` filter" \
    "no ack at all within 12s" "raw/events-wildcard.ndjson" "crates/ansible-herd/src/herdr.rs:Events::spawn"
fi

# Each type on its own, because the aggregate error names only the first offender.
# If `pane.created` requires a pane_id then new panes can never be discovered by
# subscription, and a poll is not a fallback but the only option.
: > "$OUT/raw/event-type-audit.txt"
for etype in pane.agent_status_changed pane.updated pane.created pane.closed pane.focused pane.exited workspace.created tab.created; do
  RESULT="$(python3 - "$SOCKET" "$etype" <<'ONE'
import json, socket, sys
req = {"id": "probe-one", "method": "events.subscribe",
       "params": {"subscriptions": [{"type": sys.argv[2]}]}}
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(3)
        s.connect(sys.argv[1])
        s.sendall((json.dumps(req) + "\n").encode())
        print(s.recv(65536).decode("utf-8", "replace").strip().replace("\n", " ")[:200])
except Exception as exc:  # noqa: BLE001
    print(f"probe error: {exc}")
ONE
)"
  printf '%-32s %s\n' "$etype" "$RESULT" >> "$OUT/raw/event-type-audit.txt"
done
cat "$OUT/raw/event-type-audit.txt"
NEEDS_PANE="$(grep -c 'missing field .pane_id' "$OUT/raw/event-type-audit.txt" || true)"
TOTAL_TYPES="$(wc -l < "$OUT/raw/event-type-audit.txt" | tr -d ' ')"
check D4 events info "which event types require a \`pane_id\`" \
  "$NEEDS_PANE of $TOTAL_TYPES types rejected an unfiltered subscription — see the audit" \
  "raw/event-type-audit.txt" "crates/ansible-herd/src/herdr.rs:Events::spawn"

EVENT_NAMES="$(python3 - "$OUT/raw/events-wildcard.ndjson" <<'PY'
import json, sys
names = set()
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    try:
        obj = json.loads(line)
    except Exception:
        continue
    for key in ("type", "event"):
        if isinstance(obj.get(key), str):
            names.add(obj[key])
    inner = obj.get("event") if isinstance(obj.get("event"), dict) else None
    if inner and isinstance(inner.get("type"), str):
        names.add(inner["type"])
print(" ".join(sorted(names)))
PY
)"
if [[ -n "$EVENT_NAMES" ]]; then
  check D3 events info "pushed events name themselves in a \`type\` field" "$EVENT_NAMES" \
    "raw/events-wildcard.ndjson" "crates/ansible-herd/src/herdr.rs:Events::spawn"
else
  check D3 events unknown "pushed events name themselves in a \`type\` field" \
    "no events arrived — nothing changed during the window" "raw/events-wildcard.ndjson" ""
fi

# ---------------------------------------------------------------------------
# 4 — metadata tokens: how presence gets into Herdr's own UI
# ---------------------------------------------------------------------------

bold "4. pane.report_metadata"

TOKEN_PANE="${AGENT_PANE:-$FOCUSED}"
if [[ -z "$TOKEN_PANE" ]]; then
  check E1 tokens unknown "\`pane.report_metadata\` accepts a token patch with source, ttl_ms and seq" \
    "no pane to write to" "" "crates/ansible-herd/src/herdr.rs:report_tokens"
else
  # Epoch milliseconds, not 1. `seq` is a strict monotonic guard per (pane, source)
  # and a stale patch is dropped with an `ok` — so a hardcoded 1 makes E2 pass on
  # the first ever run against a pane and fail on every run after it, which is
  # exactly the coin flip observed. This is also the bug the guard found in
  # `daemon.rs`, where the counter restarted at zero on every daemon start.
  TOKEN_SEQ="$(python3 -c 'import time; print(int(time.time()*1000))')"
  call report-metadata pane.report_metadata "$(printf '{"pane_id":"%s","source":"probe:herd","tokens":{"herd":"2 watching","probe":"hello"},"ttl_ms":120000,"seq":%s}' "$TOKEN_PANE" "$TOKEN_SEQ")" >/dev/null
  if has "$OUT/raw/report-metadata.json" result; then
    check E1 tokens pass "\`pane.report_metadata\` accepts a token patch with source, ttl_ms and seq" \
      "result.type=$(get "$OUT/raw/report-metadata.json" result.type)" "raw/report-metadata.json" \
      "crates/ansible-herd/src/herdr.rs:report_tokens"
  else
    check E1 tokens fail "\`pane.report_metadata\` accepts a token patch with source, ttl_ms and seq" \
      "$(get "$OUT/raw/report-metadata.json" error.message)" "raw/report-metadata.json" \
      "crates/ansible-herd/src/herdr.rs:report_tokens"
  fi

  call pane-get-after-tokens pane.get "{\"pane_id\":\"$TOKEN_PANE\"}" >/dev/null
  scrub "$OUT/raw/pane-get-after-tokens.json"
  if grep -q '"herd"' "$OUT/raw/pane-get-after-tokens.json"; then
    check E2 tokens pass "tokens are readable back on the pane record, so the daemon can diff them" \
      "found the token in pane.get" "raw/pane-get-after-tokens.json" \
      "crates/ansible-herd/src/daemon.rs:report_tokens"
  else
    check E2 tokens fail "tokens are readable back on the pane record" \
      "no 'herd' key in pane.get — check where tokens actually live in the response" \
      "raw/pane-get-after-tokens.json" "crates/ansible-herd/src/daemon.rs:report_tokens"
  fi

  # The assumption a socket cannot check: whether a token *displays*. The docs say
  # tokens "can be rendered as $name in Agent sidebar rows", which reads like the
  # row template has to opt in.
  ask E3 tokens "a reported token shows up in the Agents sidebar without extra config" \
    "Look at the Agents sidebar for pane $TOKEN_PANE. Do you see \"2 watching\" (the \$herd token) on its row?
     If not, try adding \$herd to the sidebar row format in your Herdr config and answer n with what you had to change." \
    "docs/plan/herdr-plugin.md#2-status-that-is-mostly-free"

  # Clear up after ourselves: null clears a key. Past the write's seq, or the clear
  # is the patch that gets silently dropped and the probe leaves its tokens behind
  # on someone's sidebar row.
  call report-metadata-clear pane.report_metadata "$(printf '{"pane_id":"%s","source":"probe:herd","tokens":{"herd":null,"probe":null},"seq":%s}' "$TOKEN_PANE" "$((TOKEN_SEQ + 1))")" >/dev/null
  if has "$OUT/raw/report-metadata-clear.json" result; then
    check E4 tokens pass "a null token value clears the key (how a watcher who left is removed)" \
      "cleared" "raw/report-metadata-clear.json" "crates/ansible-herd/src/daemon.rs:report_tokens"
  else
    check E4 tokens fail "a null token value clears the key" \
      "$(get "$OUT/raw/report-metadata-clear.json" error.message)" "raw/report-metadata-clear.json" \
      "crates/ansible-herd/src/daemon.rs:report_tokens"
  fi
fi

# ---------------------------------------------------------------------------
# 5 — notifications
# ---------------------------------------------------------------------------

bold "5. notification.show"

call notify notification.show '{"title":"herd probe","body":"validating assumptions","sound":"none"}' >/dev/null
NOTIFY_REASON="$(get "$OUT/raw/notify.json" result.reason)"
if [[ -n "$NOTIFY_REASON" ]]; then
  check F1 notify pass "\`notification.show\` reports whether it was shown, via \`result.reason\`" \
    "reason=$NOTIFY_REASON shown=$(get "$OUT/raw/notify.json" result.shown)" "raw/notify.json" \
    "crates/ansible-herd/src/herdr.rs:notify"
else
  check F1 notify fail "\`notification.show\` reports whether it was shown, via \`result.reason\`" \
    "$(head -c 200 "$OUT/raw/notify.json" | tr '\n' ' ')" "raw/notify.json" \
    "crates/ansible-herd/src/herdr.rs:notify"
fi

# The daemon only notifies on a rising edge, but knowing the host's own rate limit
# tells us whether that is belt-and-braces or the only thing standing between a
# teammate and a toast storm.
for i in 1 2 3 4 5; do
  call "notify-burst-$i" notification.show "{\"title\":\"herd probe burst $i\",\"sound\":\"none\"}" >/dev/null
done
BURST="$(for i in 1 2 3 4 5; do get "$OUT/raw/notify-burst-$i.json" result.reason; done | tr '\n' ' ')"
check F2 notify info "how the host rate-limits five toasts in a row" "$BURST" "raw/notify-burst-1.json" \
  "crates/ansible-herd/src/daemon.rs:announce"

# ---------------------------------------------------------------------------
# 6 — the Agents view projection
# ---------------------------------------------------------------------------

bold "6. agent.view.set"

call view-set agent.view.set '{"source":"probe:herd","label":"probe","sort":[{"field":"attention","order":"desc"},{"field":"state_change_seq","order":"desc"}]}' >/dev/null
if has "$OUT/raw/view-set.json" result; then
  check G1 view pass "\`agent.view.set\` accepts an attention-first sort from a non-plugin source" \
    "active=$(get "$OUT/raw/view-set.json" result.active)" "raw/view-set.json" \
    "crates/ansible-herd/src/herdr.rs:set_attention_view"
else
  check G1 view fail "\`agent.view.set\` accepts an attention-first sort with fields attention/state_change_seq" \
    "$(get "$OUT/raw/view-set.json" error.message)" "raw/view-set.json" \
    "crates/ansible-herd/src/herdr.rs:set_attention_view"
fi
call view-clear agent.view.clear '{"source":"probe:herd"}' >/dev/null
if has "$OUT/raw/view-clear.json" result; then
  check G2 view pass "\`agent.view.clear\` with our own source restores the configured sort" "cleared" \
    "raw/view-clear.json" "crates/ansible-herd/src/herdr.rs:clear_attention_view"
else
  check G2 view fail "\`agent.view.clear\` with our own source restores the configured sort" \
    "$(get "$OUT/raw/view-clear.json" error.message)" "raw/view-clear.json" ""
fi

# ---------------------------------------------------------------------------
# 7 — teleport: the frame stream
# ---------------------------------------------------------------------------

bold "7. terminal session observe"

if [[ -z "${TOKEN_PANE:-}" ]]; then
  check I1 teleport unknown "\`terminal session observe\` streams base64 frames" "no pane to observe" "" \
    "crates/ansible-herd/src/teleport.rs:parse_frame"
else
  timeout 4 "$HERDR" terminal session observe "$TOKEN_PANE" > "$OUT/raw/observe.ndjson" 2>"$OUT/raw/observe.err"
  FRAMES="$(wc -l < "$OUT/raw/observe.ndjson" | tr -d ' ')"
  if [[ "$FRAMES" -gt 0 ]]; then
    check I1 teleport pass "\`terminal session observe\` streams newline-delimited records" \
      "$FRAMES record(s) in 4s" "raw/observe-shapes.ndjson" "crates/ansible-herd/src/teleport.rs:LivePublisher::start"
  else
    check I1 teleport fail "\`terminal session observe\` streams newline-delimited records" \
      "nothing captured: $(head -c 200 "$OUT/raw/observe.err" | tr '\n' ' ')" "raw/observe.err" \
      "crates/ansible-herd/src/teleport.rs:LivePublisher::start"
  fi

  # THE field-name question for teleport. The code probes five names; this says
  # which one is real, and whether the payload decodes as base64.
  python3 - "$OUT/raw/observe.ndjson" > "$OUT/raw/frame-audit.txt" <<'PY'
import base64, json, sys

CANDIDATES = ["bytes", "data", "data_base64", "base64", "b64"]
types, found, decoded, sizes = set(), set(), 0, []
keys = set()
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    try:
        obj = json.loads(line)
    except Exception:
        continue
    if not isinstance(obj, dict):
        continue
    keys |= set(obj.keys())
    if isinstance(obj.get("type"), str):
        types.add(obj["type"])
    for container in (obj, obj.get("frame") if isinstance(obj.get("frame"), dict) else {}):
        for field in CANDIDATES:
            val = container.get(field)
            if isinstance(val, str):
                found.add(field)
                try:
                    sizes.append(len(base64.b64decode(val, validate=True)))
                    decoded += 1
                except Exception:
                    pass
print(f"record types seen : {', '.join(sorted(types)) or '(none)'}")
print(f"top-level keys    : {', '.join(sorted(keys)) or '(none)'}")
print(f"payload field(s)  : {', '.join(sorted(found)) or 'NONE OF ' + ','.join(CANDIDATES)}")
print(f"base64-decodable  : {decoded} of {len(sizes) or 0} attempted")
if sizes:
    print(f"frame bytes       : min {min(sizes)} / max {max(sizes)} / total {sum(sizes)}")
PY
  cat "$OUT/raw/frame-audit.txt"

  # A text redactor cannot see inside base64, so leaving encoded payloads in the
  # bundle would ship raw terminal output straight past the scrubber. Split the
  # capture instead: the ndjson keeps the *shapes* — which is all the fixtures need
  # — with payloads elided, and the bytes go to a file that does get redacted.
  python3 - "$OUT/raw/observe.ndjson" "$OUT/raw/observe-shapes.ndjson" "$OUT/raw/observe-decoded.bin" <<'ELIDE'
import base64, json, sys

CANDIDATES = ["bytes", "data", "data_base64", "base64", "b64"]
src, shapes_path, decoded_path = sys.argv[1], sys.argv[2], sys.argv[3]
with open(shapes_path, "w", encoding="utf-8") as shapes, open(decoded_path, "wb") as decoded:
    for line in open(src, encoding="utf-8", errors="replace"):
        try:
            obj = json.loads(line)
        except Exception:
            continue
        containers = [obj]
        if isinstance(obj.get("frame"), dict):
            containers.append(obj["frame"])
        for container in containers:
            for field in CANDIDATES:
                val = container.get(field)
                if isinstance(val, str):
                    try:
                        raw = base64.b64decode(val, validate=True)
                    except Exception:
                        container[field] = "<undecodable payload elided>"
                        continue
                    decoded.write(raw)
                    container[field] = f"<{len(raw)} bytes elided - see observe-decoded.txt>"
        shapes.write(json.dumps(obj) + "\n")
ELIDE

  if [[ -f "$SCRUB" ]] && command -v node >/dev/null 2>&1; then
    node "$SCRUB" redact < "$OUT/raw/observe-decoded.bin" > "$OUT/raw/observe-decoded.txt" \
      2>>"$OUT/raw/redactor.log" && rm -f "$OUT/raw/observe-decoded.bin"
    check I5 teleport info "decoded frame bytes go through the redactor before being written" \
      "$(grep -c redacted "$OUT/raw/redactor.log" 2>/dev/null || echo 0) redaction event(s) logged over real session output" \
      "raw/observe-decoded.txt" "plugins/herd/src/redact.ts"
  else
    mv "$OUT/raw/observe-decoded.bin" "$OUT/raw/observe-decoded.txt" 2>/dev/null || true
    check I5 teleport fail "decoded frame bytes go through the redactor before being written" \
      "redactor not built, so raw/observe-decoded.txt is UNSCRUBBED terminal output - review or delete it before sharing" \
      "raw/observe-decoded.txt" "plugins/herd/src/redact.ts"
  fi
  # The still-encoded capture must not survive into the bundle.
  rm -f "$OUT/raw/observe.ndjson"

  FRAME_FIELD="$(grep 'payload field' "$OUT/raw/frame-audit.txt" | sed 's/.*: //')"
  if [[ "$FRAME_FIELD" == NONE* ]]; then
    check I2 teleport fail "a frame's terminal bytes are base64 under one of bytes|data|data_base64|base64|b64" \
      "none matched — teleport publishes nothing until this is fixed; see raw/frame-audit.txt for the real keys" \
      "raw/frame-audit.txt" "crates/ansible-herd/src/teleport.rs:parse_frame"
  else
    check I2 teleport pass "a frame's terminal bytes are base64 under a probed field name" \
      "field: $FRAME_FIELD" "raw/frame-audit.txt" "crates/ansible-herd/src/teleport.rs:parse_frame"
  fi

  if grep -q 'closed' "$OUT/raw/frame-audit.txt"; then
    check I3 teleport pass "the stream ends with a \`terminal.closed\` record" "seen" "raw/frame-audit.txt" \
      "crates/ansible-herd/src/teleport.rs:parse_frame"
  else
    check I3 teleport unknown "the stream ends with a \`terminal.closed\` record" \
      "not seen — the probe was killed by its own timeout, which is not the close path" \
      "raw/frame-audit.txt" "crates/ansible-herd/src/teleport.rs:parse_frame"
  fi

  ask I4 teleport "observing does not steal input, scroll, or focus from the pane's owner" \
    "While the 4s observe ran just now, did pane $TOKEN_PANE keep working normally — no focus jump, no input stolen?" \
    "crates/ansible-herd/src/teleport.rs"
fi

# ---------------------------------------------------------------------------
# 8 — writing to a pane: the consent boundary
# ---------------------------------------------------------------------------

bold "8. pane.send_text (the consent boundary)"

if [[ $INTERACTIVE -eq 0 || -z "${AGENT_PANE:-}" ]]; then
  check J1 input unknown "\`pane.send_text\` types without submitting" \
    "skipped (needs a human and an agent pane)" "" "crates/ansible-herd/src/herdr.rs:send_text"
else
  printf '\n  \033[1mJ1\033[0m About to type — not submit — a line into agent pane %s.\n' "$AGENT_PANE"
  printf '     Nothing is sent to the agent unless you press Enter yourself.\n'
  printf '     Proceed? [y/N] > '
  read -r go </dev/tty
  if [[ "$go" == y || "$go" == Y ]]; then
    call send-text pane.send_text "$(printf '{"pane_id":"%s","text":"[from @probe] this should sit unsent in the composer"}' "$AGENT_PANE")" >/dev/null
    ask J1 input "\`pane.send_text\` places text in the composer *without* submitting it" \
      "Look at pane $AGENT_PANE. Is the text sitting in the input, unsent (agent did NOT start working)?" \
      "crates/ansible-herd/src/main.rs:cmd_inbox"
    note "clear it yourself with ctrl-u in that pane"
  else
    check J1 input unknown "\`pane.send_text\` types without submitting" "declined by operator" "" \
      "crates/ansible-herd/src/herdr.rs:send_text"
  fi
fi

# ---------------------------------------------------------------------------
# 9 — the plugin host contract
# ---------------------------------------------------------------------------

bold "9. plugin host"

MIN_VER="$(grep -m1 '^min_herdr_version' plugins/herd/herdr-plugin.toml | sed 's/.*"\(.*\)".*/\1/')"
if [[ -n "$HERDR_VERSION" && -n "$MIN_VER" ]]; then
  LOWEST="$(printf '%s\n%s\n' "$HERDR_VERSION" "$MIN_VER" | sort -V | head -1)"
  if [[ "$LOWEST" == "$MIN_VER" ]]; then
    check K1 plugin pass "the manifest's \`min_herdr_version\` is not newer than the installed binary" \
      "manifest $MIN_VER <= installed $HERDR_VERSION" "env.txt" "plugins/herd/herdr-plugin.toml"
  else
    check K1 plugin fail "the manifest's \`min_herdr_version\` is not newer than the installed binary" \
      "manifest claims $MIN_VER but this is $HERDR_VERSION — Herdr will refuse to link it; lower the manifest" \
      "env.txt" "plugins/herd/herdr-plugin.toml"
  fi
fi

# Linking is the only way to learn the injected environment and the startup-hook
# lifetime, and both are guesses today.
if [[ $INTERACTIVE -eq 1 ]]; then
  printf '\n  \033[1mK2\033[0m May I `herdr plugin link plugins/herd`? It registers a plugin globally\n'
  printf '     for your user (undo with `herdr plugin unlink ansible.herd`). [y/N] > '
  read -r go </dev/tty
else
  go=n
fi

if [[ "$go" == y || "$go" == Y ]]; then
  "$HERDR" plugin link plugins/herd > "$OUT/raw/plugin-link.txt" 2>&1
  if grep -qiE 'error|refus|invalid' "$OUT/raw/plugin-link.txt"; then
    check K2 plugin fail "the manifest links cleanly (fields, placements, contexts all accepted)" \
      "$(head -c 300 "$OUT/raw/plugin-link.txt" | tr '\n' ' ')" "raw/plugin-link.txt" \
      "plugins/herd/herdr-plugin.toml"
  else
    check K2 plugin pass "the manifest links cleanly (fields, placements, contexts all accepted)" \
      "linked; warnings: $(grep -i warn "$OUT/raw/plugin-link.txt" | head -3 | tr '\n' ' ')" \
      "raw/plugin-link.txt" "plugins/herd/herdr-plugin.toml"

    "$HERDR" plugin list --json > "$OUT/raw/plugin-list.json" 2>&1 || true
    "$HERDR" plugin action list --plugin ansible.herd > "$OUT/raw/plugin-actions.txt" 2>&1 || true

    # The injected environment, dumped by the plugin itself. `doctor` prints the
    # variables it found, which is exactly the list to compare against paths.ts.
    "$HERDR" plugin action invoke ansible.herd.doctor > "$OUT/raw/action-invoke.txt" 2>&1 || true
    sleep 2
    "$HERDR" plugin log list --plugin ansible.herd --limit 20 > "$OUT/raw/plugin-log.txt" 2>&1 || true
    # The interesting artefact is the invocation *context* Herdr builds, because a
    # `contexts = ["workspace"]` action gets no pane of its own and has to read
    # `focused_pane_id` out of it. Grepping the log for HERDR_ was the wrong test:
    # the log holds the command's stdout, not its environment.
    CONTEXT_KEYS="$(get "$OUT/raw/action-invoke.json" result.context 2>/dev/null)"
    [[ -n "$CONTEXT_KEYS" ]] || CONTEXT_KEYS="$(python3 - "$OUT/raw/action-invoke.txt" <<'CTX'
import json, sys
try:
    ctx = json.load(open(sys.argv[1]))["result"]["context"]
    print(",".join(sorted(ctx.keys())))
except Exception:
    print("")
CTX
)"
    if printf '%s' "$CONTEXT_KEYS" | grep -q focused_pane_id; then
      check K3 plugin pass "the invocation context carries \`focused_pane_id\`, which a workspace-context action needs" \
        "$CONTEXT_KEYS" "raw/action-invoke.txt" "crates/ansible-herd/src/main.rs:current_pane"
    else
      check K3 plugin fail "the invocation context carries \`focused_pane_id\`" \
        "context keys: ${CONTEXT_KEYS:-none found} — \`--share\` cannot resolve a pane from a keybinding" \
        "raw/action-invoke.txt" "crates/ansible-herd/src/main.rs:current_pane"
    fi
    if grep -q 'herdr.sock' "$OUT/raw/plugin-log.txt" 2>/dev/null; then
      check K6 plugin pass "the plugin runs under Herdr and receives \`HERDR_SOCKET_PATH\`" \
        "the action's own output names the socket it was given" "raw/plugin-log.txt" \
        "plugins/herd/src/paths.ts"
    fi

    ask K4 plugin "an overlay pane entrypoint opens and restores focus when it closes" \
      "Run this and close the pane:  $HERDR plugin pane open --plugin ansible.herd --entrypoint roster
     Did the herd render, and did closing it put you back where you were?" \
      "plugins/herd/herdr-plugin.toml"

    # A plain spawn was measured *not* to survive, so `startup` now goes through
    # `setsid`. This asks whether that was enough.
    if command -v setsid >/dev/null 2>&1; then
      check K5a plugin pass "\`setsid\` is available, so the daemon can leave the hook's process group" \
        "$(command -v setsid)" "" "crates/ansible-herd/src/main.rs:cmd_startup"
    else
      check K5a plugin fail "\`setsid\` is available, so the daemon can leave the hook's process group" \
        "not on PATH — the daemon needs a pane or a user service on this machine instead" "" \
        "crates/ansible-herd/src/main.rs:cmd_startup"
    fi
    ask K5 plugin "a setsid-detached daemon survives its \`[[startup]]\` hook exiting" \
      "This decides whether the daemon design works at all. Link the Rust plugin, restart Herdr,
     and check:
       herdr plugin link plugins/herdr-presence && herdr kill && herdr
       pgrep -laf 'ansible-herd daemon' || echo 'no daemon'
     Does it survive now? (A plain spawn did not; startup now uses setsid.)" \
      "crates/ansible-herd/src/main.rs:cmd_startup"

    printf '     unlink again? [Y/n] > '
    read -r un </dev/tty
    [[ "$un" == n || "$un" == N ]] || "$HERDR" plugin unlink ansible.herd >/dev/null 2>&1
  fi
else
  for id in K2 K3 K4 K5; do
    check "$id" plugin unknown "plugin host contract check $id" "skipped (did not link)" "" \
      "plugins/herd/herdr-plugin.toml"
  done
fi

# ---------------------------------------------------------------------------
# 10 — the status that matters most, timed
# ---------------------------------------------------------------------------

bold "10. blocked, timed"

if [[ $INTERACTIVE -eq 0 || -z "${AGENT_PANE:-}" ]]; then
  check C2 status unknown "a real permission prompt becomes \`blocked\`, and how fast" \
    "skipped (needs a human and an agent pane)" "" "plugins/herd/src/model.ts:statusFromHerdr"
else
  printf '\n  \033[1mC2\033[0m The load-bearing claim of the whole design: Herdr'"'"'s `blocked` is a real\n'
  printf '     substitute for the AwaitingApproval detector two spikes went into.\n'
  printf '     In pane %s, ask your agent to do something that needs approval\n' "$AGENT_PANE"
  printf '     (e.g. a shell command it must ask about). Press Enter here the moment\n'
  printf '     the permission prompt appears on screen; I will poll for `blocked`.\n'
  printf '     > '
  read -r _ </dev/tty
  START="$(python3 -c 'import time; print(int(time.time()*1000))')"
  BLOCKED=""
  for _ in $(seq 1 100); do
    call status-poll agent.list >/dev/null
    if grep -q '"blocked"' "$OUT/raw/status-poll.json"; then
      BLOCKED="$(python3 -c "import time; print(int(time.time()*1000) - $START)")"
      break
    fi
    sleep 0.1
  done
  if [[ -n "$BLOCKED" ]]; then
    check C2 status pass "a real permission prompt becomes \`blocked\`" \
      "observed ${BLOCKED}ms after you pressed Enter (includes your reaction time; the approval-producer spike measured its own detector at 1.3–3.6ms from draw)" \
      "raw/status-poll.json" "plugins/herd/src/model.ts:statusFromHerdr"
  else
    check C2 status fail "a real permission prompt becomes \`blocked\`" \
      "no pane reported blocked within 10s — run \`$HERDR agent explain $AGENT_PANE --verbose\` and attach the output" \
      "raw/status-poll.json" "plugins/herd/src/model.ts:statusFromHerdr"
    "$HERDR" agent explain "$AGENT_PANE" --verbose > "$OUT/raw/explain-not-blocked.txt" 2>&1 || true
    scrub "$OUT/raw/explain-not-blocked.txt"
  fi
fi

# ---------------------------------------------------------------------------
# 11 — the SpacetimeDB half, if the CLI is here
# ---------------------------------------------------------------------------

bold "11. SpacetimeDB module"

if command -v spacetime >/dev/null 2>&1; then
  spacetime --version > "$OUT/raw/spacetime-version.txt" 2>&1 || true
  check L1 spacetime info "the spacetime CLI is available to publish the ported module" \
    "$(head -1 "$OUT/raw/spacetime-version.txt")" "raw/spacetime-version.txt" "services/hub/"
  note "publish check (against a scratch database, not your real one):"
  note "  spacetime publish --project-path services/hub herd-probe-scratch"
  note "then confirm the names came out snake_case:"
  note "  spacetime sql herd-probe-scratch 'SELECT * FROM session_status_history LIMIT 1'"
  ask L2 spacetime "the TypeScript module publishes, and its tables are snake_case" \
    "Run the two commands above. Did publish succeed, and does \`session_status_history\` exist under that name?" \
    "services/hub/src/schema.ts"
else
  check L1 spacetime unknown "the spacetime CLI is available to publish the ported module" \
    "spacetime not on PATH — this half is unvalidated" "" "services/hub/"
  for id in L2; do
    check "$id" spacetime unknown "the TypeScript module publishes with snake_case names" \
      "skipped (no spacetime CLI)" "" "services/hub/src/schema.ts"
  done
fi

# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------

python3 - "$JSONL" "$OUT/report.md" "$OUT" <<'PY'
import json, os, sys
from collections import Counter

rows = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8") if l.strip()]
counts = Counter(r["status"] for r in rows)
out = open(sys.argv[2], "w", encoding="utf-8")

w = out.write
w("# Herdr probe report\n\n")
w(f"`{os.path.basename(sys.argv[3])}` — generated by `scripts/probe-herdr.sh`\n\n")
w(f"**{counts.get('pass', 0)} pass, {counts.get('fail', 0)} fail, "
  f"{counts.get('unknown', 0)} unknown, {counts.get('info', 0)} informational.**\n\n")

if counts.get("fail"):
    w("## Failures — these are the ones to act on\n\n")
    for r in rows:
        if r["status"] == "fail":
            w(f"### {r['id']} — {r['assumption']}\n\n")
            w(f"- **observed:** {r['observed']}\n")
            if r["code_ref"]:
                w(f"- **code:** `{r['code_ref']}`\n")
            if r["evidence"]:
                w(f"- **evidence:** `{r['evidence']}`\n")
            w("\n")

if counts.get("unknown"):
    w("## Unanswered\n\nNot failures — the probe could not reach these.\n\n")
    for r in rows:
        if r["status"] == "unknown":
            w(f"- **{r['id']}** {r['assumption']} — {r['observed']}\n")
    w("\n")

w("## Everything, in order\n\n")
w("| id | area | status | assumption | observed |\n|---|---|---|---|---|\n")
for r in rows:
    obs = r["observed"].replace("|", "\\|")[:220]
    w(f"| {r['id']} | {r['area']} | {r['status']} | {r['assumption'].replace('|', chr(92) + '|')} | {obs} |\n")

w("\n## What to send back\n\nThe whole directory. The high-value files:\n\n")
w("- `report.md` and `assumptions.jsonl` — this, and its machine-readable form\n")
w("- `raw/field-audit.txt` — every field name the parsers probe, and which one was real\n")
w("- `raw/frame-audit.txt` — which field carries terminal bytes in an observe frame\n")
w("- `raw/session-snapshot.json`, `raw/agent-list.json` — to replace the doc-derived test fixtures\n")
w("- `raw/observe-shapes.ndjson` — observe records with payloads elided\n")
w("- `raw/events-wildcard.ndjson` — whether the daemon can subscribe or must poll\n")
w("- `env.txt` — versions, so a later run diffs meaningfully\n\n")
w("Captures are piped through the plugin's redactor when it is built. "
  "Skim them anyway before sharing: they are terminal output.\n")
out.close()

print()
print(f"  pass {counts.get('pass', 0)}   fail {counts.get('fail', 0)}   "
      f"unknown {counts.get('unknown', 0)}   info {counts.get('info', 0)}")
for r in rows:
    if r["status"] == "fail":
        print(f"  FAIL {r['id']}: {r['assumption']}")
PY

bold "done"
note "report:    $OUT/report.md"
note "telemetry: $OUT"
printf '\n     Send the whole directory back. If anything failed, %s\n' \
  "raw/field-audit.txt and raw/frame-audit.txt are the two files that fix it fastest."
