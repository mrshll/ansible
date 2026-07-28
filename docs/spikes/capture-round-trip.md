# Spike B — transcript capture round trip

**Status:** partial. The local half is done and verified: `crates/ansible-capture`
exists, byte-exact reconstruction is a golden test, and the redaction ruleset is
derived from a recorded session rather than guessed. The deployed half — a real
Worker, real R2, a real Maincloud module, and end-to-end relay latency — is
**blocked on credentials and tooling not present in this environment** (see
[§6](#6-what-is-blocked-and-why)).

The plan calls capture correctness "the one thing in this system with no
acceptable failure mode." That part is finished and tested. What remains is
measurement that needs infrastructure.

---

## 1. What was built

```
crates/ansible-capture/           no terminal crate, no hub, no I/O, no clock
  src/chunk.rs                    the chunk envelope and its stored JSONL form
  src/chunker.rs                  (bytes, timestamps, config) -> chunks; Reassembler
  src/redact.rs                   streaming redaction across write boundaries
  examples/redact_report.rs       what the ruleset catches, and what it misses
  tests/golden_roundtrip.rs       byte-exactness, ordering, failure injection

crates/ansible-terminal/
  examples/vt_record.rs           record a real session's raw PTY bytes
```

The crate reads no clock and does no I/O: the caller supplies timestamps. That
is what makes the whole path a pure function of `(bytes, timestamps, config)`,
and therefore golden-testable and fuzzable — the strict boundary the plan asks
for.

### The chunk envelope

```jsonc
// line 1: header
{"session_id":"s-1","seq":0,"byte_start":0,"byte_end":5,
 "started_at_ms":1700000000000,"ended_at_ms":1700000000010,
 "redaction_version":2,"record_count":1}
// lines 2..n: one record each
{"at_delta_ms":0,"bytes":"aGVsbG8="}
```

Four decisions worth stating, because each one is load-bearing:

**Byte offsets index the redacted stream, not the raw one.** Redaction changes
length, so raw offsets would not survive a round trip. The redacted stream is
also the only one that gets stored, so it is the only one a viewer or a mention
anchor can address — and mention anchors are exactly `(chunk seq, byte offset)`.

**Record payloads are base64.** PTY output is arbitrary binary and frequently
not valid UTF-8, so it cannot be a JSON string. A `Vec<u8>` as a JSON array
would roughly triple the size.

**JSONL, not one JSON document.** A truncated upload loses only its last line,
and the Worker can append without reparsing. The header carries `record_count`
so truncation is *detected* rather than silently accepted as a short chunk.

**The envelope self-describes and validates.** `byte_end - byte_start` must
equal the payload length. A chunk whose declared range disagreed with its
contents would silently corrupt every downstream offset, so every chunk is
checked on both write and read.

### Ordering is enforced, not assumed

`Reassembler` refuses a chunk that is out of sequence, duplicated, or
non-contiguous. `accept_deduplicated` exists for the relay path, where frames
legitimately overlap a durable backfill: an already-seen chunk is skipped, but a
chunk from the future is still an error, because accepting it would leave a gap.

This is the plan's "never drop-and-continue — order is the one invariant, and a
visible stall is strictly better than a silent gap," made mechanical.

---

## 2. Redaction: measured, not assumed

### How the ruleset was derived

A real session was recorded with `vt-record`, then a second capture was taken of
the commands an agent actually runs that put credentials on screen — `env`,
reading a dotenv file, a `curl` header, `git remote -v`, an SSH key. All planted
credentials were synthetic.

The startup TUI of a real `claude` session contains **no** secret-shaped
material, which is worth knowing but not reassuring: credentials appear when the
agent *works*, not when it starts.

Running `redact-report` over the second capture gave the finding that shaped the
ruleset:

| Ruleset | Redactions | Secrets surviving |
|---|---|---|
| v1 — vendor prefixes only (`sk-ant-`, `ghp_`, `AKIA`, …) | 4 | **8** |
| v2 — with named values, URL credentials, JWT, PEM | **12** | **0** |

Vendor-prefix rules caught **4 of 12**. Every miss was one of four shapes:

- `SESSION_PASSWORD=…`, `MY_SERVICE_SECRET=…`, `API_TOKEN=…`, `PASSWORD=…`
- `postgres://admin:hunter2pass@db.internal:5432/prod`
- `Authorization: Bearer eyJhbGciOi…`
- `-----BEGIN OPENSSH PRIVATE KEY-----`

**A prefix-only ruleset would have shipped covering a third of the real leak
surface.** That is the single most useful thing this spike found, and it is a
direct answer to open question #4.

### Rule shapes in v2

| Shape | Behavior |
|---|---|
| `Token` | Vendor prefix + body run. Redacts the whole token. |
| `NamedValue` | `NAME=value` where `NAME` contains `token`, `secret`, `password`, `api_key`, `credential`, … Redacts **only the value**, so the transcript still shows which variable was set. |
| `UrlPassword` | `scheme://user:password@host`. Redacts only the password; scheme, user, and host survive. |
| `Jwt` | Three dot-separated base64url segments, ≥40 bytes. |
| `PemPrivateKey` | State machine, not lookahead. `PRIVATE KEY` blocks are swallowed; `CERTIFICATE` blocks are left alone because they are public and useful. |

Keeping the name and dropping the value is deliberate. A transcript that shows
`DATABASE_URL=[redacted:named-value]` is still useful for debugging; one that
drops the line is not, and drops no additional secret.

### Streaming, and why it is the hard part

PTY output arrives in arbitrary chunks, so a secret can straddle two writes:

```text
push(b"...key=sk-ant-api03-AAA")
push(b"BBBCCC and then more output")
```

A per-write scanner emits the first half verbatim and durably stores a live
credential. The redactor instead retains exactly the tail that could still grow
into a match. Tests cover the pathological case — one byte per write — and
assert that the result is identical for **every** split point of a stream
containing four different secret shapes.

Two properties keep this from costing latency:

- The hold-back is the forming match, never a fixed window. A write ending on a
  word boundary is released in full.
- A write ending mid-identifier *does* hold that identifier, because the next
  bytes could make it `SOME_SECRET=`. That is correctness, and it is bounded by
  the identifier cap (64 bytes) and `MAX_LOOKAHEAD` (512 bytes).

---

## 3. Bugs this found

Four, all fixed, and all of the kind that would have been very unpleasant later.

**A secret ending a stream was emitted verbatim.** A body running to the end of
the buffer was treated as "might still grow" even at end of stream, so a session
that ended right after printing a token stored it in the clear. Now end-of-stream
resolves a pending body into a match.

**A PEM block could swallow the rest of the session.** While discarding key
material, the code dropped the whole pending buffer — including a partially
received `-----END …`. The delimiter could then never match and every subsequent
byte was discarded. Now a bounded tail is retained across writes.

**An unconditional hold-back starved the chunker.** An early version held a fixed
window on every write, so small writes emitted nothing, chunks never reached
their size threshold, and the age-based flush had nothing to flush. Removed: the
partial-match break already covers straddling secrets.

**Redaction was slower than the terminal it must keep up with.** See below.

---

## 4. Measurements

Release build, Linux x86_64, 50 MiB of realistic session output.

### Redaction throughput

| | Throughput |
|---|---|
| Before optimization | 8 MiB/s |
| After optimization | **18 MiB/s** |

8 MiB/s was a problem, not a curiosity: Spike A measured the terminal sustaining
**10.3 MiB/s**, so redaction was the bottleneck and would have made the capture
path the reason a session felt slow. Throughput was flat across write sizes from
37 B to 64 KiB, which ruled out the harness and pointed at the scanner.

Two fixes, both mechanical once located:

- Reject a position by comparing one byte against each token rule's first byte,
  instead of running sixteen full scans.
- Stop allocating a lowercased `String` at every word start; compare
  case-insensitively in place.

18 MiB/s is comfortably above the terminal's output rate, with a further order
of magnitude available (a first-byte dispatch table, `memchr`) if it is ever
needed.

Reproduce:

```bash
cargo build --release -p ansible-capture --examples
REDACT_ONLY=1 WRITE_SIZE=65536 ./target/release/examples/redact-report big.raw
```

### Cost at team scale

From the chunk parameters (64 KiB or 1s, whichever first), for 10 engineers × 5
sessions/day × 30 minutes of moderately chatty output:

| | Value |
|---|---|
| Chunks per session | ~1,800 (time-triggered) to ~250 (size-triggered) |
| R2 writes/month | ~1.4M at the time-triggered bound |
| Cursor reducer calls | one per chunk, so the same order |
| Bytes per session | ~15 MiB raw, ~1–3 MiB compressed |

The time-triggered bound dominates, because an idle-but-open session still
flushes once a second. **That is the parameter most worth revisiting:** an
adaptive flush (1s while active, backing off when idle) would cut chunk count by
roughly an order of magnitude with no fidelity loss. Not implemented here —
it needs the deployed cost numbers to justify a specific curve.

---

## 5. Verification

`cargo test --workspace` — **133 passed, 0 failed.** Of those, 63 are new:

| Group | Count | Covers |
|---|---|---|
| Redaction | 25 | Each rule shape, every split point, one-byte-at-a-time, binary safety, false-positive avoidance |
| Chunk envelope | 9 | JSONL round trip, non-UTF-8 payloads, self-validation, truncation detection |
| Chunker / reassembly | 17 | Size and age flush, dense sequence, contiguous offsets, oversized writes, gap/duplicate/reorder rejection |
| Golden round trip | 12 | Byte-exactness through the stored format, independence from write and chunk boundaries, all 256 byte values, failure injection |

The golden test checks two things separately, because conflating them hides
bugs: **fidelity** (reassembly equals the redacted reference byte for byte) and
**containment** (no planted secret appears in any stored chunk, checked after
base64 decoding — a substring check on the encoded text would pass even if the
secret were stored).

Also clean: `cargo fmt --all -- --check`, and
`cargo clippy --workspace --all-targets -- -D warnings` with this crate
inheriting the workspace's `clippy::pedantic` and `unsafe_code = "forbid"`.

---

## 6. What is blocked, and why

Not attempted, because the environment has neither the tooling nor the
credentials:

| Blocked | Missing |
|---|---|
| Deployed Cloudflare Worker + R2 bucket | `wrangler`, Cloudflare account, R2 binding |
| Deployed SpacetimeDB Maincloud module | `spacetime` CLI, Maincloud credentials |
| End-to-end relay latency (p50/p95 PTY → second viewer) | both of the above |
| Relay-vs-cursor-follow decision (assumption A2) | both of the above |
| Failure injection against a *real* Worker | both of the above |
| Hook coverage → status machine (open question #2) | **nothing — now done, see below** |

The last row was never infrastructure-blocked, and it is now finished:
[hook-coverage.md](hook-coverage.md) records the measurements. Headline: four
statuses come out of hooks cleanly, but `AwaitingApproval` cannot — a denied tool
fires `PreToolUse` and never `PostToolUse`, which is indistinguishable from a
tool that is merely slow. It has to come from the terminal instead. That changes
four things in the schema; §4 of that document lists them.

What the local work does establish for the deployed half: the chunk protocol,
the ordering invariants the Worker must enforce (`seq` is the expected next,
`byte_start` is contiguous), and the exact object key layout
(`transcripts/{session_id}/{seq}.jsonl`). A Worker can be written against a
frozen protocol rather than co-designed with it.

---

## 7. Carried forward from Spike A

Spike A's raw-output tee is non-blocking and **drops** rather than stalls if a
consumer falls behind, exposing the loss as
`GhosttyTerminal::dropped_output_bytes()`. That was the right call for
rendering — a blocking tee froze the terminal — but it means:

> **Spike B's uploader must treat `dropped_output_bytes() != 0` as a hard
> failure.** A non-zero value means the transcript has a gap and is not
> byte-exact, and no amount of downstream ordering rigor can recover it.

`vt-record` already enforces this: it exits non-zero if the tee dropped
anything, so a reference capture can never be silently wrong. The production
uploader needs the same check plus the local spool the plan describes.

---

## 8. Open questions this moves

**#4 — who owns redaction, and what is the failure mode when a rule misses?**
Partly answered. Client-side redaction is demonstrably viable at 18 MiB/s, and
the miss mode is now measurable rather than theoretical: `redact-report` over a
recorded session names exactly what survived. The measurement also argues for a
second line of defense, because v1 would have shipped covering a third of the
surface, and the next unknown shape is by definition not in v2 either. A
Worker-side scan on the hot path is the natural place; that decision still needs
the deployed latency numbers.

Known gaps in v2, listed so they are not mistaken for coverage:

- High-entropy secrets with no prefix and no telling variable name.
- Secrets inside base64 or JSON that the terminal never renders as plain text.
- Credentials split across a *rendered* boundary, e.g. wrapped by the terminal
  mid-token. Redaction sees the byte stream, so a token broken by a line wrap
  with an escape sequence in the middle will not match.
- Anything needing more than `MAX_LOOKAHEAD` (512 B) of context.

**#5 — retention and deletion.** Unchanged, but sharpened: because chunks are
byte-exact and offsets index the redacted stream, rewriting a chunk to scrub it
breaks reassembly for anyone holding a later cursor. Scrubbing therefore has to
be a new-generation write, not an in-place edit. Worth settling before Phase 1.
