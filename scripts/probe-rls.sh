#!/usr/bin/env bash
#
# Does SpacetimeDB row-level security actually enforce the hub's visibility
# rules on Maincloud?
#
# This exists because the answer is not discoverable by reading. The
# `spacetimedb` 2.7.0 bindings carry this comment above the RLS attribute:
#
#     // TODO: RLS filters are currently unimplemented, and are not enforced.
#
# That comment is stale. The rules *are* enforced on Maincloud 2.7.0, and this
# script is the evidence. Open question #3 in docs/plan/multiplayer-hub.md calls
# this "a correctness dependency, not a nicety", so it gets a probe rather than a
# belief.
#
# Every assertion is made from the point of view of an identity that owns
# nothing, because that is the only viewpoint that can be wrong in a way that
# matters. Two facts make the test design non-obvious:
#
#   1. The module owner BYPASSES RLS and sees every row. So the probe must not
#      check anything from the owner's identity — it would pass unconditionally.
#   2. `spacetime --anonymous` mints a FRESH identity on every invocation, so it
#      cannot be used as a stable second principal across steps. The probe mints
#      real identities over the HTTP API instead and reuses their tokens.
#
#   scripts/probe-rls.sh              # against the default spike database
#   ANSIBLE_HUB_DB=my-db scripts/probe-rls.sh
set -euo pipefail

DB="${ANSIBLE_HUB_DB:-ansible-spike-b}"
HOST="${ANSIBLE_HUB_HOST:-https://maincloud.spacetimedb.com}"

# Unique per run so the probe is re-runnable without wiping the database.
RUN="$(date +%s)"
PRIVATE="s-private-$RUN"
SHARED="s-shared-$RUN"

pass=0
fail=0

step() { printf '\033[1m==> %s\033[0m\n' "$1"; }

# Mint a fresh identity, echoing "<hex identity> <bearer token>". Each call is a
# different principal, which is exactly what the "uninvolved third party" case
# needs. Both halves come from the one response because asking again would mint a
# different identity.
mint_identity() {
  curl -sS -X POST "$HOST/v1/identity" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["identity"], d["token"])'
}

# Run a SQL query as the holder of $1, printing rows as compact JSON.
sql_as() {
  local token="$1" query="$2"
  curl -sS -X POST "$HOST/v1/database/$DB/sql" \
    -H "Authorization: Bearer $token" \
    --data "$query" |
    python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[0]["rows"]))'
}

# assert_rows <description> <expected-json> <actual-json>
assert_rows() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  \033[32mPASS\033[0m %s\n' "$what"
    pass=$((pass + 1))
  else
    printf '  \033[31mFAIL\033[0m %s\n        expected %s\n        got      %s\n' \
      "$what" "$expected" "$actual"
    fail=$((fail + 1))
  fi
}

step "Seeding: one private session, one shared, one mention"
spacetime call "$DB" register_session \
  "$PRIVATE" "private work" laptop acme/api main opus-5 >/dev/null 2>&1
spacetime call "$DB" register_session \
  "$SHARED" "shared work" laptop acme/api main opus-5 >/dev/null 2>&1
# Sum-type reducer arguments are encoded as {"variant":{}} — lowercased.
spacetime call "$DB" set_session_visibility "$SHARED" '{"org":{}}' >/dev/null 2>&1

step "Minting two identities that own nothing"
read -r BOB_ID BOB <<<"$(mint_identity)"
read -r _CAROL_ID CAROL <<<"$(mint_identity)"

spacetime call "$DB" create_mention \
  "$SHARED" "0x$BOB_ID" "take this one" '{"chunk_seq":3,"byte_offset":128}' >/dev/null 2>&1

step "Reading as an identity that owns nothing"

# The core rule of the whole sharing model. A shared session's detail is
# readable; a private one is not visible at all — not even its existence.
assert_rows "session: shared row is visible" \
  "[[\"$SHARED\"]]" \
  "$(sql_as "$BOB" "SELECT session_id FROM session WHERE session_id = '$SHARED'")"

assert_rows "session: private row is hidden" \
  "[]" \
  "$(sql_as "$BOB" "SELECT session_id FROM session WHERE session_id = '$PRIVATE'")"

# The title-only directory card is org-visible by design, for both sessions.
# This is what lets a teammate see that you have a session, and who is watching
# it, without seeing any output.
assert_rows "session_listing: both titles are visible" \
  "[[\"private work\"],[\"shared work\"]]" \
  "$(sql_as "$BOB" "SELECT title FROM session_listing WHERE session_id = '$PRIVATE' OR session_id = '$SHARED'" |
    python3 -c 'import json,sys; print(json.dumps(sorted(json.load(sys.stdin)), separators=(",",":")))')"

# `to` and `from` are near-keywords; this confirms the dialect accepts them and
# that the recipient rule evaluates.
assert_rows "mention: the recipient can read it" \
  '[["take this one"]]' \
  "$(sql_as "$BOB" "SELECT body FROM mention WHERE session_id = '$SHARED'")"

assert_rows "mention: an uninvolved third party cannot" \
  "[]" \
  "$(sql_as "$CAROL" "SELECT body FROM mention WHERE session_id = '$SHARED'")"

# Negative control. If this ever returns rows, the filters have stopped being
# selective and every assertion above is passing for the wrong reason.
assert_rows "notification_route: nobody else's route is readable" \
  "[]" \
  "$(sql_as "$CAROL" "SELECT slack_user_id FROM notification_route")"

step "Result"
printf '  %d passed, %d failed\n' "$pass" "$fail"
if ((fail > 0)); then
  printf '\033[31mRLS is not behaving as the schema assumes. Do not ship Private as a\n'
  printf 'security boundary until this is understood.\033[0m\n'
  exit 1
fi
printf '\033[32mrls: enforced as designed\033[0m\n'
