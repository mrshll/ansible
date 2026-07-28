#!/usr/bin/env bash
#
# Do the hub's stated invariants actually hold on a deployed module?
#
# The plan makes four load-bearing claims about the hub. Each is the kind of thing
# that is true in the source and false in production if a host detail differs, so
# each gets an assertion against real Maincloud rather than a unit test:
#
#   1. The transcript cursor is Worker-only. Otherwise it means "a client claimed
#      so" instead of "durably in R2", and viewers following it read past the end
#      of the archive.
#   2. The cursor is strictly monotonic. A cursor that can move backwards breaks
#      every viewer holding a later offset, and makes byte-exact reassembly
#      impossible.
#   3. Status history is O(transitions), not O(reports). The plan calls the row
#      budget "a hard invariant, not a guideline". `update_session_status` is the
#      hottest reducer in the system, so this is where that budget is won or lost.
#   4. A session may only be written by its owner.
#
#   scripts/probe-hub.sh
#   ANSIBLE_HUB_DB=my-db scripts/probe-hub.sh
#
# Companion to scripts/probe-rls.sh, which covers read visibility.
set -euo pipefail

DB="${ANSIBLE_HUB_DB:-ansible-spike-b}"
HOST="${ANSIBLE_HUB_HOST:-https://maincloud.spacetimedb.com}"

RUN="$(date +%s)"
SESSION="s-invariants-$RUN"

pass=0
fail=0

step() { printf '\033[1m==> %s\033[0m\n' "$1"; }

ok() {
  printf '  \033[32mPASS\033[0m %s\n' "$1"
  pass=$((pass + 1))
}
bad() {
  printf '  \033[31mFAIL\033[0m %s\n        %s\n' "$1" "${2:-}"
  fail=$((fail + 1))
}

# Assert a reducer call fails, and that it fails for the stated reason rather
# than incidentally. A refusal with the wrong message is not the refusal we mean.
refuses() {
  local what="$1" expect="$2"
  shift 2
  local out
  out="$("$@" 2>&1 || true)"
  if grep -qF "$expect" <<<"$out"; then
    ok "$what"
  else
    bad "$what" "expected a refusal containing '$expect', got: $(tr -d '\n' <<<"$out" | head -c 200)"
  fi
}

accepts() {
  local what="$1"
  shift
  local out
  out="$("$@" 2>&1 || true)"
  # `spacetime call` prints nothing on success; the HTTP path returns an empty body.
  if grep -qiE "error|refus|must|cannot|not owned" <<<"$out"; then
    bad "$what" "expected success, got: $(tr -d '\n' <<<"$out" | head -c 200)"
  else
    ok "$what"
  fi
}

mint_identity() {
  curl -sS -X POST "$HOST/v1/identity" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["identity"], d["token"])'
}

# Reducer call as an arbitrary identity, which `spacetime call` cannot do.
call_as() {
  local token="$1" reducer="$2" args="$3"
  curl -sS -X POST "$HOST/v1/database/$DB/call/$reducer" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" --data "$args"
}

count_history() {
  spacetime sql --format json "$DB" \
    "SELECT COUNT(*) AS n FROM session_status_history WHERE session_id = '$SESSION'" 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["rows"][0][0])'
}

step "Seeding a session and designating a Worker identity"
spacetime call "$DB" register_session \
  "$SESSION" "invariants" laptop acme/api main opus-5 >/dev/null 2>&1
read -r WORKER_ID WORKER <<<"$(mint_identity)"
read -r _OTHER_ID OTHER <<<"$(mint_identity)"
spacetime call "$DB" set_worker_identity "0x$WORKER_ID" >/dev/null 2>&1

step "1 & 2 — the transcript cursor"

refuses "cursor: the session owner may not advance it" \
  "only the Worker identity" \
  spacetime call "$DB" advance_transcript_cursor "$SESSION" 5 1000 3

refuses "cursor: an unrelated identity may not advance it" \
  "only the Worker identity" \
  call_as "$OTHER" advance_transcript_cursor "[\"$SESSION\",5,1000,3]"

accepts "cursor: the Worker advances 0 -> 5" \
  call_as "$WORKER" advance_transcript_cursor "[\"$SESSION\",5,1000,3]"

refuses "cursor: replaying the same value is refused" \
  "must advance" \
  call_as "$WORKER" advance_transcript_cursor "[\"$SESSION\",5,1000,3]"

