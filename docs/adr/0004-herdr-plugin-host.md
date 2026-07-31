# ADR 0004 — Herdr as the host, and this project as its team layer

- **Status:** proposed (prototype built)
- **Date:** 2026-07-31
- **Deciders:** the herd prototype
- **Evidence:** [docs/plan/herdr-plugin.md](../plan/herdr-plugin.md),
  `plugins/herdr-presence/`, `crates/ansible-herd/`, `scripts/demo-herd.sh`

## Context

Both de-risking spikes are complete and product code has not started, so this is
the last cheap moment to change what we are building.

The plan in [docs/plan/multiplayer-hub.md](../plan/multiplayer-hub.md) has this
project owning the whole stack: a Tauri desktop app, an embedded terminal, a status
producer, a coordination database, a transcript store, and a team UI.
[Herdr](https://herdr.dev) — an agent multiplexer with mouse-first panes, per-agent
state detection, ssh-friendly persistence, and a documented socket and plugin API —
now covers the bottom four layers, and covers them for nineteen coding agents rather
than one.

Three of our own results are what make the overlap concrete rather than superficial:

- **Spike A** ([ADR 0001](0001-terminal-composition-model.md)) established that
  Ghostty's GPU embedding API is macOS/iOS-only, so a cross-platform terminal means
  driving `libghostty-vt` and writing our own renderer. Measured p50 input-to-glyph
  1.71 ms, 10.3 MiB/s sustained. It works, and it is a permanent maintenance
  obligation for something Herdr already ships.
- **Hook coverage** and the **`AwaitingApproval` producer**
  ([docs/spikes/hook-coverage.md](../spikes/hook-coverage.md),
  [docs/spikes/approval-producer.md](../spikes/approval-producer.md)) established
  that the single highest-value status cannot come from hooks — a denied tool is
  byte-for-byte indistinguishable from a slow one — and that reading it off the
  screen needs six co-occurring signals including *position*, because content alone
  fired on this repository's own documentation. Herdr publishes `blocked` from the
  same class of evidence, per agent, from remotely-updatable manifests.
- **Spike B** ([ADR 0002](0002-live-tail-transport.md)) built a capture path that is
  a pure function of `(bytes, timestamps, config)`, redacts before a byte can reach
  a chunk, and is byte-exact through the stored form. Nothing in Herdr replaces it —
  and Herdr's `terminal session observe` hands us exactly the input it wants,
  without a PTY of our own.

What Herdr does not do is other people. Every pane it knows about is on one machine;
its sidebar answers "which of my agents needs me", never "which of my team's".

## Decision

**Herdr is the host. This project is the team layer, delivered as a Herdr plugin.**

1. Herdr owns terminals, panes, agent detection, semantic status, persistence, and
   remote attach. We do not reimplement or second-guess any of them. In particular
   the plugin reports **display metadata only** and never semantic agent state, so
   a pane keeps exactly one status authority.
2. `ansible` owns what Herdr does not have: a team hub, cross-machine presence,
   ordering by who needs a human, teleport into a teammate's session, and an
   addressed comment channel with a consent model.
3. `ansible-capture` stays, unchanged, in the teleport path. It is the piece with no
   acceptable failure mode and it is already golden-tested.
4. `ansible-hooks` and `ansible-terminal` stay in the tree as **evidence, not
   dependencies**. Their measurements are why we can read Herdr's `blocked` and know
   what it is asserting; neither is in the presence path.
5. Presence rides on a `Hub` trait with a `dir` backend (a shared directory:
   sub-second, carries live frames) and a `git` backend (refs on a repo the team
   already has: no infrastructure, GitHub push access *is* the authorization, no
   live frames). The Spike B relay slots in behind the same trait later.
6. Every path in the hub has exactly one writer, keyed by the owner's login. No
   locks, no merges, no transactions, on any backend.

## Consequences

**Gained.** The two hardest layers stop being ours. The status that took a spike to
characterise arrives for free, for nineteen agents. Teleport needs no PTY wrapping,
no process supervision, and no terminal embedding — `terminal session observe` is
read-only by construction and supports many observers. "Runs where the agents run,
close the laptop, ssh back in" is Herdr's problem now. And there is a working
prototype today rather than a desktop app later:
`scripts/demo-herd.sh` shows presence, ordering, a raised hand, live teleport, a
comment, and delivery in one terminal with nothing installed.

**Given up.** A native UI: plugin v1 has no non-terminal surface, so the herd view
is text in a pane. Control of the status contract: a new agent screen shape shows as
`idle` until Herdr learns it, and `blocked` does not distinguish "awaiting approval"
from "awaiting input" the way our own set did — a teammate can resolve the first but
not the second. Independence: our floor is now `min_herdr_version` and whatever the
protocol does next.

**Newly required.** Every published string — headlines, comments, terminal bytes —
goes through the redactor, because a window title is written by whatever runs in the
pane. Sharing defaults to `title` (headline and status only); no terminal contents
leave a machine until a pane is explicitly set to `live`, and revoking that kills
the observe process rather than merely stopping the upload. A teammate's comment can
be *typed* into a pane unsent with one keystroke, but can only be *submitted* to the
agent with a second flag plus a config edit.

**Not yet verified.** The socket client is written against documentation rather than
a recording, which is a real departure from this repo's convention — the hook work
found four of twelve redaction assumptions wrong precisely by recording. Every
parser therefore probes field names with fallbacks and degrades rather than failing,
and `scripts/capture-herdr-fixtures.sh` exists to replace prose with observation.
Until it has been run against a real server, five things are guesses, listed in the
plan doc; the load-bearing one is `min_herdr_version = "0.7.5"`, because Herdr
refuses to link a plugin claiming a version newer than the running binary.

## Alternatives considered

**Continue with the desktop app.** Spike A proves it works. It also means owning a
VT renderer, a status detector, and a persistence story permanently, to arrive at
something whose single-machine half is worse than Herdr's while its team half — the
actually novel part — has not been started. The presence layer is the product; the
terminal was always a means.

**A Herdr integration rather than a plugin.** Integrations are per-agent hook and
session-identity installers; they report state for one agent. Presence is not about
one agent, and the plugin API is where panes, actions, keybindings, and the socket
all meet.

**Keep our own `AwaitingApproval` detector alongside Herdr's `blocked`.** Rejected
on Herdr's own reasoning: two authorities for one pane is two truths. If the
approval/input distinction turns out to matter, `agent.explain` reports the matched
rule and can recover it from Herdr's evidence rather than from a competing detector.

**SpacetimeDB (the existing hub module) as the presence transport.** It is built and
published, and RLS is genuinely enforced on Maincloud ([ADR
0003](0003-read-authorization.md)). It also needs a wasm client, a Maincloud
deployment, and an identity story before two people can see each other. `git` needs
a repo they already have. For a prototype whose whole purpose is to be argued with,
the transport with nothing to stand up wins; the trait keeps the other one available.
