# ansible

A multiplayer presence layer for Claude Code — an internal desktop app for
observing and collaborating on AI coding agent sessions across the team.

Launch Claude Code in an embedded terminal, see a live grid of everyone's
sessions and their status, see who's watching what, open any teammate's session
as a read-only live transcript, and @mention someone against a specific moment
in a session. Sessions are the first "plane"; the presence and mention layer is
built to be reused for others.

Agent sessions run locally in the user's own PTY under their own Claude
credentials. The app never calls a model API.

Phase 1 targets macOS and Linux. Sessions publish a title-only presence card by
default; transcript sharing is an explicit owner-controlled toggle. Shared
sessions aim for sub-second live viewing, with durable transcript chunks stored
outside the real-time coordination database.

## Status

De-risking is done; integration has not started. Both spikes have reported, and
the three pure crates below are built and tested. Everything networked — the
SpacetimeDB module, the Worker, identity, and the webview — is unstarted, and
one gating question (GitHub OAuth → SpacetimeDB `Identity`, and whether RLS can
express the visibility rules) is still open.

- [Architecture plan](docs/plan/multiplayer-hub.md) — repo layout and module
  boundaries, SpacetimeDB schema and reducer surface, session lifecycle data
  flow, the two de-risking spikes, and the open questions gating Phase 1.
- [Phase 1 execution plan](docs/plan/phase-1-execution.md) — what the spikes
  left, what "MVP complete" means, and the ordered work packages to get there.
  Amends the architecture plan in one place: ADR 0001 moved the terminal out of
  the webview, so `ChunkSource` and the replay clock move into Rust with it.
- [W1 — provisioning runbook](docs/plan/w1-provisioning.md) — the accounts,
  buckets, and tokens the MVP needs, each with a command that proves it works.
- [Spike A — terminal embedding](docs/spikes/terminal-embedding.md) — **done.**
  libghostty runs a real Claude Code session inside a Tauri window on Linux.
- [Spike B — transcript capture round trip](docs/spikes/capture-round-trip.md) —
  **partial.** The capture path is built and byte-exactness is a golden test;
  the deployed Worker, R2, and Maincloud measurements are still outstanding.
- [Spike B — hook coverage](docs/spikes/hook-coverage.md) — **done.** Hooks can
  drive the grid, except for the one status that matters most.
- [ADR 0001 — terminal composition model](docs/adr/0001-terminal-composition-model.md)

## Development

One script installs everything the workspace needs — GTK3, WebKitGTK, libclang,
and `libghostty-vt` built from its pinned Ghostty revision:

```bash
scripts/setup-dev-env.sh          # ~5 min cold, seconds warm; idempotent
cargo test --workspace
```

The pure crates need none of that, so `cargo test -p ansible-capture -p
ansible-hooks` works on a bare checkout. CI splits along the same line: a
one-minute job for the pure crates and a slower one that builds the native
backend and the harness against a cached `libghostty-vt`. Cloud sessions run the
same script from a `SessionStart` hook, because a container without it fails at
`cargo build` on a missing `gdk-3.0` and quietly drops the terminal crate's 16
PTY tests.

### Spike A in one paragraph

Ghostty's GUI embedding API accepts only an AppKit `NSView` or a UIKit
`UIView`, so its GPU renderer cannot be reached from a Linux host. Its
cross-platform `libghostty-vt` library — VT parsing, terminal state, render
state, input encoders — can, so the terminal is libghostty state drawn by our
own renderer into a native GTK surface packed beside the webview. Measured p50
input-to-glyph latency 1.71 ms; 10.3 MiB/s sustained output with zero dropped
bytes. xterm.js is not needed.

```bash
scripts/check-spike-a-prerequisites.sh              # check the machine
scripts/build-libghostty-vt.sh                      # build libghostty-vt (~5 min)
scripts/run-spike-a.sh claude                       # the harness, running Claude Code
cargo run -p ansible-terminal --example vt-fixture  # no display server needed
cargo test --workspace
```

### Spike B in one paragraph

`crates/ansible-capture` turns raw PTY bytes into redacted, ordered chunks as a
pure function of `(bytes, timestamps, config)` — no I/O, no clock — so the path
with no acceptable failure mode is golden-testable. Reassembly is byte-exact
through the stored JSONL form, independent of how the PTY split its writes, and
refuses a gap rather than splicing over one. Redaction was derived by recording a
real session: vendor-prefix rules alone caught only 4 of 12 planted credentials,
so named values, URL credentials, JWTs, and PEM blocks became rules too, taking
coverage to 12 of 12 at 18 MiB/s. Deployed Worker, R2, and relay-latency
measurements remain blocked on credentials.

```bash
cargo test -p ansible-capture                                   # 63 tests
cargo run -p ansible-terminal --example vt-record -- out.raw claude
cargo run -p ansible-capture --example redact-report -- out.raw  # coverage + gaps
```

### Hook coverage in one paragraph

`crates/ansible-hooks` parses real Claude Code hook payloads and derives the grid
status from them. Recorded from live sessions, so the types and the state machine
are grounded in observation: `Starting`, `Working`, `AwaitingInput`, and `Done`
come out of hooks cleanly. `AwaitingApproval` does not — a denied tool fires
`PreToolUse` and never `PostToolUse`, which is byte-for-byte indistinguishable
from a tool that is merely slow (measured at 9.2 s for a legitimate command). So
that status is taken from a terminal hint instead of guessed from a timer, and the
API makes that a requirement rather than a convention.

```bash
cargo test -p ansible-hooks                    # 33 tests, incl. fixture replay
scripts/capture-hook-payloads.sh               # re-record the fixtures
```

## Intended stack

| Layer | Choice |
|---|---|
| Desktop shell | Tauri (Rust core, TypeScript/React webview) |
| Embedded terminal | libghostty-vt for terminal state, rendered into a native surface beside the webview ([ADR 0001](docs/adr/0001-terminal-composition-model.md)); the component boundary keeps xterm.js swappable |
| Real-time coordination | SpacetimeDB (Maincloud) — members, sessions, presence, mentions. Hot state only, never transcripts |
| Transcript storage | Cloudflare R2, written through a Worker that also enforces access |
| Auth | GitHub OAuth; org membership is hub membership |
| Status events | Claude Code hooks, auto-configured by the app |