refuses "cursor: moving backward is refused" \
  "must advance" \
  call_as "$WORKER" advance_transcript_cursor "[\"$SESSION\",2,400,1]"

refuses "cursor: a regressing byte cursor is refused" \
  "must not regress" \
  call_as "$WORKER" advance_transcript_cursor "[\"$SESSION\",6,10,4]"

accepts "cursor: the Worker advances 5 -> 6" \
  call_as "$WORKER" advance_transcript_cursor "[\"$SESSION\",6,1200,4]"

step "3 — status history is O(transitions), not O(reports)"

before="$(count_history)"
# Twenty identical reports. A naive implementation writes twenty rows; this is
# precisely the shape the hot path produces, because the status machine reports
# on every hook event whether or not anything changed.
for _ in $(seq 20); do
  spacetime call "$DB" update_session_status \
    "$SESSION" '{"working":{}}' "running: Bash" '{"hook":{}}' >/dev/null 2>&1
done
after_repeats="$(count_history)"

if [[ "$after_repeats" == "$((before + 1))" ]]; then
  ok "history: 20 identical reports wrote exactly 1 transition row"
else
  bad "history: 20 identical reports wrote exactly 1 transition row" \
    "history went from $before to $after_repeats"
fi

# A detail-only change is not a transition either: `running: Bash` ->
# `running: Read` is the same status and must not cost a row.
spacetime call "$DB" update_session_status \
  "$SESSION" '{"working":{}}' "running: Read" '{"hook":{}}' >/dev/null 2>&1
after_detail="$(count_history)"
if [[ "$after_detail" == "$after_repeats" ]]; then
  ok "history: a detail-only change wrote no transition row"
else
  bad "history: a detail-only change wrote no transition row" \
    "history went from $after_repeats to $after_detail"
fi

# And a real transition must still be recorded, or the test above is passing
# because nothing is ever written.
spacetime call "$DB" update_session_status \
  "$SESSION" '{"awaitingInput":{}}' "" '{"hook":{}}' >/dev/null 2>&1
after_move="$(count_history)"
if [[ "$after_move" == "$((after_detail + 1))" ]]; then
  ok "history: a real status change did write one row"
else
  bad "history: a real status change did write one row" \
    "history went from $after_detail to $after_move"
fi

step "Status provenance — the Spike B hook-coverage finding, enforced"

# Hooks cannot distinguish "awaiting a human" from "running a slow tool": a
# denied tool and an 8-second tool produce identical hook sequences. So the
# reducer refuses the status from the hook path rather than letting the guess
# ship. See docs/spikes/hook-coverage.md §3.
refuses "status: AwaitingApproval is refused from the hook path" \
  "may only be reported by StatusSource::Terminal" \
  spacetime call "$DB" update_session_status \
  "$SESSION" '{"awaitingApproval":{}}' "awaiting approval: Bash" '{"hook":{}}'

accepts "status: AwaitingApproval is accepted from the terminal" \
  spacetime call "$DB" update_session_status \
  "$SESSION" '{"awaitingApproval":{}}' "awaiting approval: Bash" '{"terminal":{}}'

# SessionEnd.reason was "other" even on a clean exit, so the hook cannot tell
# success from failure. Only the supervisor's exit status can.
refuses "status: Failed is refused from the hook path" \
  "may only be reported by StatusSource::Supervisor" \
  spacetime call "$DB" update_session_status \
  "$SESSION" '{"failed":{}}' "exit 1" '{"hook":{}}'

step "4 — a session may only be written by its owner"

refuses "ownership: a stranger cannot change the status" \
  "not owned by the caller" \
  call_as "$OTHER" update_session_status "[\"$SESSION\",{\"working\":{}},\"\",{\"hook\":{}}]"

refuses "ownership: a stranger cannot share the transcript" \
  "not owned by the caller" \
  call_as "$OTHER" set_session_visibility "[\"$SESSION\",{\"org\":{}}]"

refuses "ownership: a stranger cannot re-register the session id" \
  "already exists under another owner" \
  call_as "$OTHER" register_session \
  "[\"$SESSION\",\"stolen\",\"host\",\"repo\",\"main\",\"opus-5\"]"

accepts "ownership: re-registering as the owner re-attaches (idempotent)" \
  spacetime call "$DB" register_session \
  "$SESSION" "invariants" laptop acme/api main opus-5

step "Result"
printf '  %d passed, %d failed\n' "$pass" "$fail"
((fail == 0)) || exit 1
printf '\033[32mhub: invariants hold\033[0m\n'
