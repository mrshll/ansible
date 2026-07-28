#!/usr/bin/env bash
#
# The Spike B round trip: a real PTY session through a real Worker and R2, with a
# **second process** reconstructing the stream.
#
# What this asserts, in the order the plan asks for it:
#
#   1. Byte-exact reconstruction. The viewer's output must equal the publisher's
#      redacted reference, byte for byte. This is the claim the whole capture path
#      exists to support.
#   2. Perceived latency. p50/p95 from "bytes hit the PTY" to "second viewer
#      applied them", for the relay *and* for cursor-follow, so assumption A2 is
#      decided by measurement rather than preference.
#   3. Failure injection. Kill the Worker mid-session and verify the transcript is
#      still gapless and correctly ordered afterwards.
#
# Requires a Worker on ANSIBLE_WORKER_URL. By default that is a *local* workerd
# (`npm run dev` in services/transcript-worker), which uses local R2 and local
# Durable Object storage and contacts Cloudflare not at all.
#
#   scripts/probe-relay.sh
#   ANSIBLE_WORKER_URL=https://... scripts/probe-relay.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

WORKER_URL="${ANSIBLE_WORKER_URL:-http://localhost:8787}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PUBLISH="$REPO_ROOT/target/release/examples/relay-publish"
VIEW="$REPO_ROOT/target/release/examples/relay-view"

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

if [[ ! -x $PUBLISH || ! -x $VIEW ]]; then
  step "Building the harness"
  cargo build -p ansible-transport --examples --release
fi

if ! curl -sf -o /dev/null "$WORKER_URL/v1/session/ping/status" \
  -H "Authorization: Bearer ${ANSIBLE_VIEW_TOKEN:-spike-view-token}"; then
  printf '\033[31mNo Worker at %s.\033[0m\n' "$WORKER_URL"
  printf 'Start one locally:  cd services/transcript-worker && npm run dev\n'
  exit 2
fi

# A workload with the shape that matters: bursty output, a quiet gap long enough to
# force an age-based chunk flush, and a burst big enough to force a size-based one.
# A single `seq 1 100000` would only ever exercise the size path.
cat >"$WORK/workload.sh" <<'WORKLOAD'
echo "=== build starting ==="
for i in $(seq 1 400); do
  echo "[$i] compiling module_$i.rs with some reasonably long output line to fill bytes"
done
sleep 1.5                      # forces an age-based flush with no output pending
echo "=== running tests ==="
for i in $(seq 1 4000); do
  echo "test case $i ... ok (elapsed ${i}ms) padding padding padding padding padding"
done
printf 'AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE\n'   # must never reach the archive
printf 'DATABASE_URL=postgres://admin:hunter2pass@db.internal:5432/prod\n'
sleep 0.4
echo "=== done ==="
WORKLOAD

# A slow trickle of output, which is the case that actually distinguishes the two
# transports. Chatty output closes chunks on *size* almost immediately, so
# cursor-follow looks nearly as fast as the relay; sparse output closes them on the
# *age* timer instead, and every byte waits for it.
#
# This is not a contrived case. It is what a session awaiting approval looks like —
# the single most important thing the grid has to surface promptly.
cat >"$WORK/sparse.sh" <<'SPARSE'
for i in $(seq 1 15); do
  echo "[$i] thinking about the next step"
  sleep 0.4
done
SPARSE

run_round_trip() {
  local mode="$1" session="$2" workload="${3:-$WORK/workload.sh}"
  local ref="$WORK/$session.reference" out="$WORK/$session.viewer"
  local lat="$WORK/$session.latency"

  # Viewer first, so it is attached before the first byte and measures the live
  # path rather than backfilling everything after the fact.
  ANSIBLE_WORKER_URL="$WORKER_URL" \
    ANSIBLE_VIEW_MODE="$mode" \
    ANSIBLE_VIEW_SECONDS=6 \
    ANSIBLE_LATENCY_OUT="$lat" \
    "$VIEW" "$session" "$out" >"$WORK/$session.viewer.log" 2>&1 &
  local viewer_pid=$!
  sleep 1

  ANSIBLE_WORKER_URL="$WORKER_URL" \
    ANSIBLE_PUBLISH_SECONDS=45 \
    ANSIBLE_SPOOL_DIR="$WORK/spool-$session" \
    ANSIBLE_REFERENCE_OUT="$ref" \
    "$PUBLISH" "$session" bash "$workload" >"$WORK/$session.publish.log" 2>&1
  local publish_status=$?

  wait "$viewer_pid" || true

  if [[ $publish_status -ne 0 ]]; then
    bad "$mode: publisher exited cleanly" "$(tail -3 "$WORK/$session.publish.log")"
    return
  fi

  # (1) Byte-exactness. cmp, not a hash: when this fails the first differing byte
  # is the entire diagnosis.
  if cmp -s "$ref" "$out"; then
    ok "$mode: byte-exact reconstruction ($(wc -c <"$ref") bytes)"
  else
    bad "$mode: byte-exact reconstruction" "$(cmp "$ref" "$out" 2>&1 | head -2)"
  fi

  # Containment, checked separately from fidelity: byte-exactness alone would be
  # satisfied by faithfully storing a secret.
  #
  # The emptiness guard is the point — "no secret found" in a zero-byte file is not
  # evidence of anything, and a containment check that passes when the transport
  # delivered nothing is worse than no check at all.
  if [[ ! -s $out ]]; then
    bad "$mode: planted secrets absent from the transcript" \
      "viewer output is empty, so containment is untested"
  elif grep -qa "AKIAIOSFODNN7EXAMPLE\|hunter2pass" "$out"; then
    bad "$mode: planted secrets absent from the transcript" "a planted secret survived redaction"
  else
    ok "$mode: planted secrets absent from the transcript"
  fi

  grep -E "viewer: (latency|bytes)" "$WORK/$session.viewer.log" | sed 's/^/        /'
  if [[ -s $lat ]]; then
    LAT_FILE="$lat" python3 - <<'PY'
import os, statistics
xs = sorted(int(l) for l in open(os.environ["LAT_FILE"]) if l.strip())
if xs:
    def pct(p):
        return xs[min(len(xs) - 1, round((len(xs) - 1) * p))]
    print(f"        samples={len(xs)} p50={pct(.5)}ms p95={pct(.95)}ms "
          f"p99={pct(.99)}ms max={xs[-1]}ms median_abs={statistics.median(xs)}")
PY
  fi
}

