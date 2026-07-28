# Phase 1 execution plan — from spikes to a dogfoodable MVP

A review of [multiplayer-hub.md](multiplayer-hub.md) against the code that now
exists, and the ordered next steps to finish the MVP.

**Where we are.** Both de-risking spikes have reported. Three pure Rust crates
are built and tested — terminal, capture, hooks — about 7,100 lines with 166
tests. Two of the three gating open questions are answered, one of them in a way
that changes the schema. ADR 0001 is accepted.

**What is left is not a spike.** Every networked component in the plan is
unstarted: `services/` and `packages/` do not exist, there is no SpacetimeDB
module, no Worker, no webview beyond a 34-line placeholder page, no identity,
no IPC surface, and no CI. The MVP is now an integration project with one
unanswered question (#3, identity and RLS) and one unbuilt platform (macOS).

**The single most important finding in this review** is in [§3](#3-one-plan-level-correction-chunksource-belongs-in-rust):
ADR 0001 moved the terminal out of the webview, which relocates `ChunkSource`,
the replay clock, and the archive fetch from TypeScript into Rust. The plan's §1
still places them in the webview. That is a one-paragraph correction now and a
rewrite of the viewer in week six.

---

## 1. What exists, measured against the plan

`cargo test -p ansible-capture -p ansible-hooks` passes 96 tests in a bare
container today. The terminal crate needs GTK3 development packages and a
from-source `libghostty-vt`, so `cargo test --workspace` **fails on a fresh
checkout** — see [W0](#w0--ci-and-a-reproducible-checkout).

| Plan surface | State | Where |
|---|---|---|
| `crates/ansible-terminal` | **done.** VT state, PTY, snapshot, input encoding, 16-test PTY matrix. Linux only in practice — the renderer is Cairo/Pango in the harness | `crates/ansible-terminal`, ADR 0001 |
| `crates/ansible-capture` | **done.** Chunker, five-shape redaction ruleset, `Reassembler`, byte-exact golden test | `crates/ansible-capture` |
| `crates/ansible-hooks` | **done.** Payload types, `StatusMachine`, fixture replay | `crates/ansible-hooks` |
| `TerminalSurface` boundary | **partly.** `TerminalBackend` + `Snapshot` are frozen; the `replay`-mode source does not exist | `src/backend.rs` |
| Tauri app | **harness only.** Linux GTK sibling surface, Cairo renderer, GDK input. No IPC commands, no session model, `bundle.active: false`, product id `…spike-a` | `apps/desktop/src-tauri` |
| React webview | **not started.** No `package.json` anywhere; one static placeholder page | `apps/desktop/src/index.html` |
| `packages/protocol` | **not started.** Chunk envelope exists in Rust only; no TS mirror, no manifest type, no deep-link format | — |
| `services/hub-module` | **not started.** No table, reducer, or RLS filter has been written | — |
| `services/transcript-worker` | **not started.** Object key layout and ordering invariants are frozen, so it can be written against a fixed target | capture-round-trip §6 |
| `services/slack-bridge` | **not started** | — |
| `crates/ansible-hub-client` | **not started** | — |
| Session supervisor, spool, uploader | **not started.** Capture is a pure function with no caller | — |
| Hook receiver | **not started.** Contract is frozen: one endpoint, dispatch on `hook_event_name`, route on `session_id` | hook-coverage §6 |
| `AwaitingApproval` producer | **not started.** `StatusMachine` requires a `TerminalHint`; nothing constructs one | hook-coverage §3 |
| Identity / keychain | **not started**, and the design is unresolved — open question #3 | — |
| CI | **none.** Four PRs merged with no automated check | — |

### Which open questions are actually closed

| # | Question | State |
|---|---|---|
| 1 | libghostty embeddable, under which compositing model | **closed.** ADR 0001: `libghostty-vt` + our renderer, native sibling surface |
| 2 | Do hooks yield a grid-quality status signal | **closed, and it changed the design.** Four statuses yes; `AwaitingApproval` must come from the terminal |
| 3 | GitHub OAuth → SpacetimeDB `Identity`; can RLS express visibility | **open, and it is now the only gating question.** Needs credentials → [W2](#w2--spike-c-identity-and-rls-the-last-gating-question) |
| 4 | Who owns redaction | **half-closed.** Client-side is viable at 18 MiB/s and its misses are measurable; whether a Worker-side second line is worth hot-path latency still needs deployed numbers |
| 5 | Retention and deletion | **open, sharpened.** Scrubbing must be a new-generation write, not an in-place edit, or reassembly breaks for anyone holding a later cursor |
| 6–10 | Adoption, second plane, multi-machine, viewer writes, Maincloud ops | **open by design.** Answerable during Phase 1 |

Assumption **A2** (relay-first live tail) is still untested — it was the part of
Spike B that infrastructure blocked. [§4](#4-sequencing-call-durable-before-relay)
recommends deliberately deferring it.

---

## 2. What "MVP complete" means

Six claims, each with a check a second person can run. The MVP is done when all
six pass on two machines belonging to two people.

| # | Claim | Passes when |
|---|---|---|
| 1 | Launch a Claude Code session in the app | Pick repo + branch in the app, a real `claude` runs in the embedded terminal, and it is as usable as a terminal |
| 2 | A live grid of everyone's sessions and status | B's grid shows A's session within a second of launch, and shows `Working` / `AwaitingInput` / `AwaitingApproval` / `Done` correctly through a real task with a real approval |
| 3 | See who is watching what | A sees B's avatar appear on the session when B opens it, and disappear when B closes the window |
| 4 | Open a teammate's session read-only, live | B sees A's output within the documented latency target, with no gaps, and can scroll back to the start of the session |
| 5 | @mention a teammate against a moment | A's OS notification fires with the app window closed; the deep link opens the viewer at the anchored offset |
| 6 | Consent holds | An unshared session exposes title, owner, and coarse lifecycle *only* — verified by inspecting what a second identity can actually subscribe to and fetch, not by looking at the UI |

Explicitly **not** in the MVP, so nobody plans around them: the
capture-anything daemon (A3 / #6), a second plane (#7), viewer write or input
handoff (#9), multi-machine grouping (#8), transcript search, live terminal
thumbnails in the grid ([§3](#3-one-plan-level-correction-chunksource-belongs-in-rust)),
and Windows.

---

## 3. One plan-level correction: `ChunkSource` belongs in Rust

The plan's §1 places the transcript viewer, `ChunkSource`, and the replay clock
in the React webview, and calls the teammate viewer "the *same component* in
`replay` mode." That was written when the terminal was expected to be a webview
component. ADR 0001 decided otherwise: the terminal is a native surface packed
beside the webview, driven from Rust, and **terminal bytes never enter the
webview**.

Both statements cannot hold. Keeping "one code path, two sources" — which is
what makes the teammate viewer nearly free — means the shared code path is
`ansible-terminal` plus our renderer *in the Rust core*, so the second source
has to live there too:

```
crates/
├─ ansible-terminal        unchanged
├─ ansible-capture         unchanged
├─ ansible-hooks           unchanged
├─ ansible-transcript      NEW — ChunkSource, replay clock, dedup, archive fetch
│                          CursorFollowSource · RelaySource · LiveChunkSource
└─ ansible-hub-client      as planned
apps/desktop/
├─ src-tauri/src/          + terminal/ session/ capture/ hooks/ hub/ identity/ commands/
│                          + replay/  — feeds a Snapshot from ChunkSource, not a PTY
└─ src/                    grid · session chrome · mention composer · settings · presence
                           NO transcript/, NO terminal/
```

Four consequences, all of them improvements:

- **The archive fetch is a Rust HTTP client**, so the Worker bearer token stays
  in the core next to the keychain and never reaches a webview context.
- **Replay is renderer-identical to live.** Feeding reassembled bytes into the
  same libghostty terminal means a teammate sees byte-identical rendering,
  including anything TUI-native. A TypeScript viewer would have needed a second
  renderer, which ADR 0001 rejected for exactly this reason.
- **Mention anchors need an IPC read.** The composer is in the webview but
  `(chunk seq, byte offset)` is core state, so `commands/` needs
  `current_anchor(session_id)`. Small, but it has to be designed in rather than
  discovered.
- **The grid shows status tiles, not live terminals.** One native surface is
  visible at a time, so a grid of live thumbnails is not available. This is a UI
  descope, and it is the right one — the grid's job is status and presence.

Two things that are *not* consequences, because they are the natural worries:
a session's terminal state machine runs whether or not its surface is drawn, so
**status detection does not depend on the session being on screen**; and many
sessions can run headless in one app while only one is rendered.

The plan's §1 tree and its `TerminalSurface` sketch should be amended to match.
Everything else in §1 — the four boundary rules, the two hub connections, the
`ChunkSource` seam itself — survives unchanged.

---

## 4. Sequencing call: durable before relay

Assumption A2 aims for a sub-second Durable Object relay while allowing Spike B
to select R2 cursor-follow instead. Spike B never got to measure either. The
recommendation is to **build cursor-follow first, ship the MVP on it, measure
it, and add the relay behind `ChunkSource` afterwards** ([W10](#w10--the-relay-and-the-a2-decision)).

Three reasons. Cursor-follow is the only path that must exist regardless —
backfill, reconnect, and replay-after-close all use it, and the relay is
strictly an accelerator on top. Its correctness is already proven locally by the
golden test, so it inherits the ordering rigor rather than needing its own.
And building the accelerator before the thing it accelerates means measuring a
latency improvement against nothing.

The cost is honest and bounded: viewers see a 1–3 second delay until W10 lands.
The plan already frames that as acceptable if documented rather than hidden.
The `LiveChunkSource` seam is what makes it a later addition instead of a
rewrite, and that seam is the deliverable of [W6](#w6--the-worker-and-the-durable-read-path).

---

## 5. Immediate next steps

Ordered by when to start, with dependencies. Sizes are one-engineer days and
deliberately rough. W0–W4 are the next two weeks; three of them need no
credentials and can start today.

### W0 — CI and a reproducible checkout
**1 day. No dependencies. Start now.**

Four PRs have merged with no automated check, and `cargo test --workspace`
fails on a fresh clone because GTK3 and `libghostty-vt` are absent. Both facts
get worse every week.

- `.github/workflows/ci.yml`: `cargo fmt --check`, `cargo clippy --all-targets
  -D warnings`, and tests for the pure crates on every push — these need no
  system dependencies and cover 96 tests today.
- A second job that installs GTK3/WebKitGTK, restores a cached `libghostty-vt`
  keyed on the pinned Ghostty commit, and runs the PTY matrix. Build from
  source only on a cache miss; `scripts/build-libghostty-vt.sh` takes ~5 min.
- A `SessionStart` hook so cloud sessions arrive with a buildable workspace,
  reusing `scripts/check-spike-a-prerequisites.sh` and `seed-zig-cache.sh`.
- Enforce the row-budget invariant in review, as the plan asks: nothing in
  SpacetimeDB may grow with transcript volume.

**Done when** a PR that breaks the capture golden test cannot merge, and a fresh
container can run the full workspace suite without manual setup.

### W1 — Provisioning
**Hours, but they are someone's hours, and five work packages wait on them.**

Not code, and the reason Spike B is half-finished. Needed: a Cloudflare account
with R2 and `wrangler`; a SpacetimeDB Maincloud account and the `spacetime`
CLI; a GitHub OAuth app for the org with a loopback redirect; and a Slack app
with `chat:write` for [W9](#w9--mentions-notifications-and-slack). One dev
machine with interactive `claude` auth is also needed for W4 — the container
this work has run in forces a permissive permission mode, which is why a real
approval prompt has never been observed.

**Done when** W2 and W6 can start.

### W2 — Spike C: identity and RLS, the last gating question
**2–3 days. Needs W1. Highest design risk remaining.**

Open question #3 is the only gating question still open, and it is a fork, not a
detail: it decides whether the Worker becomes a mandatory intermediary for
*every* write, and whether `Private` is enforceable or cosmetic.

The hypothesis to test: a small Worker endpoint verifies a GitHub token, checks
org membership, and mints a short-lived JWT whose issuer we control; Maincloud
is configured to trust that issuer's JWKS; `Identity` then derives
deterministically from `(issuer, subject)`, so `upsert_member_from_token()` can
read verified claims rather than client assertions. Confirm on Maincloud
specifically — not from documentation — and in the same spike write one
`#[client_visibility_filter]` that hides `session` from non-owners while leaving
`session_listing` org-visible, then verify it by subscribing as a second
identity and observing what actually arrives.

**Fork if the hypothesis fails:** all writes route through the Worker under a
single service identity, the webview's viewer connection becomes read-only,
presence moves to the agent connection, and the plan's "two connections, not
one" reasoning needs revisiting. Better to learn that in week one than week six.

**Done when** two real GitHub identities have distinct SpacetimeDB identities,
and a second identity provably cannot subscribe to a private session's detail
row.

### W3 — Protocol freeze, hub module, hub client
**4–6 days. Can start today; local `spacetime` needs no Maincloud.**

Apply the four schema consequences from hook-coverage §4 *before* generating
bindings, because they are free now and a migration later:

1. Document the terminal as the source of `AwaitingApproval`; the status machine
   is not hooks-only.
2. Keep `status_detail` an unstructured short string.
3. `close_session(exit_reason)` is called by the supervisor, never driven off
   `SessionEnd.reason` — it was `"other"` on a clean exit.
4. **Drop `Idle`.** Nothing can set it; derive it in the viewer from
   `last_event_at`. Carrying a status nothing produces is worse than not having
   it. (This is the one item here that is a decision, not a transcription — see
   [§6](#6-decisions-that-need-a-person-not-an-experiment).)

Then: `packages/protocol` with the chunk envelope, hook event, manifest, and
deep-link format — generated from the Rust types rather than hand-mirrored, so
TS and Rust cannot drift; `services/hub-module` with the nine tables, the
reducer surface, the RLS filters from W2, and both scheduled reducers; and
`crates/ansible-hub-client` wrapping it for the core.

**Done when** a local module accepts `register_session` →
`update_session_status` → `close_session` from an integration test, rejects a
non-monotonic cursor, and `reap_stale_sessions` flips a session to `Detached`.

### W4 — The `AwaitingApproval` producer
**2 days. Needs a machine with interactive `claude` auth (W1), not cloud infra.**

The grid *is* the product, and the plan calls approval "the interruption a
teammate can actually resolve." Hooks cannot supply it, the terminal can, and
today nothing constructs the `TerminalHint` that `StatusMachine` requires. This
is the highest-value unproven piece of the MVP.

Detect the prompt from `Snapshot` — the visible grid as text — and, critically,
detect when it **clears**, which is the half no hook can ever provide. Pair the
detection with `PreToolUse`'s `tool_name` for the detail string. While there,
answer the two questions hook-coverage §5 left open: does `Notification` fire on
a real permission prompt (a cheaper trigger, though it still cannot see the
prompt clear), and does `PermissionRequest` fire at all.

Record the screens as fixtures the way the hook payloads were recorded, so a
Claude Code TUI change is a reviewable diff and not a silently dead grid.

**Done when** a real approval prompt drives `AwaitingApproval` within a second,
answering it returns the session to `Working`, and a 30-second legitimate tool
call never trips it.

### W5 — The app skeleton: supervisor, spool, receiver
**5–8 days. Starts after W3's client exists; the local parts can start sooner.**

Turn the Spike A harness into the app: rename off `…spike-a`, add the
`commands/` IPC surface, and build `session/` (supervisor and status machine
host, one PTY per session, several sessions headless), `hooks/` (one localhost
receiver, per-session bearer token, session-scoped settings overlay), and
`capture/`.

One design change from the plan worth making deliberately: **spool first,
upload second.** The plan's §3 treats the local spool as a fallback for Worker
failure. Make it the primary sink — the tee's consumer is an append to a local
file, and the uploader reads from the spool. Spike A's tee drops rather than
stalls when a consumer falls behind, and capture-round-trip §7 requires the
uploader to treat `dropped_output_bytes() != 0` as a hard failure; a file append
is fast and bounded in a way a network client is not. The crash path the plan
wants — re-upload the tail on next launch and finalize late — then falls out of
the same mechanism instead of needing its own.

**Done when** launching a session registers it, publishes correct status through
a real task, spools byte-exactly, survives an app kill with a resumable spool,
and reports a nonzero drop count as a failure rather than a warning.

### W6 — The Worker and the durable read path
**5–7 days. Needs W1 and W3.**

`services/transcript-worker`: authenticated chunk PUT that enforces the frozen
invariants (`seq` is the expected next, `byte_start` is contiguous), R2 write at
`transcripts/{session_id}/{seq}.jsonl`, `advance_transcript_cursor` on success,
`finalize` writing `manifest.json`, and an authorized read that re-checks
visibility and either streams from R2 or issues a short-lived signed URL.
Alongside it, `crates/ansible-transcript` with `CursorFollowSource` and the
replay clock — the per-record timing deltas are already in the envelope, so
replay is time-accurate rather than dumped.

Then run the measurements Spike B could not: cursor-follow p50/p95, chunks and
bytes per session at team scale, R2 ops per month, and the failure injection
matrix — kill the Worker, the app, and the network mid-session, and verify no
gaps and no reordering after recovery.

**Done when** a second process reconstructs a real session byte-exactly from R2
through the deployed Worker, and survives all three kills.

### W7 — The webview
**6–8 days. Needs W3 bindings. Parallel with W5/W6.**

There is no JavaScript toolchain yet, so this starts at `package.json`: Vite,
React, TypeScript, generated SpacetimeDB bindings, the viewer-role connection.
Then the grid (status tiles grouped by person, `AwaitingApproval` loudest),
presence avatars, the launcher, the session-view chrome around the native
rectangle, the sharing toggle, and settings. Respect the ADR: no webview UI may
overlap the terminal region, so modals and panels avoid it or move native.

**Done when** two app instances show each other's sessions and presence live,
and the sharing toggle visibly gates what the other side can open.

### W8 — The replay viewer
**3–4 days. Needs W5, W6, W7.**

Wire `ChunkSource` into a replay-mode surface: backfill through the cursor, feed
reassembled bytes into a libghostty terminal, render with the same renderer as
live. Add `current_anchor` for the mention composer, "live tail stalled" state
when the cursor stops advancing, and scrollback to session start via the
manifest.

**Done when** claim 4 of [§2](#2-what-mvp-complete-means) passes, and closing
sharing mid-view closes the viewer's read path.

### W9 — Mentions, notifications, and Slack
**4–6 days. Needs W7, W8.**

`create_mention` with anchors, the agent connection's narrow subscription so OS
notifications fire with the window closed, the `ansible://session/{id}?at=…`
deep link plus a web fallback, `services/slack-bridge`, and
`mark_mention_read` / `mark_mention_delivered`.

**Done when** claim 5 passes on a machine where the app is not focused.

### W10 — The relay, and the A2 decision
**3–5 days. After W6's numbers exist.**

`RelaySource` over the session Durable Object, `LiveChunkSource` joining relay
and cursor-follow with sequence dedup, and the measurement that settles A2:
relay p95 versus cursor-follow p95, and the cost difference. Either adopt the
relay or record the measured cursor-follow delay and stop — explicitly, in a
document, rather than leaving a second unreliable transport behind the UI.

While the numbers are fresh, revisit the adaptive flush: capture-round-trip §4
found the 1-second time-triggered flush dominates chunk count because an idle
session still flushes every second, and estimated ~10× fewer R2 writes from
backing off when idle.

### W11 — macOS and packaging
**5–10 days, highest variance in this plan. See [§6](#6-decisions-that-need-a-person-not-an-experiment).**

The renderer, the surface, and the input path are Linux-only today, and no
macOS host has ever built them. Also unbuilt: keychain versus secret-service,
desktop notifications, deep-link registration, bundling (`bundle.active` is
`false`), signing, notarization, and update. Plus IME, untested on both
platforms and named in Spike A as the most likely source of unpleasant
surprises.

---

## 6. Decisions that need a person, not an experiment

**Which platform does the MVP dogfood on?** This is the biggest schedule
variable and it is not an engineering question. Options: Linux-first, where
everything except W11 is already proven and the team must run Linux to
participate; macOS-first, which front-loads W11 onto the critical path and asks
whether to reuse `libghostty-vt` with a CoreText renderer (one code path, worse
typography) or take the AppKit embedding API and accept two renderers; or both,
which is the plan's stated Phase 1 target and the slowest path to first
feedback. **Recommendation: pick the platform the team already uses and start
W11 in parallel with W3 rather than after W10.** If that is macOS, W11 moves up
and Spike A's §6 list becomes immediate work. If the answer is "we should find
out whether people will even launch sessions in the app first" (#6), a
Linux-first internal trial is the cheapest way to learn it.

**Question #4 — the redaction second line.** v1 would have shipped covering a
third of the real leak surface, and the next unknown shape is by definition not
in v2 either. Decide after W6 whether a Worker-side scan is worth hot-path
latency, with the known v2 gaps in front of you: high-entropy secrets with no
prefix, secrets inside base64 or JSON, tokens broken by a terminal line wrap,
and anything needing more than 512 bytes of context.

**Question #5 — retention and deletion.** Needed before W6 finalizes the object
layout. How long do transcripts live, can an author delete one, and what happens
when an agent touches customer data? Chunks are byte-exact and offsets index the
redacted stream, so a scrub must be a new-generation write; whether the cursor
may ever move backward follows from this answer.

**Drop `Idle`?** Recommended in W3. Nothing can set it, the viewer can derive it
from `last_event_at`, and it is free to remove before bindings are generated.

---

## 7. What would make me re-plan

| Trigger | Effect |
|---|---|
| W2's identity hypothesis fails on Maincloud | Worker intermediates all writes; the two-connection design and the webview's write path both change. Re-plan W3, W5, W7 |
| W4 cannot detect an approval prompt reliably from `Snapshot` | The grid loses its highest-value signal. Fall back to surfacing pending-tool *age* honestly rather than guessing a status, and say so in the UI |
| W6's failure injection finds a gap against a real Worker | The chunk protocol needs rework before anything is built on it — the plan's own kill criterion. Stop and fix; everything downstream inherits it |
| Cursor-follow p95 is much worse than 3s | Pull W10 forward ahead of W7 |
| macOS typography with our renderer is judged unacceptable | Take the AppKit path and accept two renderers (ADR 0001's first "revisit if") |
| The `libghostty-vt` pin needs to move | Bindgen turns it into a compile error, not undefined behavior. Budget a day; do not do it mid-integration |
