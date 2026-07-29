# AGENTS.md

Orientation for coding agents working in this repo. `README.md` is the human
entry point and stays authoritative on what has been built and measured; this
file is the operating manual — how to build, what the bar is, and which
boundaries are load-bearing.

## What this is

`ansible` is a multiplayer presence layer for Claude Code: an internal desktop
app for observing and collaborating on agent sessions across a team. Agent
sessions run locally in the user's own PTY under their own Claude credentials —
**the app never calls a model API**, and nothing here should introduce one.

Status: planning and de-risking. Both spikes are complete; product code has not
started. That means most of what exists is spike code plus the docs that justify
it, and the docs are the design of record:

- `docs/plan/multiplayer-hub.md` — architecture, schema, module boundaries, open
  questions gating Phase 1.
- `docs/adr/000{1,2,3}-*.md` — decisions taken (terminal composition, live-tail
  transport, read authorization). If a change contradicts an ADR, write or amend
  an ADR rather than quietly diverging.
- `docs/spikes/*.md` — what was measured, with numbers. Prefer citing these over
  re-deriving a claim.

## Layout

| Path | What |
|---|---|
| `crates/ansible-capture` | PTY bytes → redacted, ordered chunks. Pure: no I/O, no clock. Golden-tested. |
| `crates/ansible-hooks` | Claude Code hook payload types + status derivation. Pure; fixtures are real recordings. |
| `crates/ansible-terminal` | libghostty-vt backed terminal: PTY lifecycle, terminal state, input encoding, snapshots. Binds C. |
| `crates/ansible-transport` | Publishes chunks/frames to the Worker and reassembles them on the far side. All the I/O lives here. |
| `apps/desktop/src-tauri` | Package name `ansible-spike-a` — the Spike A harness (GTK surface beside a Tauri webview), not the product app yet. |
| `services/hub-module` | SpacetimeDB module: schema + reducers. wasm32 cdylib, **excluded from the Cargo workspace**. |
| `services/transcript-worker` | Cloudflare Worker (TS): ingest, Durable Object relay, R2, cursor advance. |
| `scripts/` | Every build/probe entrypoint. Each script's header comment explains why it exists. |

## Commands

```bash
scripts/install-hooks.sh                 # once per clone: point git at .githooks/
scripts/lint.sh                          # THE definition of a clean tree
scripts/lint.sh --fix                    # apply mechanical fixes, then re-check
cargo test --workspace                   # works without the native library
scripts/build-libghostty-vt.sh           # ~5 min Zig build into vendor/ (gitignored)
scripts/run-spike-a.sh claude            # the harness with a real session (needs a display)
cargo run -p ansible-terminal --example vt-fixture   # headless terminal render
cargo run -p ansible-capture --example redact-report -- out.raw
```

