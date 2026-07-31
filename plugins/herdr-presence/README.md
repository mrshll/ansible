# Herd — team presence for coding agents, as a Herdr plugin

See every agent session on your team in one ordered list, say what you are working
on, raise a hand when you are stuck, and teleport into a teammate's session to watch
it live and leave a comment.

[Herdr](https://herdr.dev) already knows which of *your* panes needs you. This adds
the other people.

```
  #  who          agent   state     for   what
  ─  ───          ─────   ─────     ───   ────
  1  Alice        claude  !blocked    4m  wire up read authorization
       ↳ help: RLS refuses to compare an enum to a literal
  2  Sam          claude  +done      15m  docs: hook coverage table
  3  Robin        claude  >working   35s  port the chunker to the relay [live] 👀2
```

## Try it in ten seconds, without installing anything

```bash
scripts/demo-herd.sh
```

Two members on one hub, the roster's ordering, a live teleport stream arriving byte
for byte, a comment crossing between them, and delivery on the other side. No Herdr
required — the demo runs with the server absent on purpose, to show that the hub half
and the Herdr half fail independently.

## Install

```bash
cargo build --release -p ansible-herd
herdr plugin link plugins/herdr-presence          # while developing
# or, once this is pushed:
herdr plugin install mrshll/ansible/plugins/herdr-presence

herdr plugin config-dir ansible.herd              # where config.toml goes
"$(herdr plugin config-dir ansible.herd)/../.."   # (state lives beside it)
```

Then write a config. `ansible-herd init` writes a starter with every knob
documented; the two decisions it leaves to you are your login and where the hub is.

```toml
login = "your-github-login"
display_name = "Sam"

[hub]
# "git" — presence over Git refs on a repo the team already has. No
#         infrastructure; push access to the repo is the authorization.
kind = "git"
remote = "origin"
repo = "/path/to/a/clone"

# "dir" — a directory every member can read and write. Sub-second, and the only
#         backend that carries live teleport frames today.
# kind = "dir"
# path = "/mnt/team/herd"

[share]
default = "title"       # headline and status only; no terminal contents
allow_submit = false    # a teammate's comment may not be submitted to your agent
```

Check it:

```bash
herdr plugin action list --plugin ansible.herd
"$(herdr plugin config-dir ansible.herd)/../../herd" doctor   # or: ansible-herd doctor
```

`doctor` reports every layer — identity, hub, Herdr socket, daemon, watch leases,
unread mail — rather than stopping at the first problem, because "why can't we see
each other" is the question it exists to answer.

## Keybindings

Add to your Herdr config:

```toml
[[keys.command]]
key = "prefix+h"
type = "plugin_action"
command = "ansible.herd.roster"
description = "show the herd"

[[keys.command]]
key = "prefix+?"
type = "plugin_action"
command = "ansible.herd.ask"
description = "ask the herd for help"

[[keys.command]]
key = "prefix+L"
type = "plugin_action"
command = "ansible.herd.share-live"
description = "share this pane live"
```

## Using it

**The roster** (`prefix+h`, or `ansible-herd roster`) is the one interactive surface:

| | |
|---|---|
| `<n>` | teleport into session *n* — opens a live view beside your work |
| `c <n> <text>` | comment on session *n* |
| `! <text>` | raise your hand with a note (empty lowers it) |
| `h <text>` | set your headline |
| `s <n> live\|title\|off` | change what one of *your* panes publishes |
| `i` | inbox |
| `a <n>` / `a <n> !` | type a comment into the pane / submit it to the agent |
| `d <n>` | dismiss a comment |
| `r` / `q` | refresh / quit |

Most of the time you type nothing. The headline falls back to Herdr's stripped
terminal title, which for Claude Code is a summary of the current task, and the
status is Herdr's own.

## What leaves your machine

Nothing until you install this, and then:

- **`off`** — the pane does not appear in the herd at all.
- **`title`** (the default) — headline, Herdr's status, repo and branch. No terminal
  contents, ever.
- **`live`** — the above plus a redacted live byte stream, and only while somebody
  is actually watching.

Three things hold that up:

1. **Everything published is redacted first** — headlines and comments as well as
   terminal bytes — through the same ruleset that caught 12 of 12 planted
   credentials in `docs/spikes/capture-round-trip.md`. A window title is written by
   whatever is running in the pane, and it goes to the whole team.
2. **Watching is visible and revocable.** A watcher shows up as a `$herd` token on
   your own Agent sidebar row. Someone asking to watch a `title` pane shows as
   `live = asked` — the request is the nudge, and nothing streams until you say yes.
   Dropping back to `title` kills the observation, not just the upload.
3. **A teammate cannot write to your agent.** `terminal session observe` is
   read-only by construction. A comment reaches your inbox; accepting it *types* it
   into your composer unsent, and you press Enter. Submitting it as a prompt needs
   both `a <n> !` and `allow_submit = true` in your config.

A plugin is ordinary code Herdr runs as your user, with no sandbox. That applies to
this one too: read `herdr-plugin.toml` and `herd` before you link it.

## Limits worth knowing

- **The `git` backend carries no live frames.** A commit per terminal chunk is not a
  stream. Teleport needs `kind = "dir"`, or the relay backend when it lands.
  Presence and comments work fine on `git`, at fetch-interval latency (~3–5 s).
- **The roster is line-driven, not a full-screen TUI.** This workspace forbids
  `unsafe` and raw mode means `termios`; `crossterm` is the upgrade and the ordering
  logic does not change.
- **A teleport view runs until you close its pane.** There is no "the session ended"
  a watcher can observe, so closing the pane is how you stop watching. The watch
  lease then expires within 15 s and the owner stops publishing.
- **This is written against Herdr's documentation, not a recording.** Every response
  parser probes field names with fallbacks and degrades rather than failing, and
  `scripts/capture-herdr-fixtures.sh` records the real shapes. If something looks
  blank that should not be, run it — the diff is the bug report. The five specific
  guesses are listed in [docs/plan/herdr-plugin.md](../../docs/plan/herdr-plugin.md).

## Why it looks like this

[docs/plan/herdr-plugin.md](../../docs/plan/herdr-plugin.md) — the design: ordering
as the product, the teleport handshake that is just two presence documents
converging, the consent ladder, and what to build next.

[docs/adr/0004-herdr-plugin-host.md](../../docs/adr/0004-herdr-plugin-host.md) — the
decision to make Herdr the host, and what that gives up.
