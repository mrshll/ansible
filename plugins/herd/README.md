# Herd — team presence for coding agents (TypeScript)

The Herdr plugin. See every agent session on your team in one ordered list, say
what you are working on, raise a hand, and — once the I/O layer lands — teleport
into a teammate's session.

```bash
npm install
npm run --workspace @ansible/herd build

export HERDR_PLUGIN_CONFIG_DIR=/tmp/herd-cfg HERDR_PLUGIN_STATE_DIR=/tmp/herd-state
node plugins/herd/dist/main.js init      # writes config.toml
node plugins/herd/dist/main.js demo      # a synthetic teammate
node plugins/herd/dist/main.js roster
node plugins/herd/dist/main.js doctor    # what is and is not working
```

## Status

**The pure core is ported and tested. The I/O layer is not finished.**

| | |
|---|---|
| `model.ts` | status mapping, share modes, normalization — done, tested |
| `redact.ts` | the redactor, with its twelve-credential regression set — done, tested |
| `roster.ts` | ordering and rendering — done, tested |
| `paths.ts` | config and the Herdr-injected directories — done |
| Herdr socket client | not yet |
| SpacetimeDB adapter | not yet |
| reconcile daemon | not yet |
| teleport | not yet |

`crates/ansible-herd` (Rust, plugin id `ansible.herd-rs` at
`plugins/herdr-presence`) is complete and working, and stays in the tree until this
one has been run against a real Herdr server. Both can be linked at once because the
ids differ — but do not run both daemons against the same hub as the same login, or
they will take turns overwriting each other's presence.

See [ADR 0005](../../docs/adr/0005-typescript-and-the-herdr-host.md) for why the
port happened and what it found, and
[docs/plan/herdr-plugin.md](../../docs/plan/herdr-plugin.md) for the design the code
implements.

## What leaves your machine

Unchanged from the Rust plugin, and enforced in the same places:

- **`off`** — the session does not appear in the herd.
- **`title`** (the default) — headline, status, repo and branch. No terminal
  contents, ever.
- **`live`** — the above plus a redacted live byte stream, and only while somebody
  is actually watching.

Sharing is the schema's `visibility`, so "a teammate may watch this" and "a teammate
may read the transcript" are one decision rather than two that can disagree. Watching
is a `presence` row keyed by connection, so closing the pane retires it — no lease
has to expire. And every published string goes through `redact.ts` first, including
terminal titles, because a title is written by whatever is running in the pane.
