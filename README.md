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

Planning. No implementation yet.

- [Architecture plan](docs/plan/multiplayer-hub.md) — repo layout and module
  boundaries, SpacetimeDB schema and reducer surface, session lifecycle data
  flow, the two de-risking spikes, and the open questions gating Phase 1.

## Intended stack

| Layer | Choice |
|---|---|
| Desktop shell | Tauri (Rust core, TypeScript/React webview) |
| Embedded terminal | libghostty via libghostty-rs, behind a component boundary so xterm.js can be swapped in |
| Real-time coordination | SpacetimeDB (Maincloud) — members, sessions, presence, mentions. Hot state only, never transcripts |
| Transcript storage | Cloudflare R2, written through a Worker that also enforces access |
| Auth | GitHub OAuth; org membership is hub membership |
| Status events | Claude Code hooks, auto-configured by the app |
