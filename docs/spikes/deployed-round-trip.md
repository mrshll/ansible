# Spike B — the deployed half: hub, Worker, relay, round trip

**Status:** done. The parts that were blocked on credentials are now built,
deployed where deployable, and measured.

This completes Spike B. The local half — byte-exact capture and the redaction
ruleset — is in [capture-round-trip.md](capture-round-trip.md); the hook and status
findings are in [hook-coverage.md](hook-coverage.md). This document covers what
those two left open: a real SpacetimeDB module on Maincloud, a real Worker with R2
and a Durable Object relay, and a **second process** reconstructing a live session
byte for byte.

The two decisions this spike settled have their own ADRs, which record the reasoning
and the rejected alternatives; this document is the evidence behind them:
[ADR 0002 — live-tail transport](../adr/0002-live-tail-transport.md) and
[ADR 0003 — read authorization](../adr/0003-read-authorization.md).

Headline findings, in descending order of how much they change the plan:

1. **Row-level security works, and the bindings say it doesn't.** The 2.7.0 source
   carries `// TODO: RLS filters are currently unimplemented, and are not
   enforced.` That comment is stale. Filters are enforced per-row and per-identity
   on Maincloud. Trusting the comment would have grown a Worker-mediated read path
   the design does not need. Open question #3's RLS half: **answered, favourably.**
2. **RLS cannot compare an enum column to a literal**, which forced the one schema
   change in this spike: `session.shared_with_org: bool` alongside
   `session.visibility: Visibility`.
3. **Cursor-follow alone cannot meet the latency target — and it misses worst
   exactly when it matters most.** Its p95 is 1.3–1.6 s *on localhost*, before any
   network. The relay is 3 ms. Assumption A2: **keep the relay.**
4. **The module owner bypasses RLS.** `Private` is a boundary between teammates,
   not between a teammate and whoever holds the publish credential.