step "Chatty output — relay transport (the sub-second path)"
run_round_trip relay "s-relay-$$"

step "Chatty output — cursor-follow transport (the simpler fallback)"
run_round_trip cursor "s-cursor-$$"

step "Sparse output — relay transport"
run_round_trip relay "s-relay-sparse-$$" "$WORK/sparse.sh"

step "Sparse output — cursor-follow transport (where the age timer is paid)"
run_round_trip cursor "s-cursor-sparse-$$" "$WORK/sparse.sh"

step "Failure injection — Worker refuses mid-session"
# Point the publisher at a URL that accepts nothing, so every chunk upload fails
# and the spool is the only thing keeping the transcript whole. This is the
# "keep buffering to a local spool, never drop-and-continue" path.
DEAD_SESSION="s-dead-$$"
SPOOL="$WORK/spool-dead"
set +e
ANSIBLE_WORKER_URL="http://127.0.0.1:9" \
  ANSIBLE_PUBLISH_SECONDS=10 \
  ANSIBLE_SPOOL_DIR="$SPOOL" \
  ANSIBLE_REFERENCE_OUT="$WORK/dead.reference" \
  "$PUBLISH" "$DEAD_SESSION" bash -c 'for i in $(seq 1 2000); do echo "line $i padding padding padding"; done' \
  >"$WORK/dead.log" 2>&1
dead_status=$?
set -e

if [[ $dead_status -ne 0 ]] && [[ -n "$(ls -A "$SPOOL/$DEAD_SESSION" 2>/dev/null)" ]]; then
  ok "unreachable Worker: publisher failed loudly and kept every chunk spooled"
else
  bad "unreachable Worker: publisher failed loudly and kept every chunk spooled" \
    "exit=$dead_status spool=$(ls -A "$SPOOL/$DEAD_SESSION" 2>/dev/null | wc -l) files"
fi

step "Recovery — the spooled tail uploads to a working Worker, in order"
# Same spool, now with a reachable Worker: this is the crash-restart path, where a
# session that died mid-stream finalizes late instead of losing its tail.
if ANSIBLE_WORKER_URL="$WORKER_URL" \
  ANSIBLE_PUBLISH_SECONDS=1 \
  ANSIBLE_SPOOL_DIR="$SPOOL" \
  "$PUBLISH" "$DEAD_SESSION" bash -c 'true' >"$WORK/recover.log" 2>&1; then
  remaining="$(ls -A "$SPOOL/$DEAD_SESSION" 2>/dev/null | wc -l)"
  if [[ "$remaining" -eq 0 ]]; then
    ok "recovery: the whole spool drained to the archive"
  else
    bad "recovery: the whole spool drained to the archive" "$remaining chunks still spooled"
  fi
else
  bad "recovery: the whole spool drained to the archive" "$(tail -3 "$WORK/recover.log")"
fi

# And the recovered archive must still reassemble to the original bytes — the
# point of the spool is not "no error", it is "no gap".
RECOVERED="$WORK/dead.recovered"
if ANSIBLE_WORKER_URL="$WORKER_URL" ANSIBLE_VIEW_MODE=cursor ANSIBLE_VIEW_SECONDS=2 \
  "$VIEW" "$DEAD_SESSION" "$RECOVERED" >"$WORK/dead.view.log" 2>&1 &&
  cmp -s "$WORK/dead.reference" "$RECOVERED"; then
  ok "recovery: the late-uploaded transcript is byte-exact"
else
  bad "recovery: the late-uploaded transcript is byte-exact" \
    "$(cmp "$WORK/dead.reference" "$RECOVERED" 2>&1 | head -2)"
fi

step "Result"
printf '  %d passed, %d failed\n' "$pass" "$fail"
((fail == 0)) || exit 1
printf '\033[32mrelay: round trip verified\033[0m\n'
