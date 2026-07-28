#!/usr/bin/env bash
#
# Record real Claude Code hook payloads by installing a session-scoped hook
# overlay and running a real session.
#
#   scripts/capture-hook-payloads.sh [outdir]
#
# Produces, in outdir (default: a temp dir):
#   tool-allowed.jsonl   a session where the tool ran
#   tool-denied.jsonl    a session where a PreToolUse hook denied the tool
#
# Those are the fixtures crates/ansible-hooks/tests/fixtures/ holds. Re-running
# this is how you re-record them after a Claude Code upgrade; the diff then shows
# exactly which payload shapes moved.
#
# Requires `claude` on PATH and working credentials.
set -euo pipefail

OUT="${1:-$(mktemp -d)}"
mkdir -p "$OUT"
WORK="$OUT/work"
mkdir -p "$WORK"

# Hook commands receive the payload as JSON on stdin. Log one line per
# invocation so a recording replays in arrival order.
cat > "$OUT/receiver.py" <<'PY'
#!/usr/bin/env python3
"""Append one JSON line per hook invocation: {event, at_ms, payload}."""
import json, os, sys, time

event = sys.argv[1] if len(sys.argv) > 1 else "unknown"
log = os.environ["HOOK_LOG"]
raw = sys.stdin.read()
try:
    payload = json.loads(raw) if raw.strip() else None
except json.JSONDecodeError:
    payload = {"_unparsed": raw[:4000]}
with open(log, "a", encoding="utf-8") as f:
    f.write(json.dumps({"event": event, "at_ms": int(time.time() * 1000), "payload": payload}) + "\n")
PY

# A PreToolUse hook that denies Bash. This is how the deny path is reached
# without needing an interactive approval prompt, which some environments
# (including CI and this container) cannot produce.
cat > "$OUT/deny.py" <<'PY'
#!/usr/bin/env python3
import json, os, sys, time

raw = sys.stdin.read()
try:
    payload = json.loads(raw)
except json.JSONDecodeError:
    payload = {}
with open(os.environ["HOOK_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"event": "PreToolUse", "at_ms": int(time.time() * 1000), "payload": payload}) + "\n")

if payload.get("tool_name") == "Bash":
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": "capture-hook-payloads.sh: denied to record the deny path",
    }}))
PY

# Subscribe to every event name that appears in the Claude Code binary, so the
# recording shows which ones actually fire rather than only the expected ones.
python3 - "$OUT" <<'PY'
import json, sys
out = sys.argv[1]
events = ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Notification", "Stop",
          "SubagentStop", "SessionStart", "SessionEnd", "PreCompact",
          "PostToolUseFailure", "PermissionRequest"]

def overlay(deny: bool) -> dict:
    hooks = {}
    for e in events:
        cmd = f"python3 {out}/deny.py" if (deny and e == "PreToolUse") \
              else f"python3 {out}/receiver.py {e}"
        hooks[e] = [{"hooks": [{"type": "command", "command": cmd}]}]
    return {"hooks": hooks}

json.dump(overlay(False), open(f"{out}/settings-allow.json", "w"), indent=2)
json.dump(overlay(True), open(f"{out}/settings-deny.json", "w"), indent=2)
PY

record() {
  local label="$1" settings="$2" mode="$3"
  local log="$OUT/$label.jsonl"
  rm -f "$log"
  echo "==> recording $label"
  (
    cd "$WORK"
    HOOK_LOG="$log" claude --print \
      --settings "$settings" \
      ${mode:+--permission-mode "$mode"} \
      'Use Bash to run: echo hook-payload-capture' < /dev/null > /dev/null 2>&1 || true
  )
  if [[ ! -s "$log" ]]; then
    echo "    no events recorded - is 'claude' authenticated?" >&2
    return 1
  fi
  python3 -c "
import json, sys
rows = [json.loads(l) for l in open('$log')]
print('    ' + ' -> '.join(r['event'] for r in rows))
"
}

record tool-allowed "$OUT/settings-allow.json" acceptEdits
record tool-denied "$OUT/settings-deny.json" ""

echo
echo "fixtures in $OUT"
echo "to update the checked-in copies:"
echo "  cp $OUT/tool-{allowed,denied}.jsonl crates/ansible-hooks/tests/fixtures/"
