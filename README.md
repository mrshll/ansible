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

## Status

Planning, plus one runnable spike.

- [Architecture plan](docs/plan/multiplayer-hub.md) — repo layout and module
  boundaries, SpacetimeDB schema and reducer surface, session lifecycle data
  flow, the two de-risking spikes, and the open questions gating Phase 1.
- [Spike A — terminal embedding](docs/spikes/terminal-embedding.md) — **done.**
  libghostty runs a real Claude Code session inside a Tauri window on Linux.
- [ADR 0001 — terminal composition model](docs/adr/0001-terminal-composition-model.md)

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

## Intended stack

| Layer | Choice |
|---|---|
| Desktop shell | Tauri (Rust core, TypeScript/React webview) |
| Embedded terminal | libghostty-vt for terminal state, rendered into a native surface beside the webview ([ADR 0001](docs/adr/0001-terminal-composition-model.md)); the component boundary keeps xterm.js swappable |
| Real-time coordination | SpacetimeDB (Maincloud) — members, sessions, presence, mentions. Hot state only, never transcripts |
| Transcript storage | Cloudflare R2, written through a Worker that also enforces access |
| Auth | GitHub OAuth; org membership is hub membership |
| Status events | Claude Code hooks, auto-configured by the app |
