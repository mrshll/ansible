# transcript-worker

Ingests transcript chunks, relays them to viewers in real time, writes them to R2,
and advances the hub's cursor.

## Running it locally

```bash
npm install
npm run dev          # workerd on :8787, local R2, local Durable Object storage
```

Local mode uses simulated R2 and Durable Object storage on disk and **contacts
Cloudflare not at all**. That is how the whole round trip in
`docs/spikes/deployed-round-trip.md` was measured.

With it running:

```bash
scripts/probe-relay.sh          # byte-exactness, latency, failure injection
```

## Not deployed, deliberately

`wrangler deploy` would create a Worker, a Durable Object namespace, and an R2
bucket in whichever Cloudflare account is authenticated. The accounts on the
development machines here serve production traffic, so deploying is an explicit
decision rather than a default, and nothing in this repo does it automatically.

Before deploying:

1. Confirm the target account is one where creating `ansible-transcript-worker` and
   the `ansible-transcripts-spike` bucket is appropriate.
2. Set `HUB_TOKEN` as a **secret**, not a var:
   `npx wrangler secret put HUB_TOKEN`. It must be a bearer token for the identity
   registered as `hub_config.worker_identity` in the hub module — otherwise the
   Worker logs `hub is not configured` and the cursor never advances. That degrades
   freshness, not correctness: chunks still land in R2 and viewers still reconstruct
   byte-exactly.
3. Replace `PUBLISH_TOKEN` and `VIEW_TOKEN`, which are spike-grade shared secrets
   committed in `wrangler.jsonc`. See "Auth is spike-grade" below.

## The two streams

A session publishes twice over, and the distinction is the design:

| | Frames | Chunks |
|---|---|---|
| Sent when | bytes leave the PTY | a chunk closes (~64 KiB or ~1 s) |
| Addressed by | byte range | sequence number |
| Durable | no | yes, in R2 |
| Purpose | the relay feels live | the archive, backfill, and the cursor |

A viewer receives both and must not double-apply the overlap. It tracks one number —
the byte offset it has contiguously accepted — and splices every message by absolute
offset. That is what makes "backfill through the cursor, then tail the relay" safe;
deduplicating by sequence number could not work, because a frame is not a chunk.

`crates/ansible-capture` owns the chunk envelope and its golden round-trip test.
`src/protocol.ts` is a port of it. When the Rust changes, this changes with it, or
the archive stops being byte-exact.

## Ordering

The Durable Object refuses anything that would leave a gap, and says so rather than
papering over it:

| Condition | Response |
|---|---|
| Chunk is the next expected sequence, contiguous | 204 |
| Chunk sequence below the cursor (a retry that already landed) | 204, no-op |
| Chunk out of order | 409, naming the expected sequence |
| Chunk whose `byte_start` does not continue the archive | 409 |
| Body truncated (fewer records than the header declares) | 400 |
| Frame skipping ahead of the live stream | 409, and viewers get a `stall` message |

Order is the one invariant. A visible stall is strictly better than a silent gap.

Ordering state lives in the DO's SQLite, not memory: a DO can be evicted between two
frames, and an in-memory cursor would make the next frame look like a gap — or worse,
be accepted at the wrong offset.

R2 is written **before** the cursor advances, and the cursor is what every viewer
follows. Advancing first would publish an offset readers could race to and get a 404
from the archive.

## Auth is spike-grade

Matching the plan's "no auth beyond a hardcoded token" for Spike B. Two things must
change before Phase 1, both marked in the source:

- **The view token rides in the WebSocket query string**, because browsers cannot set
  headers on a WebSocket. It lands in logs. Phase 1 wants a short-lived ticket
  fetched over HTTP first.
- **A token cannot express "sharing was just turned off."** Phase 1 must check the
  hub for the session's current visibility on every read. Without that, un-sharing is
  recorded but not enforced.

## Routes

```
POST /v1/session/:id/chunk        publish a durable chunk (JSONL body)
POST /v1/session/:id/frame        publish an ephemeral relay frame
GET  /v1/session/:id/relay        viewer WebSocket
GET  /v1/session/:id/chunk/:seq   read one chunk from the archive
GET  /v1/session/:id/manifest     read the manifest, once finalized
POST /v1/session/:id/finalize     write the manifest
GET  /v1/session/:id/status       relay and cursor state
```
