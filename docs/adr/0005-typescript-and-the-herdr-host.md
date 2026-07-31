# ADR 0005 — TypeScript for everything we write

- **Status:** accepted (in progress)
- **Date:** 2026-07-31
- **Supersedes:** the language half of [ADR 0004](0004-herdr-plugin-host.md); its
  decision to host on Herdr stands
- **Evidence:** `services/hub/`, `plugins/herd/`, `scripts/check-ts.sh`

## Context

[ADR 0004](0004-herdr-plugin-host.md) moved the product from a Tauri desktop app to
a Herdr plugin, and built that plugin in Rust because the repo was already Rust.
That reasoning does not survive its own conclusion. Once Herdr owns the terminal
and the status detection, the Rust that was worth having was the Rust that talked
to a PTY — and none of it is in the path any more:

- `ansible-terminal` binds libghostty. Herdr owns the terminal.
- `ansible-hooks` derives status from Claude Code hooks. Herdr's `blocked` replaces
  it, for nineteen agents.
- `ansible-capture` is a redactor and a chunk format. Real, and small enough to
  port: ~200 lines of rules whose *value* is the twelve-credential recording that
  produced them, not the language they are written in.

What is left is a socket client, a poll loop, a text view, and a database schema.
Meanwhile the three runtimes this system actually targets all have first-class
TypeScript: `spacetimedb@2.7.1` ships TypeScript **server modules** at the same
2.7.x line as the Rust bindings; the transcript Worker was already TypeScript; and a
Herdr plugin is any argv command, so Node qualifies.

So the reusable asset is not the Rust — it is the **deployed infrastructure and its
architecture**: a SpacetimeDB database on Maincloud with a verified RLS posture
([ADR 0003](0003-read-authorization.md)), a Cloudflare Worker with R2 and a Durable
Object relay measured at ~3 ms p95 ([ADR 0002](0002-live-tail-transport.md)), and
the schema that has survived two designs.

## Decision

**TypeScript 7, with oxlint and oxfmt, for the plugin and all server logic. The
architecture is unchanged.**

1. `services/hub` is the SpacetimeDB module, ported from `services/hub-module`
   (Rust). Same tables, same reducer names, same RLS strings. **`spacetime publish`
   against the existing database is a schema-compatible republish, not a new
   database**, because `CASE_CONVERSION_POLICY` defaults to `SnakeCase` — so
   `sessionStatusHistory` is still `session_status_history` and the Worker's
   `advance_transcript_cursor` call keeps working untouched.
2. `plugins/herd` is the plugin. It reuses the schema's own concepts instead of
   inventing presence types: **watchers are `presence` rows** (connection-keyed, so
   `clientDisconnected` retires them and no lease has to expire), and **sharing is
   `visibility`** (`off`/`title`/`live` = absent/`Private`/`Org`), so "may watch"
   and "may read the transcript" are one authorization decision.
3. `scripts/check-ts.sh` is the TypeScript bar — oxfmt, oxlint at
   correctness/suspicious/perf/pedantic as errors, `tsc` per package, vitest — and
   `scripts/lint.sh` calls it, so there is still exactly one definition of a clean
   tree.
4. The Rust prototype stays in the tree beside it (`crates/ansible-herd`,
   `plugins/herdr-presence`, now under the plugin id `ansible.herd-rs`) until the
   TypeScript plugin has been run against a real Herdr server. Both can be linked;
   only one should run.

Three additions to the schema, all because presence now comes from Herdr:
`SessionStatus` gains `Unknown` (Herdr reports an agent it will not classify;
folding that into `Starting` would claim a lifecycle position we have no evidence
for), `StatusSource` gains `Herdr`, and there is a `help_request` table keyed by
identity — because a raised hand is a fact about a person, which is what the first
roster got wrong by printing the same note under three rows.

## Consequences

**Gained.** One language across the plugin, the module, and the Worker, so a schema
change is one edit and the same types describe both ends. Three runtimes that all
want TypeScript get it. The pieces worth keeping came across with their evidence
intact: the redactor's twelve planted credentials are now a vitest regression set,
and the roster's ordering rule — fresh before stale, asking before not, longest wait
first — is still a tested pure function.

**Given up.** The Rust type system on the presence path, and `unsafe_code =
"forbid"` as a property of the whole tree. The compensation is a deliberately sharp
`tsconfig.base.json` (`noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`,
`erasableSyntaxOnly`, `verbatimModuleSyntax`) and oxlint's pedantic set as errors —
with the exceptions recorded in `.oxlintrc.json`: `max-lines` off, because this
repo's convention is that the reasoning lives beside the code and a length cap is a
standing incentive to delete it; `max-lines-per-function` at 100 to mirror
`clippy::too_many_lines`; `require-await` off, because a Durable Object's
`webSocketMessage` must return a promise whether or not it awaits.

**Two upstream snags found, both worth knowing before anyone else hits them.**

1. **`spacetimedb@2.7.1`'s declarations are unusable under `nodenext` module
   resolution.** Its `.d.ts` files use extensionless relative imports, so
   `export * from '../lib/type_builders'` resolves to nothing and `t` — the type
   builder every table needs — is invisible, while the runtime exports it fine. The
   fix is `moduleResolution: "bundler"`, which is also semantically right: the
   module is bundled to wasm by `spacetime publish`, never resolved by Node.
2. **`t.enum()`'s declared return type omits the variant constructors it creates.**
   `SessionStatus.Working()` exists at runtime but not in the types. Enum values are
   tagged unit objects, so `reducers.ts` writes `{ tag: "Working" }` through a
   one-line helper and spells the unions out — which has the side benefit of putting
   the wire form in the source. Related: `exactOptionalPropertyTypes` is off for
   `services/hub` only, because the package's own declarations use `name?: string`
   where the checker then demands `string | undefined`.

**Not finished.** The plugin's I/O layer: the Herdr socket client, the SpacetimeDB
adapter, the reconcile daemon, and teleport. `plugins/herd` currently ships
`init`, `doctor`, `roster`, and `demo` against a file-backed world — enough to
exercise the ported core end to end and to keep the roster demonstrable — and says
so in `doctor`'s output rather than implying more. The Rust plugin remains the
working one until that lands.

**Cannot be verified here.** `spacetime publish` needs the CLI and credentials, so
the ported module is typechecked and linted but has not been published. Until it
has, the claim that it is a schema-compatible republish is an argument from the
naming policy, not an observation. `spacetime publish --project-path services/hub`
against a scratch database is the check, and it should happen before anything is
built on top.

## Alternatives considered

**Keep the Rust module and generate a TypeScript client.** `spacetime generate
--lang typescript` is the blessed path and would have left the deployed module
alone. It also leaves the repo bilingual, with the schema — the thing that changes
most while the design moves — in the language furthest from the plugin.

**Use the SpacetimeDB client SDK in the plugin rather than the CLI.** The SDK needs
generated bindings, which need the CLI, which is not available here. Driving
`spacetime call` / `spacetime sql` needs no codegen, works against the real deployed
module today, and mirrors how the plugin already shells out to `herdr` for
`terminal session observe`. The SDK is the upgrade once the bindings are generated
and committed; it is worth taking for the subscription, which is push rather than
poll.

**Deno or Bun instead of Node.** Both would remove the build step. Herdr's plugin
docs assume whatever is on the user's PATH, and Node is the one that certainly is.
