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

Planning and de-risking. **Both spikes are complete.** There is now also a working
prototype of the presence layer built a different way — as a plugin for
[Herdr](https://herdr.dev), an agent multiplexer that already owns the panes, the
agent detection, and the remote-attach story this app was going to build. See
[ADR 0004](docs/adr/0004-herdr-plugin-host.md); the desktop-app plan below is not
retired, but the plugin is the cheaper way to find out whether the *team* half is
any good.

```bash
scripts/demo-herd.sh      # presence, teleport, and a comment, in one terminal
```

- [Architecture plan](docs/plan/multiplayer-hub.md) — repo layout and module
  boundaries, SpacetimeDB schema and reducer surface, session lifecycle data
  flow, the two de-risking spikes, and the open questions gating Phase 1.
- [Spike A — terminal embedding](docs/spikes/terminal-embedding.md) — **done.**
  libghostty runs a real Claude Code session inside a Tauri window on Linux.
- [Spike B — transcript capture round trip](docs/spikes/capture-round-trip.md) —
  **done.** The capture path is built and byte-exactness is a golden test.
- [Spike B — hook coverage](docs/spikes/hook-coverage.md) — **done.** Hooks can
  drive the grid, except for the one status that matters most.
- [W4 — the `AwaitingApproval` producer](docs/spikes/approval-producer.md) —
  **done.** The one status hooks cannot give us now has a producer: a real
  permission prompt reaches the grid 1.3–3.6 ms after it is drawn.
- [Spike B — the deployed half](docs/spikes/deployed-round-trip.md) — **done.**
  Hub module on Maincloud, Worker with R2 and a Durable Object relay, and a
  second process reconstructing a live session byte for byte. Decides
  assumption A2 and answers the RLS half of open question #3.
- [ADR 0001 — terminal composition model](docs/adr/0001-terminal-composition-model.md)
- [ADR 0002 — live-tail transport](docs/adr/0002-live-tail-transport.md) — keep the
  relay; cursor-follow is the durable path, and the join splices by byte offset.
- [ADR 0003 — read authorization](docs/adr/0003-read-authorization.md) — RLS is
  enforced and is the mechanism; no filtering intermediary.
- [Herd — presence as a Herdr plugin](docs/plan/herdr-plugin.md) — **prototype
  built.** The team layer on top of Herdr instead of on top of our own terminal:
  what Herdr replaces, what it costs, the teleport handshake, and the consent ladder.
- [ADR 0004 — Herdr as the host](docs/adr/0004-herdr-plugin-host.md) — Herdr owns
  the panes and the status; this project owns the team.
- [ADR 0005 — TypeScript for everything we write](docs/adr/0005-typescript-and-the-herdr-host.md)
  — **in progress.** The plugin and the SpacetimeDB module in TypeScript 7 with
  oxlint and oxfmt; the deployed infrastructure and its architecture are what get
  reused, not the Rust.

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
coverage to 12 of 12 at 18 MiB/s.

```bash
cargo test -p ansible-capture                                   # 63 tests
cargo run -p ansible-terminal --example vt-record -- out.raw claude
cargo run -p ansible-capture --example redact-report -- out.raw  # coverage + gaps
```

### The deployed half in one paragraph

`services/hub-module` is published to SpacetimeDB Maincloud and
`services/transcript-worker` runs on local `workerd` with a Durable Object relay and
R2; `crates/ansible-transport` publishes a real PTY session into them and a second
process reconstructs it byte for byte. Row-level security **is** enforced on
Maincloud — the 2.7.0 bindings say it is not, and that comment is stale — though it
cannot compare an enum column to a literal, which is why `session` carries a
`shared_with_org` boolean beside its `visibility` enum. Cursor-follow's p95 is
1.3–1.6 s on loopback against the relay's 3 ms, and it is slowest on sparse output,
which is exactly what a session awaiting approval looks like, so the relay stays
(assumption A2). The Worker is deliberately not deployed: that creates Cloudflare
resources and wants an explicit account decision.

```bash
scripts/probe-rls.sh                                     # 6 assertions: read visibility
scripts/probe-hub.sh                                     # 17: cursor, row budget, ownership
cd services/transcript-worker && npm install && npm run dev &
scripts/probe-relay.sh                                   # 11: byte-exactness, latency, recovery
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
cargo test -p ansible-hooks                    # 62 tests, incl. fixture replay
scripts/capture-hook-payloads.sh               # re-record the fixtures
```

### The `AwaitingApproval` producer in one paragraph

`ansible_hooks::approval` recognises a real permission prompt in rendered screen
text, which is the half of the status signal no hook has. Measured against real
interactive `claude`: a prompt reaches `AwaitingApproval` **1.3–3.6 ms** after it
is drawn, answering it returns to `Working` in **21–62 ms**, and a tool holding
its bracket open for 15.1 s never trips it. Detection needs six signals to
co-occur — five of content plus, crucially, the modal's footer being the *last*
thing on screen. Content alone is not enough: the review of this change found the
detector firing on this repository's own documentation, since a fenced example
contains the whole block. Position is what separates a live modal from a faithful
description of one. Under-reporting degrades to an honest "working for 2m", while
over-reporting teaches people to ignore the one status meant to summon them. Running both halves together also found a real bug:
`Notification` fires ~6 s after the prompt with "Claude needs your permission" and
was demoting the very status it reported. `PermissionRequest` fires too, once per
prompt rather than once per tool — a genuine rising edge, though still no event
reports a prompt being *dismissed*, which is why the terminal stays the producer.

```bash
scripts/probe-approval.sh                      # 18 assertions against a real session
```

### The Herdr plugin in one paragraph

`crates/ansible-herd` is a Herdr plugin — one binary, several manifest entrypoints —
that publishes each machine's agent sessions to a team hub and renders the team as
one ordered list. Herdr supplies the panes and the semantic status, so the
`AwaitingApproval` problem two spikes went into arrives as its `blocked` for free,
and `herdr terminal session observe` hands us base64 ANSI frames that go straight
through `ansible-capture`'s redactor and chunker — teleport with no PTY of our own.
Presence rides on a hub trait with two backends that need nothing stood up: a shared
directory (sub-second, carries live frames) and **Git refs on a repo the team already
has**, where `refs/herd/<login>` is disjoint per member so publishing cannot
conflict, and push access *is* the authorization. Sharing defaults to headline-only;
a teammate's comment reaches your inbox and can be *typed* into your composer unsent,
but reaching your agent as a prompt needs a flag and a config edit. The socket client
is written against Herdr's documentation rather than a recording — a real departure
from this repo's convention — so every parser probes field names and degrades, and
`scripts/capture-herdr-fixtures.sh` is how that gets fixed.

```bash
scripts/demo-herd.sh                           # the whole idea, no Herdr needed
cargo test -p ansible-herd                     # 111 tests, incl. real-git round trips
herdr plugin link plugins/herdr-presence       # with Herdr installed
scripts/probe-herdr.sh                         # validate every assumption, emit telemetry
scripts/capture-herdr-fixtures.sh              # record the real socket shapes
```

## Development

Two toolchains, one bar. `scripts/lint.sh` is the definition of a clean tree and it
calls `scripts/check-ts.sh` for the TypeScript half, so there is no way for the two
to disagree or for CI to differ from a local run.

```bash
npm install                # TypeScript 7, oxlint, oxfmt, vitest
scripts/check-ts.sh        # oxfmt --check, oxlint, tsc per package, vitest
scripts/check-ts.sh --fix  # apply what can be applied
```

Three TypeScript packages, three runtimes, and therefore three module-resolution
modes — which is why `tsc` runs per package rather than once over a solution:
`services/hub` is bundled to wasm by `spacetime publish`,
`services/transcript-worker` is bundled by wrangler, and `plugins/herd` is run by
Node as argv commands.

One-time setup per clone, which points git at the versioned hooks in
`.githooks/`:

```bash
scripts/install-hooks.sh
```

After that, `scripts/lint.sh` is the definition of a clean tree, and the
pre-commit hook and CI both run exactly it — there is one bar and no way for
local and CI to disagree.

```bash
scripts/lint.sh          # rustfmt --check, then clippy over every target
scripts/lint.sh --fix    # apply the mechanical fixes, then re-check
git commit --no-verify   # bypass the hook once (CI still runs it)
```

Every clippy warning is an error in `lint.sh` while staying a warning during a
normal `cargo build`, so iteration stays quiet without letting a warning reach
`main`. The lint set itself lives in the `[lints]` tables in `Cargo.toml`:
`clippy::all` and `clippy::pedantic` everywhere, plus an explicit soundness set
— the `cast_*` lints, `ptr_as_ptr`, `undocumented_unsafe_blocks`, and
`unsafe_op_in_unsafe_fn`. `unsafe_code` is `forbid` outside the two crates that
bind C, and those two carry `undocumented_unsafe_blocks` to hold every `unsafe`
block to a stated invariant. `rust-toolchain.toml` pins the toolchain, because
`-D warnings` against a moving rustfmt and clippy is a version lottery.

Two things worth knowing about the hook. It lints the whole workspace rather
than just the staged files, because rustfmt and clippy are not per-file tools —
so a partial `git add` can pass the hook on the strength of unstaged work, and CI
is the real backstop. And `cargo fmt` only reaches files the module tree
declares, so a new `.rs` file no `mod` mentions is invisible to both tools until
it is wired in.

## Intended stack

| Layer | Choice |
|---|---|
| Desktop shell | Tauri (Rust core, TypeScript/React webview) |
| Embedded terminal | libghostty-vt for terminal state, rendered into a native surface beside the webview ([ADR 0001](docs/adr/0001-terminal-composition-model.md)); the component boundary keeps xterm.js swappable |
| Real-time coordination | SpacetimeDB (Maincloud) — members, sessions, presence, mentions. Hot state only, never transcripts |
| Transcript storage | Cloudflare R2, written through a Worker that also enforces access |
| Auth | GitHub OAuth; org membership is hub membership |
| Status events | Claude Code hooks, auto-configured by the app |

The Herdr plugin ([ADR 0004](docs/adr/0004-herdr-plugin-host.md)) substitutes the
first three rows and the last: Herdr owns the shell, the terminal, and the status,
and presence rides on Git refs or a shared directory rather than SpacetimeDB. The
transcript path is unchanged — `ansible-capture` is the same crate in both designs.
