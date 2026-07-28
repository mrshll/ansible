# ADR 0002 — Live-tail transport

- **Status:** accepted
- **Date:** 2026-07-28
- **Deciders:** Spike B
- **Evidence:** [docs/spikes/deployed-round-trip.md](../spikes/deployed-round-trip.md)

## Context

A teammate opening a shared session should see output as it happens. The
architecture plan named this assumption A2 and left it explicitly open: aim for
sub-second via a Worker-hosted Durable Object relay, but *"Spike B may select R2
cursor-follow if relay complexity or cost is disproportionate."*

Two transports were candidates, and they are not variations of one design:

1. **Cursor-follow.** The app chunks output and uploads it; the Worker writes each
   chunk to R2 and advances `session.chunk_cursor`; viewers watch the cursor and
   fetch what appeared. One path for backfill and live viewing, no extra moving
   parts.
2. **Relay.** The app additionally streams bytes to a per-session Durable Object the
   moment they leave the PTY, and the DO fans them out over WebSocket. Faster, and a
   second transport to build, authenticate, and keep correct.

The plan expected the cost of cursor-follow to be "1–3 seconds" and treated the
decision as a trade between latency and complexity. Spike B built both behind one
interface and measured them.

## Decision

**Keep the relay as the live path, with cursor-follow as the durable path, joined
behind one source that the viewer cannot see through.**

Concretely, a session publishes twice over:

- **Frames** — sent the instant bytes leave the PTY, ephemeral, addressed by *byte
  range*, no retry. Their only job is to arrive quickly.
- **Chunks** — sent when the chunker closes one (~64 KiB or ~1 s), durable, addressed
  by *sequence*, written to R2 before the cursor advances.

The viewer backfills through the cursor, tails the relay, and falls back to
cursor-follow after a disconnect.

**The join splices by absolute byte offset, not by sequence number.** This is the
non-obvious half of the decision and it is load-bearing. Frames and chunks are not
two views of the same units — a frame is an arbitrary byte range, a chunk is a
sequence, and a frame routinely delivers half of a chunk that arrives whole a moment
later. Deduplicating by sequence cannot express that. Tracking one number,
`received_through`, makes the three cases fall out of a single comparison: entirely
behind it (drop), straddling it (take only the new tail), or starting beyond it (a
gap — refuse).

## Consequences

**Accepted.**

- **Two transports to keep correct**, which is the cost the plan was worried about.
  It is contained by the join: both paths enter the viewer through one function that
  enforces ordering, so neither can be wrong in a way the other hides.
- **The relay carries a per-session Durable Object**, whose ordering state must live
  in its SQLite rather than memory — a DO can be evicted between two frames, and an
  in-memory offset would make the next frame look like a gap, or worse be accepted at
  the wrong offset.
- **A WebSocket cannot be returned from a Durable Object RPC method** (it fails with
  `Web Socket request did not return status 101`), so the relay upgrade is a `fetch`
  handler while every other entry point is RPC. An inconsistency the platform forces,
  not a choice.
- **Frames are unauthenticated by anything better than a token today**, and the token
  rides in the WebSocket query string because browsers cannot set headers on a
  WebSocket. Spike-grade, and called out in the Worker's README as a Phase 1 blocker.

**Gained.**

- Measured p50 2 ms / p95 3 ms from "bytes left the PTY" to "a second process
  applied them", against cursor-follow's p95 of 1.3–1.6 s.
- Byte-exact reconstruction on both paths, verified against a local reference, and
  after recovering a spooled tail from a total Worker outage.
- The cursor keeps meaning "durably in R2" rather than "recently seen", because the
  relay never advances it.

## Alternatives rejected

**Cursor-follow alone.** Rejected on measurement. Its p95 is 1.3–1.6 s *on
loopback*, with no network latency at all, so it does not meet the sub-second target
even under conditions no real deployment will enjoy.

The decisive detail is *where* it is slow. Chatty output closes chunks on the size
threshold almost immediately, so cursor-follow looks nearly as fast as the relay —
p50 134 ms. Sparse output closes them on the age timer instead, and every byte waits
for it: p50 848 ms, p95 1,288 ms. Sparse output is not a corner case. **It is what a
session awaiting approval looks like** — the single highest-value thing the grid has
to surface promptly. Cursor-follow is slowest precisely when the product most needs
it to be fast, which turns a latency number into a product argument.

Tuning the flush interval down does not rescue it: the chunk is also the unit of
durability and of R2 write cost, so shortening it trades cost and write amplification
for a latency floor the relay does not have at all. The plan's own cost analysis
already flagged the time-triggered bound as the dominant term.

**Relay alone, without the durable path.** Never viable: there would be no backfill
for a viewer joining mid-session, no recovery after a disconnect, and no archive to
replay. The relay is explicitly ephemeral.

**Batching frames in the Worker rather than chunking in the app.** The plan's §3 read
this way. Rejected because `crates/ansible-capture` already produces chunks at those
parameters, and a second notion of chunk boundaries would break the byte offsets that
mention anchors depend on. The app chunks; the Worker persists what it receives.

## Revisit if

- Deployed measurements against a real edge change the picture. The numbers above are
  localhost and are a **floor**, not a prediction. The structural conclusion — relay
  wins, and wins most on sparse output — does not depend on the network, but the
  absolute figures must be re-measured after deploy rather than extrapolated.
- Durable Object cost at team scale turns out to dominate. Then the fallback is not
  "drop the relay" but "relay only while a viewer is actually present", which
  presence already tells us.
- Input handoff lands (open question #9). A bidirectional stream changes the relay's
  authentication and audit requirements substantially, and this ADR assumes read-only.
