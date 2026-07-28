<!--
Delete any section that does not apply. The checklists below are not ceremony:
each item is an invariant that a spike or the architecture plan established, and
that nothing in CI can check for you.
-->

## What and why

<!-- What changed, and which work package or open question it moves. Link the
plan section or spike finding it follows from. -->

## How it was verified

<!-- Commands run and what they proved. If a measurement changed, give the
before and after. -->

---

## Invariants

Tick what applies; strike out what does not.

**If this touches the SpacetimeDB module** — the row budget is a hard invariant,
not a guideline ([plan §0](../docs/plan/multiplayer-hub.md#0-assumptions-this-plan-rests-on)):

- [ ] Nothing added grows with transcript volume. Row budget stays O(sessions) +
      O(status transitions), both bounded
- [ ] `session_status_history` still writes on transition only, never per event
- [ ] `advance_transcript_cursor` is still Worker-only and strictly monotonic —
      the cursor means "durably in R2", not "a client said so"
- [ ] A private session still exposes `session_listing` only, and its coarse
      status reveals lifecycle only — not that an agent is awaiting approval
- [ ] Visibility changes were verified by subscribing as a second identity, not
      by looking at the UI

**If this touches the capture path** — capture correctness has no acceptable
failure mode ([capture-round-trip](../docs/spikes/capture-round-trip.md)):

- [ ] The golden round trip still passes: reassembly is byte-exact and no
      planted secret appears in a stored chunk after base64 decoding
- [ ] Ordering is still refused rather than repaired — no gap is spliced over
- [ ] `ansible-capture` still does no I/O and reads no clock
- [ ] A nonzero `dropped_output_bytes()` is still a hard failure, not a warning
- [ ] If redaction changed, `redact-report` was re-run and its coverage and
      known gaps are recorded

**If this touches status:**

- [ ] `AwaitingApproval` still comes from a terminal hint and never from a timer
      ([hook-coverage §3](../docs/spikes/hook-coverage.md#3-the-finding-awaitingapproval-is-not-in-the-hooks))
- [ ] Unknown hook events and unknown payload fields are still ignored rather
      than fatal, so a Claude Code upgrade cannot break the receiver
- [ ] `Failed` still comes from the supervisor's exit status, not
      `SessionEnd.reason`

**If this touches the terminal or the session view:**

- [ ] No webview UI overlaps the terminal rectangle
      ([ADR 0001](../docs/adr/0001-terminal-composition-model.md))
- [ ] Terminal bytes still do not enter the webview
- [ ] If the libghostty pin moved, it moved deliberately and the PTY matrix
      passes

## Checks

- [ ] `cargo test --workspace`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