`scripts/lint.sh` is what the pre-commit hook runs and what CI runs — one
entrypoint, so local and CI cannot drift. Run it before every commit; do not
hand-roll `cargo fmt`/`cargo clippy` invocations as a substitute, because
`lint.sh` also covers the two things `--workspace` cannot see (`hub-module` on
wasm32, and the Worker's `tsc --noEmit`).

`git commit --no-verify` exists but CI still runs the same script, so it only
defers the failure.

## The lint bar

- `clippy::all` + `clippy::pedantic` everywhere, plus an explicit soundness set:
  the four `cast_*` lints, `ptr_as_ptr`, `undocumented_unsafe_blocks`,
  `unsafe_op_in_unsafe_fn = "deny"`.
- `unsafe_code = "forbid"` except in the two crates that bind C or GTK
  (`ansible-terminal`, `ansible-spike-a`), which set it to `allow` and rely on
  `undocumented_unsafe_blocks` to hold every `unsafe` block to a stated SAFETY
  invariant.
- Warnings are errors only inside `lint.sh` (`-D warnings`), so ordinary
  `cargo build` stays quiet.
- **Cargo has no partial `[lints]` inheritance, so the same block is spelled out
  in four manifests**: root `Cargo.toml`, `crates/ansible-terminal/Cargo.toml`,
  `apps/desktop/src-tauri/Cargo.toml`, `services/hub-module/Cargo.toml`. Change
  one, change all four.
- Fix a `cast_*` warning with an explicit conversion (`try_into`, `From`, or a
  documented clamp) — never a wider `as`, never an `#[allow]` without a comment
  saying why the lint is wrong here.
- `rust-toolchain.toml` pins `1.97.1`. Don't bump it incidentally; a bump changes
  rustfmt output and clippy's lint set, so it is its own commit with `lint.sh`
  run.

## Build gotchas

- **libghostty-vt is optional at build time.** `crates/ansible-terminal/build.rs`
  looks for `vendor/libghostty-vt` (or `LIBGHOSTTY_VT_DIR`) and sets
  `cfg(have_libghostty_vt)` when it finds it. Without it, only the contract,
  event, and snapshot types compile — `cargo test --workspace` still passes, and
  `ansible-spike-a` cannot run. If you touch code under `#[cfg(have_libghostty_vt)]`,
  build the vendored library first or your change is unlinted and untested.
- **bindgen needs libclang with Clang's builtin headers.** `lint.sh` and
  `run-spike-a.sh` both call `scripts/detect-libclang.sh`; on Linux install
  `libclang-dev`, not `libclang1`. On macOS, leaving `LIBCLANG_PATH` unset is the
  working path.
- Linux native deps are the list in `.github/workflows/ci.yml` and
  `scripts/check-spike-a-prerequisites.sh`: `libgtk-3-dev`,
  `libwebkit2gtk-4.1-dev`, `libcairo2-dev`, `libpango1.0-dev`, `libclang-dev`.
  GTK3 and WebKitGTK **4.1** specifically — Tauri v2 links against those, and
  mixing in gtk4 puts two incompatible GObject worlds in one process.
- `cargo fmt` and clippy only reach files the module tree declares. A new `.rs`
  file that no `mod` mentions is invisible to both until it is wired in.
- Cargo example targets use hyphenated names with underscored paths
  (`vt-fixture` → `examples/vt_fixture.rs`), declared explicitly as
  `[[example]]`. Add both halves for a new example.
- `services/hub-module` needs the `wasm32-unknown-unknown` target and is built by
  `spacetime publish`, not the host toolchain. It has its own `Cargo.lock`.
- The Worker's `tsc --noEmit` step is *skipped* rather than failed when
  `node_modules` is absent, so a Rust-only checkout lints clean. If you change
  TypeScript, run `npm install` in `services/transcript-worker` so the check
  actually executes.

## Boundaries that must hold

These four carry the architectural weight (`docs/plan/multiplayer-hub.md` §1);
everything else is organization.

1. **`ansible-terminal` depends on neither Tauri nor the hub.** It must build and
   run standalone. That is what keeps the xterm.js fallback a configuration
   change rather than a rewrite.
2. **`ansible-capture` depends on nothing and does no I/O.** Pure function of
   `(bytes, timestamps, config)`. Every network call, retry, and spool write goes
   in `ansible-transport`. Capture correctness has no acceptable failure mode, so
   it gets the strictest boundary — new dependencies there need a real argument.
3. **Order is never inferred and a gap is never spliced over.** Reassembly
   refuses a gap rather than guessing; a visible stall beats a transcript that is
   quietly wrong. Redaction happens before a byte can reach a chunk, so byte
   offsets index the redacted stream.
4. **The live-tail join splices by absolute byte offset, not by sequence.**
   Frames (arbitrary byte ranges, ephemeral) and chunks (sequences, durable) are
   not two views of the same units, so deduplicating by sequence is wrong. See
   ADR 0002.

Two more worth knowing: the hub holds hot coordination state only and never
transcripts; and `AwaitingApproval` cannot be derived from hooks (a denied tool
is byte-for-byte indistinguishable from a slow one), so it comes from a terminal
hint and the API makes that a requirement rather than a convention.

## Things not to do without asking

- **Do not run `wrangler deploy`** or otherwise create Cloudflare resources. Not
  deploying is a deliberate decision — see `services/transcript-worker/README.md`.
- **Do not publish to SpacetimeDB Maincloud** as a side effect of another change.
- Don't commit anything under `vendor/`, `target/`, `node_modules/`, or
  `apps/desktop/src-tauri/gen/` — all gitignored, all generated.
- Don't run the `probe-*.sh` scripts expecting them to work offline: `probe-rls.sh`
  and `probe-hub.sh` need the `spacetime` CLI and a deployed module,
  `probe-relay.sh` needs `npm run dev` running, and
  `capture-hook-payloads.sh` needs `claude` on PATH with working credentials.
- Don't weaken a lint, a golden test, or an ADR to make a change fit. Those are
  the parts of this repo that were expensive to establish.

## Writing style

Match what's here. Comments and doc headers explain *why* — the constraint, the
measurement, or the failed alternative — not what the next line does. Several
comments exist specifically to record that something surprising is true (RLS
*is* enforced on Maincloud despite the stale binding comment; the frame/chunk
dedup correction). Keep that habit: if you discover a fact that a future reader
would otherwise get wrong, write it down next to the code that depends on it.
