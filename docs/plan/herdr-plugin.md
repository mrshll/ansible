# Presence as a Herdr plugin

**Status:** prototype, built and runnable. See
[ADR 0004](../adr/0004-herdr-plugin-host.md) for the decision this doc argues for,
and `plugins/herdr-presence/` for the thing itself.

```bash
scripts/demo-herd.sh        # the whole idea, one terminal, no Herdr needed
```

## The pivot, in one paragraph

[Herdr](https://herdr.dev) is an agent multiplexer: real terminal panes, per-pane
agent detection, state rolled up to tabs and workspaces, and it runs where the
agents run so you can close the laptop and ssh back in. That is most of what
`ansible` was going to build a desktop app for. What Herdr does not do is *other
people* — every pane it knows about is on one machine, and its sidebar answers
"which of my agents needs me" rather than "which of my team's". So the interesting
version of this project is not a second terminal; it is the team layer on top of
Herdr's. This document works out what that layer is, what it must not do, and where
the honest limits are.

## What Herdr already gives us, and what it costs to accept

The three hardest things in `docs/plan/multiplayer-hub.md` were the terminal, the
status, and the transport. Two of them go away.

| We were going to build | Herdr's answer | What we keep |
|---|---|---|
| Spike A: libghostty-vt in a Tauri window, our own renderer, 1.71 ms input-to-glyph | real panes, already there | the measurement, and nothing else |
| Hooks + a screen detector for `AwaitingApproval` (Spike B, W4) | `blocked`, from its own per-agent screen manifests | the lesson, not the code |
| PTY wrapping to capture bytes | `terminal session observe`: base64 ANSI frames, read-only, many observers | `ansible-capture` verbatim |
| A grid UI | the Agents sidebar, with plugin-supplied `$tokens` on each row | the team view, which is genuinely ours |

The one that stings is `AwaitingApproval`. `docs/spikes/approval-producer.md`
measured a real permission prompt reaching that status **1.3–3.6 ms** after it is
drawn, found that content alone was not enough — the detector fired on this
repository's own documentation until position was added — and found a real bug where
`Notification` arrived ~6 s late and demoted the status it was reporting. Herdr's
`blocked` is the same signal from the same kind of evidence, and it is maintained
against nineteen agents with remotely-updatable manifests. Keeping our own would
mean maintaining a second opinion about the same pane, which Herdr's own docs warn
against: *"each pane has one status authority… this avoids two competing sources of
truth."* So `crates/ansible-hooks` becomes a record of how the problem was
characterised, not a thing in the path. The characterisation is what has lasting
value; it is why we can read Herdr's `blocked` and know what it is claiming.

Accepting Herdr costs three things worth naming. Presence is only as good as its
detection, so a new agent screen shape shows as `idle` until Herdr learns it. The
plugin is ordinary code Herdr launches as your user with no sandbox, so trust is
per-install. And a plugin cannot draw anything but a terminal pane — no native UI in
plugin v1 — so the herd view is text.

## What the layer actually is

Four verbs. Everything else is plumbing for them.

1. **See the herd.** One ordered list of every agent session on the team.
2. **Say what you are doing.** A headline per machine, and a hand you can raise.
3. **Teleport.** Open a teammate's session and watch it live.
4. **Support.** Leave a comment against what you are looking at, which reaches
   the owner — and, if they let it, their agent.

### 1. Ordering is the product

A presence view that buries the one blocked session under nine idle ones has failed
at the only thing it exists to do. So the ordering rule is a tested pure function
(`roster::rows`), not a property of whatever loop draws the screen:

```
fresh before stale      →  a stale `blocked` row is a claim about the past
asking before not       →  a raised hand or blocked/done
blocked, done, working, idle, unknown
longest wait first      →  15 s blocked outranks 1 s blocked
```

The last one matters more than it looks. Without it the list reshuffles every time
anyone's status changes, and a queue that reorders under you is one you stop
reading.

Two smaller decisions in the same spirit. A member quiet past `stale_after_ms`
(20 s) is *marked*, not hidden — "Alice was blocked and her laptop went to sleep" is
information — and only dropped at `forget_after_ms` (5 min). And a raised hand
applies to every one of that person's sessions but prints its note once; the first
version printed the same sentence under three rows, which is how the note ended up
on the member document instead of on each card.

### 2. Status that is mostly free

`Status` mirrors Herdr's five states exactly. The headline — "what I'm working on" —
comes from the best available source:

1. what you typed (`herd status --headline`, or `h` in the roster);
2. Herdr's `terminal_title_stripped`, which for Claude Code is a short summary of
   the current task, with the animating spinner glyph already removed;
3. the workspace and tab.

Point 2 is the one that makes this feel free: most of the time nobody types
anything and the headline is still right. It is also why every published string goes
through `ansible-capture`'s `Redactor` — a window title is set by whatever is
running in the pane, `curl -H "Authorization: …"` as a title is a thing that
happens, and headlines go to the whole team. That redactor caught 12 of 12 planted
credentials at 18 MiB/s in `docs/spikes/capture-round-trip.md`; reusing it for an
80-character string costs microseconds and means there is one answer to "what
redacts published text".

A hand is raised with a note, because "blocked" is something Herdr already knows
and "I cannot get RLS to deny and I have been at it for twenty minutes" is the thing
that makes someone walk over. It gets a popup pane rather than an action, since an
action is a command with no way to ask a question.

**Presence goes back into Herdr's own UI.** `pane.report_metadata` tokens render as
`$name` in an Agent sidebar row, so the owner sees `2 watching` where they are
already looking, without a second window. This is display metadata only — the plugin
never reports semantic state, for the one-authority reason above.

### 3. Teleport, and the handshake that is just presence

`terminal session observe` is the whole reason this is cheap. It is read-only by
construction, supports many simultaneous observers, and emits base64 ANSI frames —
so the publisher is:

```
herdr terminal session observe → base64 decode → Redactor → Chunker → hub
```

and the viewer is its inverse. The middle is `ansible-capture` unchanged, which
means teleport inherits redaction-before-storage, dense sequence numbers,
contiguous byte ranges, and a refusal to splice over a gap. `dir` hub round trip is
byte-exact in `hub::dir::tests`; the viewer's ordering gate is separately tested
against duplicates, gaps, and mid-stream joins.

The handshake has no request/response at all, which is the part worth stealing.
Watching is *published intent*: the watcher puts a key in their own presence
document, the owner's daemon sees it, and the **owner's** share mode decides
whether frames follow. Two documents converging, no protocol. It gives three things
for free:

- **Asking is visible.** If the owner is on `title`, they get `live = asked` on
  their pane and a watcher count. The request itself is the nudge to share.
- **Revocation is real.** Dropping to `title` drops the `LivePublisher`, whose
  `Drop` kills the observe process. Sharing stops at the source, not at the upload.
- **Nobody watches invisibly.** The watcher list is on the owner's own row.

Watching is a **lease**, not a flag (15 s, refreshed every 3 s). A viewer pane is
stopped by closing it, which kills the process with no chance to clean up — so
without expiry the owner would see "somebody is watching" forever, and a live stream
would keep running for an audience of nobody.

### 4. Support, and the line we do not cross

`terminal session control` exists. A watcher could type directly into someone else's
agent. We deliberately do not.

A comment is a `Message` addressed to a key. It reaches the owner's inbox, raises a
toast, and lands as a `$note` token on the pane. Then there are three steps of
increasing consequence, and each is a separate deliberate act:

| | what happens | what it takes |
|---|---|---|
| read | you see it | nothing |
| accept | the text is **typed into the composer**, unsent | `a <n>` |
| submit | the text goes to the agent as a prompt | `a <n> !` **and** `allow_submit = true` in config |

The middle row is the interesting one. `pane.send_text` types without submitting, so
a teammate's words appear in your agent's input and *you* press Enter. The human
stays the one who submits. Injected text is attributed inline — `[from @alice] …` —
so it is legible in the transcript later. Submitting needs a flag *and* a config
edit, because "a remote human writes a prompt to your agent" should never become
true because a default changed.

## Where presence lives

Every path in the hub is keyed by the login of the only process allowed to write it.
No shared mutable state, no lock, no merge, no transaction — on a filesystem, in
Git, or on a relay. That single constraint is what lets one schema sit on three very
different transports.

| | to stand up | presence latency | live frames |
|---|---|---|---|
| **`dir`** — a directory everyone can read and write | nothing, if you already share a filesystem (NFS, SMB, Syncthing, Tailscale drive, one box) | sub-second | **yes** |
| **`git`** — refs on a repo you already have | nothing | fetch interval, ~3–5 s | no |
| *relay* — the Worker and Durable Object from Spike B | Cloudflare account decisions | ~3 ms | yes, not built |

**`git` is the answer to "connect with a GitHub team".** Each member publishes to
`refs/herd/<login>`: a parentless commit built through a temporary index, so no
history accumulates, nothing appears in branch listings or a PR, and your worktree
and index are untouched (asserted in `hub::git::tests`). Because refs are disjoint,
two people publishing at the same instant cannot conflict — the failure mode Git
usually forces you to think about does not arise. And **push access is the
authorization**: if GitHub lets you push, you are in the herd; if your access is
revoked, your presence stops being publishable the same minute. No API token, no
database, no service to run, and no separate answer to "who is on the team".

It cannot carry live frames — a commit per terminal chunk is not a stream — so
`put_chunk` refuses with an error that names the backend that works. That is the
honest split: `git` for the team, `dir` for teleport, relay when both matter at once.

## Shape of the code

```
plugins/herdr-presence/herdr-plugin.toml   the manifest: startup, 3 panes, 9 actions
plugins/herdr-presence/herd                argv wrapper, picks release or debug
crates/ansible-herd/src/
  herdr.rs      socket client — defensive parsing, and why
  hub/          the trait, `dir`, `git`
  model.rs      the two documents that cross the hub
  roster.rs     ordering and rendering, pure
  teleport.rs   observe → redact → chunk, and the viewer's gate
  daemon.rs     the reconcile loop
  state.rs      one writer per file, atomic writes, watch leases
  main.rs       one binary, one subcommand per manifest entrypoint
```

Two structural choices carry most of the weight.

**Reconcile from state; subscribe for speed.** Every tick reads the *whole* current
state from Herdr and rebuilds what should be published. Events are only a nudge to
run that tick sooner. It costs a snapshot per second on a local socket and buys a
daemon that cannot drift out of sync no matter which events it missed, and that
keeps working if the event payload shape changes underneath it. If
`events.subscribe` is refused outright, the daemon says so once and polls — the
difference is sub-second versus one-second, not working versus not.

**Every step of a tick is independent.** Herdr can go away while the hub is fine,
and the hub can go away while Herdr is fine. Either one short-circuiting the other
means a teammate's comment sits undelivered because a socket blinked. `demo-herd.sh`
demonstrates this without meaning to: it runs with no Herdr at all, the reconcile
and toast steps log failures, and the mail still lands.

## The uncomfortable part: this is written against documentation

`herdr.rs` and `teleport.rs` were written against herdr.dev, not against a running
server. That is a real difference from everything else in this repo, where the
convention is that fixtures come from recordings — and where recording paid off
loudly: `docs/spikes/hook-coverage.md` only found that a denied tool is
byte-for-byte indistinguishable from a slow one by recording real sessions, and the
redaction rules only reached 12 of 12 because a recording showed vendor-prefix rules
catching 4.

So the code is written to fail gracefully rather than to be right by luck:

- every field is probed with fallbacks and tolerates absence;
- `session.snapshot` falls back to `agent.list`; a degraded roster beats none;
- an unknown status becomes `unknown` instead of an error;
- the frame payload field is probed across five plausible names;
- test fixtures are labelled doc-derived, in the tests themselves.

`scripts/capture-herdr-fixtures.sh` is how that gets fixed: it records
`session.snapshot`, `agent.list`, `agent.explain`, the event stream, and 3 s of
`terminal session observe` from a real server, and says which files to compare. The
first person with Herdr installed should run it, and the diff is the review.

**How the guesswork gets closed:**

```bash
npm install && npm run --workspace @ansible/herd build   # so captures get scrubbed
scripts/probe-herdr.sh                                   # on a machine with Herdr
```

`scripts/probe-herdr.sh` turns every assumption into a numbered check and emits a
telemetry bundle: `report.md` for a human, `assumptions.jsonl` for a diff, and
`raw/` with the responses that should replace the doc-derived test fixtures. It is
safe against a working session — every write is one display-only token, one toast,
and one Agents-view sort, each undone — and it never sends input to an agent
without asking. Frame payloads are decoded and redacted rather than stored as
base64, because a text scrubber cannot see inside base64 and the whole point is a
bundle that is safe to hand over.

The two files worth reading first are `raw/field-audit.txt` — every field name the
parsers reach for, and which one was actually there — and `raw/frame-audit.txt`,
which says which field carries terminal bytes in an observe frame.

**Known-unverified, in the order they would hurt.** The check id in brackets is
where the probe reports on it.

1. **Whether `events.subscribe` accepts a subscription with no `pane_id` filter**
   [D1]. If not, the daemon polls — designed for, not fatal, but a second of
   latency on the status that matters most. The probe also tries the scoped form
   [D2], which is the fallback shape.
2. **The `terminal.frame` payload field name** [I2]. The code probes five
   plausible names; if none match, teleport publishes nothing and the fix is one
   line.
3. **`session.snapshot`'s field names** [B2]. Wrong names cost labels, not rows —
   the audit distinguishes a genuinely missing required field from an optional
   override that simply was not set.
4. **`min_herdr_version = "0.7.5"`** [K1]. Herdr refuses to link a plugin claiming
   a version newer than the binary, so this is a hard gate on a guess.
5. **Whether a `[[startup]]` hook's detached grandchild survives** [K5]. This one
   decides whether the daemon design works at all. If Herdr kills the process
   group, the daemon needs a different launch — a visible pane, or an OS service.

**What the probe cannot answer, and the commands for them.** These need eyes, a
restart, or credentials.

1. **Do reported tokens actually render in the Agents sidebar?** [E3] The docs say
   tokens "can be rendered as `$name` in Agent sidebar rows", which reads like the
   row template has to opt in. If they do not appear, the answer is a config
   change and the probe wants to know what it was.
   ```bash
   herdr pane report-metadata <pane> --source probe --token herd="2 watching"
   # then look at the sidebar row for <pane>
   ```
2. **Does a startup hook leave a daemon running?** [K5]
   ```bash
   herdr plugin link plugins/herdr-presence
   herdr kill && herdr            # restart the server so startup hooks fire
   pgrep -laf 'ansible-herd daemon' || echo 'no daemon survived'
   ```
3. **Does the popup pane size the way the manifest asks?**
   ```bash
   herdr plugin pane open --plugin ansible.herd-rs --entrypoint ask
   # expect a session-modal popup, 60% wide and 8 rows tall
   ```
4. **Does `plugin install` work from a GitHub subdirectory, with our build
   commands?** This is the path a teammate would actually use.
   ```bash
   herdr plugin install mrshll/ansible/plugins/herd --ref claude/herdr-plugin-presence-8r0ggq
   ```
5. **Does the ported SpacetimeDB module publish, and are its names snake_case?**
   [L1, L2] Use a scratch database, not the real one.
   ```bash
   spacetime publish --project-path services/hub herd-probe-scratch
   spacetime sql herd-probe-scratch "SELECT * FROM session_status_history LIMIT 1"
   spacetime sql herd-probe-scratch "SELECT session_id, shared_with_org FROM session LIMIT 1"
   ```
   If those two queries resolve under those names, the port is wire-compatible with
   the Rust module and the migration is a republish. If they do not, `CASE_CONVERSION_POLICY`
   is not doing what ADR 0005 claims and the RLS strings need rewriting.
6. **Is RLS still enforced after the port?** The existing probe already asserts
   this; point it at the scratch database.
   ```bash
   scripts/probe-rls.sh          # 6 assertions, from an identity that owns nothing
   ```

## What to build next, in order

1. **Run `capture-herdr-fixtures.sh`** and close the five items above. Everything
   else is speculation until this is done.
2. **A real full-screen roster.** `crossterm` or `ratatui`; the line-driven view is
   a placeholder that exists because `unsafe` is forbidden here and raw mode needs
   `termios`. The ordering function does not change.
3. **The relay backend.** `crates/ansible-transport` already publishes and
   reconstructs against a Durable Object at ~3 ms p95, against cursor-follow's
   1.3–1.6 s ([ADR 0002](../adr/0002-live-tail-transport.md)). It slots in behind
   `Hub` and makes teleport work between machines that share nothing.
4. **Anchored comments.** `Message::anchor_line` is carried and displayed but
   nothing anchors *properly* yet. The byte-offset anchoring in `ansible-capture`
   is the real answer — a comment pinned to a moment in the stream, not a line
   number in a screen that has since scrolled.
5. **Team membership from GitHub.** Right now the roster is whoever publishes.
   `get_teams`/`get_team_members` would let `doctor` say "bob is pushing presence
   but is not in the team", which is the check that turns push access into a real
   roster rather than a convention.
6. **`agent.view.set` driven by the herd.** Today `herd focus` installs a
   static attention-first sort. The interesting version filters *your* sidebar by
   what the *team* is blocked on.

## Open questions worth arguing about

- **Is the hub the right shape, or should presence be one document?** One document
  per member is what removes locking, but it means a reader assembles the world
  from N files and cannot subscribe to changes. A relay could push instead.
- **Should a comment be able to submit at all?** `allow_submit` is off by default
  and needs two deliberate steps. It might be right for it not to exist.
- **Is `blocked` enough?** Herdr's `blocked` covers approval, question, and
  permission UI. Our old status set distinguished `AwaitingApproval` from
  `AwaitingInput`, and a teammate can resolve the first but not the second. If that
  distinction turns out to matter, `agent.explain` reports the matched rule and
  could recover it — from Herdr's evidence rather than from a second detector.
- **What happens when two people watch the same session and both comment?** Today:
  two inbox items, in order. That is probably right, and definitely untested with
  real humans.
- **Does the daemon belong in a plugin at all?** A startup hook spawning a detached
  daemon is slightly against the grain of a host that describes its hooks as
  one-shot initializers. A visible pane, or an OS-level service, are the
  alternatives.