5. **A reducer *can* read verified token claims**, and `Identity` is confirmed to be
   exactly `from_claims(issuer, subject)`. This was not on the spike's list — it
   answers one of the three questions the W1 runbook says cannot be answered from
   this repo, and it shrinks the remaining identity spike. See
   [§9](#9-identity-a-question-answered-early).

---

## 1. What was built

```
services/hub-module/               SpacetimeDB module — the schema source of truth
  src/lib.rs                       tables, enums, lifecycle reducers
  src/reducers.rs                  the reducer surface, grouped by caller
  src/rls.rs                       row-level security filters
services/transcript-worker/        Cloudflare Worker
  src/index.ts                     routes and the authorization boundary
  src/relay.ts                     SessionRelay Durable Object: fan-out + R2 + cursor
  src/protocol.ts                  chunk envelope and relay messages (port of the Rust)
  src/hub.ts                       the Worker's one write: advance_transcript_cursor
crates/ansible-transport/          publish and reconstruct
  src/publisher.rs                 spool, upload, backoff, recover
  src/viewer.rs                    LiveViewer: the relay/archive join
  src/spool.rs                     chunks that exist but are not yet durable
  examples/relay_publish.rs        PTY -> redact -> chunk -> publish
  examples/relay_view.rs           the second process
scripts/probe-rls.sh               6 assertions: read visibility
scripts/probe-hub.sh               17 assertions: cursor, row budget, ownership
scripts/probe-relay.sh             11 assertions: byte-exactness, latency, recovery
```

Deployed: **`ansible-spike-b` on SpacetimeDB Maincloud**, a new database that
touches neither the existing `ansible` nor `ansible-dev`.

Not deployed: **the Worker.** It runs on local `workerd` with local R2 and local
Durable Object storage, which exercises the real bindings and the real DO lifecycle
without contacting Cloudflare. Deploying it needs an explicit decision about which
Cloudflare account it lands in — see [§6](#6-what-is-still-not-deployed).

`cargo test --workspace` — **178 passed, 0 failed** (up from 133; 12 new in
`ansible-transport`). `scripts/lint.sh` clean, now including the wasm module and
`tsc` for the Worker, both of which it previously did not cover.

---

## 2. Row-level security: enforced, with three constraints

Verified by `scripts/probe-rls.sh`, which asserts **only** from the viewpoint of an
identity that owns nothing — the one viewpoint that can be wrong in a way that
matters.

| Assertion, as a non-owner | Result |
|---|---|
| A shared session's detail row is visible | visible |
| A private session's detail row | **hidden** |
| Both sessions' title-only listings | visible |
| A mention addressed to me | visible |
| A mention between two other people | **hidden** |
| Someone else's notification route | **hidden** |

The positive cases are what make this trustworthy. A blanket deny would satisfy
every "hidden" row above and look identical, so the probe also proves the filters
*grant* correctly: flipping one session to `Org` makes exactly that row appear, and
a non-owner sees 1 of 3 sessions rather than 0 or 3.

### The three constraints

**RLS requires public tables.** Publishing a filter on a `private` table fails:

```
Error: failed to create row-level security: ... Cannot define RLS rule on private
table: session. Please make table public if you wish to restrict access using RLS.
```

So `public` here means "subscribable, then filtered", not "world-readable". Worth
stating explicitly in review, because `public` reads like the opposite.

**RLS cannot compare an enum column to a literal.** This is the finding with
schema consequences. `WHERE visibility = 'Org'` is rejected:

```
Error: failed to create row-level security: ... The literal expression `Org` cannot
be parsed as type `(org: () | private: () | granted: ())`
```

Lowercase fails identically. There is no literal syntax for a unit variant, so the
single most important rule in the system — *this session is shared with the org* —
is inexpressible against the typed column.

The fix is a redundant boolean, `session.shared_with_org`, written in the same
reducer body and therefore the same transaction as `visibility`. The enum stays the
source of truth because it is the honest type; the boolean is a projection of it for
the query planner. The cost is a two-field invariant, which is why
`set_session_visibility` is the only thing allowed to write either.

**The module owner bypasses RLS.** With three sessions owned by three identities,
the module owner's query returns all three; every other identity sees only what the
rules allow. This is conventional (Postgres `BYPASSRLS` behaves the same way) but it
bounds what `Private` promises, and the plan should say so rather than implying
transcripts are private *from the operator*.

### One more thing worth knowing

`spacetime --anonymous` mints a **new identity on every invocation**, so it cannot
serve as a stable second principal across steps. The probes mint identities through
`POST /v1/identity` and reuse the bearer token, which is also how the Worker
authenticates. Anyone writing multi-identity tests will hit this.

---

## 3. Hub invariants, checked against the deployed module

`scripts/probe-hub.sh` — 17 assertions, all passing against Maincloud.

**The cursor means "durably in R2", and that is mechanical.** It is advanced only by
the identity in `hub_config.worker_identity`, and only forward:

| Attempt | Result |
|---|---|
| Before any Worker identity is configured | refused — "refusing to trust a cursor" |
| By the session's own owner | refused |
| By an unrelated identity | refused |
| By the Worker, 0 → 5 | accepted |
| By the Worker, replaying 5 | refused — "must advance" |
| By the Worker, 5 → 2 | refused |
| By the Worker, with a regressing byte cursor | refused — "must not regress" |

The unset case matters more than it looks: a hub with no configured Worker refuses
cursors outright rather than defaulting to trusting the caller.

**The row budget holds.** The plan calls `O(sessions) + O(transitions)` "a hard
invariant, not a guideline". Measured, on the deployed module:

- 20 identical `update_session_status` calls → **1** history row.
- A detail-only change (`running: Bash` → `running: Read`) → **0** new rows.
- A real status change → **1** row.

That is the difference between a table that grows with *activity* and one that grows
with *reports*, and the hot path produces reports on every hook event.

**The hook-coverage findings are now enforced by the schema, not just documented.**
`update_session_status` carries a `StatusSource`, and rejects:

- `AwaitingApproval` from anything but `Terminal` — because a denied tool and a slow
  tool produce byte-identical hook sequences, so the hook path cannot know.
- `Failed` from anything but `Supervisor` — because `SessionEnd.reason` was `"other"`
  on a clean exit.

Nobody can wire the guess up by accident now. `Idle` was **dropped** from
`SessionStatus`: Spike B found nothing can set it, and the viewer derives it from
`last_event_at`.

---

## 4. The round trip, and the A2 decision

`scripts/probe-relay.sh` runs a real PTY workload through the Worker twice per
transport, then injects failures. 11 assertions, all passing.

### Byte-exactness

The viewer's output equals the publisher's redacted reference, byte for byte, in
every configuration: relay and cursor-follow, chatty and sparse, and after
recovering a spooled tail. 350,954 bytes in the chatty case.

Checked separately from **containment**: planted credentials
(`AKIA…`, `postgres://admin:hunter2pass@…`) appear nowhere in what the viewer
reconstructs. Byte-exactness alone would be satisfied by faithfully storing a
secret, so the two are different claims. The containment check also fails if the
viewer's output is *empty* — a "no secret found" that passes on a zero-byte file is
worse than no check, and it passed on an empty file exactly once before that guard
existed.

### Latency

Local `workerd`, so treat these as a **floor**, not a prediction. Measured from
"bytes left the PTY" to "second process applied them", one sample per frame and one
per record.

| Output shape | Transport | p50 | p95 | p99 | max |
|---|---|---:|---:|---:|---:|
| Chatty (~350 KB burst) | **relay** | 2 ms | 3 ms | 3 ms | 4 ms |
| Chatty | cursor-follow | 134 ms | **1,565 ms** | 1,574 ms | 1,578 ms |
| Sparse (a line every 400 ms) | **relay** | 2 ms | 3 ms | 3 ms | 3 ms |
| Sparse | cursor-follow | **848 ms** | **1,288 ms** | 1,314 ms | 1,314 ms |

**A2: keep the relay.** Cursor-follow's p95 already exceeds the plan's sub-second
target on loopback, with zero network latency, and the gap is structural rather than
tunable: a byte cannot become durable before its chunk closes, and a chunk does not
close until 64 KiB or 1 s. The relay's cost is bounded by the network alone.

The sparse row is the one that decides it. Chatty output closes chunks on *size*
almost immediately, which flatters cursor-follow; sparse output pays the full age
timer on every byte. And sparse output is not a corner case — it is what a session
awaiting approval looks like, which is the single most important thing the grid has
to surface promptly. **Cursor-follow is slowest precisely when the product needs to
be fastest.**

Cursor-follow remains necessary as the backfill and recovery path, and the viewer
cannot tell which path supplied a byte. It is just not sufficient as the live one.

#### A measurement error worth recording

The first version of this table reported cursor-follow p50 at ~50 ms, which is
wrong, and wrong in the flattering direction. It timed each chunk from
`ended_at_ms` — the timestamp of its *last* record. The last record in a chunk is
fresh the instant the chunk closes; the first has been waiting the whole flush
interval. Timing per *record* from each record's own arrival moved sparse p50 from
~50 ms to 848 ms and changed the A2 conclusion. The relay was always measured per
frame, so the comparison had been apples to oranges.

### Failure injection

| Injected | Behaviour |
|---|---|
| Worker unreachable for the whole session | Publisher keeps reading, keeps chunking, keeps every chunk spooled, exits non-zero. No gap, no silent truncation. |
| Same spool re-run against a working Worker | Whole spool drains in ascending order; transcript is byte-exact. |
| Chunk arriving out of order | Worker returns 409 and names the expected sequence. |
| Chunk whose `byte_start` does not continue the archive | 409. |
| Truncated upload body | 400 — caught by `record_count`, not accepted as a short chunk. |
| Replay of an already-durable chunk | 204, idempotent, so a retry after a timeout is not an error. |

The spool is ordered **numerically, not lexically**. `10.jsonl` sorts before
`9.jsonl` as text, and replaying in that order would make the Worker reject the tail
as a gap — stalling the session the recovery exists to unstall. There is a test for
it.

---

## 5. Bugs this found

**A viewer can never be sent a `WebSocket` from a Durable Object RPC method.** It
fails with `Web Socket request did not return status 101`. WebSockets cannot cross
the RPC boundary, so the relay upgrade has to be a `fetch` handler while everything
else stays RPC. Not documented anywhere obvious; cost an hour.

**A Durable Object constructor cannot set up its schema in
`blockConcurrencyWhile`** if it also reads that schema in the constructor. The
callback is async and the constructor cannot await it, so the first read hit
`no such table: relay_state`. `sql.exec` is synchronous, so the setup belongs inline
— which is not a shortcut but the only correct order.

**The publisher aborted the session on the first failed upload.** It propagated the
error out of the read loop, which truncated the transcript at the point of failure —
the exact outcome the spool exists to prevent. The plan says to keep buffering and
let the cursor stall; it now does, and the reference stream is written before any
recovery is attempted so a failed upload still leaves something to compare against.

**The cursor-follow latency metric flattered its own transport.** See §4.

---

## 6. What is still not deployed

**The Worker.** Everything about it is exercised — routes, authorization, R2 writes,
Durable Object ordering state across eviction, WebSocket fan-out, the hub call —
against local `workerd` with local R2. What local mode cannot produce is network
latency to a real edge, and real R2 durability semantics.

It is not deployed because deploying it creates resources in a Cloudflare account,
and the accounts on this machine (`dynamical.org`, `upstream.tech`) serve production
traffic. That needs an explicit decision, not a default. When it happens:

- Set `HUB_TOKEN` to a bearer token for the identity registered as
  `hub_config.worker_identity`, as a **secret**, not a var. Without it the Worker
  logs `hub is not configured` and the cursor never advances — which the local runs
  demonstrate is a freshness failure, not a correctness one: the bytes still land in
  R2 and viewers still reconstruct byte-exactly.
- Re-run `scripts/probe-relay.sh` with `ANSIBLE_WORKER_URL` pointing at the
  deployment. The latency table in §4 should be re-measured, not extrapolated.

**The tokens are shared secrets**, matching the plan's "no auth beyond a hardcoded
token" for Spike B. Two things must change before Phase 1, and both are marked in
the source:

- The view token rides in the WebSocket query string, because browsers cannot set
  headers on a WebSocket. It lands in logs. Phase 1 wants a short-lived ticket
  fetched over HTTP first.
- A token cannot express "sharing was just turned off". Phase 1 must check the hub
  for the session's current visibility on every read, which is what makes
  un-sharing take effect rather than merely being recorded.

**`upsert_member` trusts client-asserted GitHub claims.** `#[unique]` on
`github_login` stops someone claiming a login already taken, and that is all. The
verified path from GitHub OAuth to a SpacetimeDB `Identity` is the unresolved half
of open question #3 and it is unchanged by this spike. Phase 1 must not ship this
reducer as written; the doc comment on it says so.

---

## 7. Where the plan should change

| Plan text | Change |
|---|---|
| §0 A2 — "Spike B may select R2 cursor-follow if relay complexity or cost is disproportionate" | **Resolved: keep the relay.** Cursor-follow's p95 exceeds the target on loopback, and is worst on sparse output — the awaiting-approval case. |
| §2 tables — `session.visibility` | Add `shared_with_org: bool`. RLS cannot compare an enum to a literal, and the org-visibility rule is the most important one in the system. |
| §2 enums — `SessionStatus` includes `Idle` | Drop `Idle`; nothing can set it. Derive from `last_event_at`. |
| §2 reducers — `update_session_status(session_id, status, detail)` | Add `source: StatusSource`. `AwaitingApproval` must come from the terminal and `Failed` from the supervisor; the reducer enforces both. |
| §2 row-level security — "Verify RLS support and expressiveness on Maincloud early" | **Done.** Enforced. No filtering intermediary needed for reads. Note the module-owner bypass and the public-table requirement. |
| §2 reducers — `advance_transcript_cursor` "called by the Worker, never the app" | Make it mechanical: add `hub_config.worker_identity` and a `set_worker_identity` reducer. Refuse cursors when it is unset. |
| §3 step 3 — "batches them at ~64KB or ~1s ... into r2://" | Clarify that the *app* chunks and the Worker persists what it receives. Re-batching in the Worker would create a second notion of chunk boundaries and break the byte offsets that mention anchors depend on. |
| §3 step 3 — bytes → "ordered frames over a Worker WebSocket" | Name the two streams explicitly: ephemeral frames addressed by **byte range**, durable chunks addressed by **sequence**. The viewer splices by absolute offset, which is what makes the join safe. |

## 8. Open questions this moves

**#3 — the verified path from GitHub OAuth to `Identity`, and can RLS express the
visibility rules?** The RLS half is answered: yes, with the three constraints in §2,
and reads do **not** need a filtering intermediary. The OAuth half is untouched and
still gates Phase 1. Note that the two halves are less coupled than the plan
assumed — RLS being real means the trust path only has to establish *who you are*,
not also *what you may read*.

**#4 — who owns redaction, and what happens when a rule misses?** Sharpened. The
publisher redacts once, before any byte reaches either stream, and the round trip
confirms nothing unredacted reaches R2. But the Worker sees only already-redacted
bytes, so a Worker-side second line of defence would be scanning material that has
already been scanned by the only component that knows the ruleset. If a second line
is wanted, the honest place is a re-scan pass over R2 with a newer ruleset version —
which §8 of [capture-round-trip.md](capture-round-trip.md) already notes must be a
new-generation write, because rewriting a chunk in place breaks byte-exactness for
anyone holding a later cursor.

**#5 — retention and deletion.** Unchanged, and now with a mechanism attached: the
cursor is *enforced* monotonic on the deployed module, so a scrub cannot move it
backward. Deletion has to be a new generation or a tombstone, not an edit.

**A1 — transcript fidelity.** The raw-archive half is now proven end to end. The
derived structured event index is still unbuilt; nothing here contradicts it, and
the `events/` prefix on the same chunk path remains available.

---

## 9. Identity: a question answered early

`docs/plan/w1-provisioning.md` lists three things to find out during provisioning,
calls them the whole input to the identity spike, and says *"none of them can be
answered from this repo."* The second one now can be, and it was answered by
accident while wiring the Worker's authentication.

**The question:** *Can a reducer read claims from the caller's token, or only
`ctx.sender`?* `upsert_member_from_token()` is specified to read **verified** claims
and never a client-asserted login. If only the identity were available, membership
would have to be established by a trusted writer instead — a materially different
design.

**The answer: yes.** `ctx.sender_auth()` returns an `AuthCtx`, whose `jwt()` yields
`JwtClaims` with `issuer()`, `subject()`, `audience()`, and `raw_payload()` for
custom claims. The `whoami` reducer logs this, and against deployed Maincloud with a
minted token:

```
whoami: sender=c200b75e…5ec0a is_internal=false has_jwt=true
whoami: issuer="localhost" subject="4da374b2-…" audience=["spacetimedb"]
        derived_identity=c200b75e…5ec0a
whoami: claim_names=["aud", "exp", "hex_identity", "iat", "iss", "sub"]
```

Two things worth extracting from those three lines.

**`derived_identity` is byte-identical to `sender`.** `JwtClaims::identity()` is
`Identity::from_claims(issuer, subject)`, and it reproduces the sender exactly. That
confirms the *mechanism* the runbook's first question hypothesizes: identity derives
from `(iss, sub)`, so if a token can be issued with a stable subject per GitHub user,
that user has a stable `Identity`. It is no longer a hypothesis about how the platform
might work.

**Custom claims are readable.** `raw_payload()` is the whole payload as JSON, so a
`github_login` claim minted by our Worker would be readable — and *verified*, because
the host validated the signature before the reducer ran.

**What this does not answer.** The runbook's first question is really two: *how*
identity derives from claims (answered above) and *whether Maincloud can be
configured to trust a third-party JWT issuer* — an issuer URL plus a JWKS endpoint.
That second half is a console and platform-support question, not a code question, and
it is still open. It remains the last thing gating Phase 1.

But the shape of the fix is now known rather than guessed:

1. Our Worker completes GitHub OAuth and mints a JWT with a stable `sub` and a
   `github_login` claim.
2. Maincloud is configured to trust that issuer — **the one unverified step.**
3. `upsert_member` reads the login from `ctx.sender_auth()` instead of its argument,
   which closes the hole its own doc comment currently warns about.

Note that step 3 is a small change to one reducer, not an architectural shift. The
plan's worry — that a failed identity hypothesis routes *all* writes through the
Worker under one service identity — turns out to hinge entirely on step 2.

The `whoami` reducer is left deployed. It reads only the caller's own token, writes no
rows, and logs claim **names** rather than values: a real token may carry an email or
a display name, and a diagnostic that dumps a token payload into a log is how those
end up somewhere nobody meant to put them.
