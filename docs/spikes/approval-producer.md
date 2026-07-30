# W4 — the `AwaitingApproval` producer

**Status:** done, and it fixes a bug it went looking for something else to find.

**Question asked:** the grid's highest-value signal has no producer.
[hook-coverage.md](hook-coverage.md) established that `AwaitingApproval` cannot
come from hooks and must come from the terminal, and `StatusMachine` was built to
require a `TerminalHint` for it — but **nothing constructed one**. Can a real
permission prompt be recognised on the rendered screen, reliably enough to drive
the grid?

**Answer: yes, and quickly.** A real prompt reaches `AwaitingApproval` **1.3–3.6
ms** after it is drawn, answering it returns to `Working` in **22–62 ms**, and a
tool holding its bracket open for 15.1 s never trips it. The recognition is a
pure function of screen text, so the whole thing is replayable from recordings.

Everything below was measured against real interactive `claude` v2.1.220 on a
normal Linux dev machine — the first time this project has had one. The container
the earlier spikes ran in forces a permissive permission mode, which is why a
genuine prompt had never been observed
([hook-coverage §5](hook-coverage.md#5-environment-limits)).

`scripts/probe-approval.sh` reproduces all of it. **18 assertions, 0 failures.**

Headline findings, in descending order of how much they change the plan:

1. **A `Notification` was demoting the status it was reporting.** On a real
   prompt, `Notification` fires with the message `"Claude needs your permission"`
   — about **six seconds after** the prompt is drawn. `StatusMachine` treated any
   notification as `AwaitingInput`, so the one event that means "a human is being
   asked" moved the grid *off* `AwaitingApproval`. A real bug, found only because
   the probe ran both halves at once, and fixed here. See [§5](#5-the-bug-this-found).
2. **`PermissionRequest` fires, and it is a real rising edge.** Absent from the
   `--print` recordings, it fires here once per *prompt* and not once per tool. It
   carries `tool_name`, `tool_input`, and `permission_suggestions`. This answers
   the second of hook-coverage §5's two open questions and it is genuinely new
   information — but it does **not** make this a hooks-only signal, because
   nothing reports the prompt being *dismissed*. See [§6](#6-what-this-changes-about-the-hook-story).
3. **`PreToolUse` lands ~30 ms *before* the prompt is drawn.** So "a bracket
   opened" is not a proxy for "no approval was needed", and any code that waits on
   one to conclude the other reads the gap as an answer. This cost the probe two
   runs before it was understood, and it is a trap the app would have hit too.
4. **The folder-trust gate is a near-perfect look-alike** — same `❯` marker, same
   numbering, same `Esc to cancel` footer — and it appears *before any tool runs*.
   Reading it as an approval would put every fresh session on the grid as
   `AwaitingApproval`. It is now a checked-in fixture asserting it does not.

---

## 1. What was built

```
crates/ansible-hooks/src/approval.rs            the detector: screen text -> TerminalHint
crates/ansible-hooks/tests/screen_replay.rs     6 tests over recorded screens
crates/ansible-hooks/tests/fixtures/screens/    5 verbatim screen recordings
crates/ansible-hooks/tests/fixtures/approval-session.jsonl   the hook log beside them
crates/ansible-terminal/examples/approval_probe.rs           the live prover/recorder
scripts/probe-approval.sh                       18 assertions against a real session
```

The detector lives in `ansible-hooks`, next to the `TerminalHint` it produces and
the `StatusMachine` that consumes it, and it takes **screen text rather than a
`Snapshot`**. That keeps the crate pure and dependency-free — it still builds and
tests with no libghostty and no system libraries, which is what lets it run in the
cheap half of CI. The caller renders a `Snapshot` to text and hands it over; the
one-liner is `machine.observe_terminal(&approval::hint(&snapshot.screen_text()))`.

The probe is an *example* on `ansible-terminal` with a dev-dependency on
`ansible-hooks`, so the dependency exists only for the example. The library still
depends on neither the hub nor anything above it.

---

## 2. What a real prompt looks like

Two shapes, both recorded. A `Bash` call:

```text
────────────────────────────────────────────────────────────────────
 Bash command

   python3 -c "print(sum(range(1000000000)))"
   Sum integers 0 to 999999999 in Python

 This command requires approval

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don’t ask again for: python3 *
   3. No

 Esc to cancel · Tab to amend · ctrl+e to explain
```

A `Write` call, with a rendered diff above the question:

```text
 Create file
 probe.txt
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
  1 hello
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Do you want to create probe.txt?
 ❯ 1. Yes
   2. Yes, allow all edits during this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend
```

The question differs per tool, the middle option differs per tool, and the
content above the question is arbitrary. What is stable is the shape *below* the
question, which is what the detector keys on.

**When the prompt is answered it is gone entirely** — replaced by the ordinary
input box and status footer, with nothing left in the viewport. That matters more
than it sounds: it means the falling edge is as observable as the rising one,
which is the half no hook provides.

---

## 3. Detection: six signals, one of them positional

A screen is a prompt only if **all** of these hold:

| Signal | Why it is not sufficient alone |
|---|---|
| A line starting `Do you want` and ending `?` | This repository's own documentation contains that string verbatim |
| At least two numbered options | Any list |
| A `❯` marker on one of them | Absent from prose, present in the trust gate too |
| An option that refuses (`No`, `No, exit`) | A chooser that cannot be declined is some other UI |
| An `Esc to cancel` footer | Shared with the trust gate |
| **That footer being the last thing on screen** | The signal that actually separates a modal from a description of one |

The first five are content, and **content turned out not to be enough.** The
review of this change found that `detect()` fired on this very document, on the
detector's own doc comment, and on the checked-in `.screen` fixtures: a fenced
example contains the whole block, every line is trimmed before matching, so
indentation does not save it. A session that ran `cat` on any of them would have
gone to `AwaitingApproval` with nobody waiting — precisely the over-report this
module calls unrecoverable. The test that was supposed to cover it passed only
because its sample had been weakened to prose with no marker and no footer.

The sixth signal is positional and fixes it. The modal *replaces* the input box,
so nothing is drawn beneath it — true of both recorded prompt screens, where the
footer is the last non-blank line. A session merely displaying a prompt still has
its own input box and status line underneath. Position distinguishes the two;
content cannot.

**The asymmetry is the design.** A missed prompt degrades to `Working` plus a
visible pending age, which is honest and recoverable. A false prompt trains
people to ignore the single status meant to summon them, and that is not. So when
in doubt the detector reports nothing — and where the screen looks *half-drawn*
rather than empty, it reports `TerminalHint::Indeterminate`, which changes
nothing at all. That third state exists because a frame caught between the
question being drawn and the options being drawn would otherwise read as "no
prompt" and blink the grid off `AwaitingApproval` and straight back — the same
defect as the `Notification` bug in §5, arriving from the other side.

Two wrap cases are handled, and were not at first: an option that soft-wraps onto
a second screen line no longer ends the block, and a question whose object wraps
is rejoined across however many rows it occupies — bounded at four, and stopped by
a blank line, a second `?`, an option or the footer, so a stray question mark
cannot reach back and borrow someone else's `Do you want`. Every fixture was
recorded at 120 columns where nothing wrapped, so nothing caught this — at 80
columns a long `2. Yes, and don't ask again for: …` would have made the whole
prompt invisible, and a deep path in a resized pane takes more than the one
continuation row the first fix allowed for.

These are a TUI's strings, so they will move. That is why they are named
constants, why the fixtures are recordings, and why re-recording is one script.

---

## 4. Measurements

From `scripts/probe-approval.sh`, across six runs on this machine.

| What | Measured |
|---|---|
| Prompt drawn → `AwaitingApproval` | **1.3 ms · 1.6 ms · 1.7 ms · 1.8 ms · 3.6 ms** across runs |
| Keystroke → back to `Working` | **21–62 ms** |
| Detector cost per screen | mean **21–26 µs**, max 3.2 ms (5,953–23,168 screens per run) |
| Longest open bracket with no prompt | **15.1 s**, across 2,107 screens read mid-tool |
| `Notification` after the prompt is drawn | **~6,000 ms** |
| `PreToolUse` before the prompt is drawn | **~30 ms** |

The first row is the one the claim needed, and it is three orders of magnitude
inside the one-second target. The reason is that there is no polling interval in
it: the host already takes a snapshot on damage to render, and the detector is
25 µs of work on a screen it was going to read anyway. **Recognising a prompt is
free relative to drawing one.**

The detector cost's *max* is 3.2 ms against a 21 µs mean, which is allocation
noise from `screen_text()` on a busy frame, not a tail in the parse.

### The three claims

| Claim | Result |
|---|---|
| A real prompt drives `AwaitingApproval` within a second | **1.3–3.6 ms.** Detail `awaiting approval: Write` — the noun from `PreToolUse`, the verb from the screen |
| Answering it returns the session to `Working` | **21–62 ms**, and the answered screen holds no prompt |
| A long legitimate tool call never trips it | 15.1 s bracket, 2,107 mid-tool screens, never tripped |

The third claim is *also* asserted deterministically, in
`no_elapsed_time_turns_a_working_screen_into_an_approval`, which walks a real
bracket's age out to an hour against the recorded mid-tool screen. The live probe
can only show that the environment's longest tool did not trip it; the test shows
that elapsed time is not an input at all. That test needs no credentials, so the
strongest form of the claim is the one that runs in CI.

---

## 5. The bug this found

`StatusMachine::apply` treated every `Notification` as `AwaitingInput`. On a real
prompt:

```
[20010 ms] hook      Working -> Working  running: Bash   (PreToolUse)
[20060 ms] terminal  Working -> AwaitingApproval         awaiting approval: Bash
[26583 ms] hook      AwaitingApproval -> AwaitingInput   "Claude needs your permission"   (Notification)
[26585 ms] terminal  AwaitingInput -> AwaitingApproval   awaiting approval: Bash
```

Six seconds after the grid correctly said `AwaitingApproval`, the notification
*about that prompt* moved it to `AwaitingInput`, and the next screen read moved it
back. Two spurious transitions, two history rows, and a grid that flickers off the
one status a teammate can act on — caused by the event whose whole purpose is to
say a human is needed.

The fix is one guard: a notification while a prompt is on screen is *about* that
prompt, so it does not change the status. `PreToolUse` already had this guard;
`Notification` did not, because when it was written the event had never been
observed firing.

This is the argument for building the two halves together rather than in
sequence. Neither half is wrong alone — the detector was right, the hook handler
was defensible — and the interaction was only visible with both running against a
real session.

---

## 6. What this changes about the hook story

Two of hook-coverage §5's open questions are now answered, and the answers are
more interesting than expected.

**Does `Notification` fire on a real permission prompt? Yes** — with the message
`"Claude needs your permission"`, about six seconds late. It is a genuine signal
and far too slow to be the grid's, and it was actively harmful until §5's fix. It
also fires with `"Claude is waiting for your input"` when a session sits idle,
which is a different meaning on the same channel, so the *message* carries the
distinction and the event does not.

**Does `PermissionRequest` fire? Yes**, and it is the most useful new fact here.
Recorded payload:

```jsonc
{"hook_event_name":"PermissionRequest","tool_name":"Write",
 "tool_input":{"file_path":"…/probe.txt","content":"hello\n"},
 "permission_mode":"default",
 "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}
```

It fired **twice in a session with three tool calls** — once for each of the two
that were actually asked about, and not for the `echo` that was allowed. So unlike
`PreToolUse`, it means "a human is being asked", which is precisely the rising
edge hook-coverage concluded did not exist.

**This does not overturn the finding, and the schema rule should stand.** What
hook-coverage actually proved is that *the deny path is unobservable in hooks* —
and that is still true. `PermissionRequest` gives the rising edge; nothing gives
the falling edge when a prompt is declined or dismissed with `Esc`, because no
event fires for either. A grid driven by `PermissionRequest` alone would show
`AwaitingApproval` until the next `Stop`, which on a denial can be a long way off.

So the terminal remains the only complete producer, and
`update_session_status`'s rule that `AwaitingApproval` may come only from
`StatusSource::Terminal` remains correct as written.

What `PermissionRequest` is genuinely good for is **corroboration**, and there is
a decision to make about whether to use it. It could confirm a screen detection,
or catch a prompt drawn in a shape the detector does not know — at the cost of
letting a second producer influence the most important status in the system. It is
deliberately **not wired up here**; the typed variant is not even modelled, so it
parses as `HookEvent::Unknown` and changes nothing. That is a schema decision, not
a bug fix, and the plan should make it explicitly.

---

## 7. Environment limits worth recording

Two behaviours shaped the probe and would shape the app.

**Claude Code backgrounds a long `sleep`.** Asked to run `sleep 35 && echo`, it
dispatched a background task: `PostToolUse` arrived with `duration_ms: 41`,
followed by a `SubagentStop` and a `<task-notification>` prompt 35 s later. The
bracket closed in 41 ms, so the intended slow-tool window never existed.

**This environment blocks foreground sleeps outright.** Pressed to run one in the
foreground, the session refused and explained that "foreground sleep is
disallowed in this environment", offering `run_in_background` instead.

The negative case therefore uses a CPU-bound command — `python3 -c
"print(sum(range(1000000000)))"` — which is neither special-cased nor
backgroundable, and which held its bracket open for 15.1 s. This is also why the
unbounded form of the claim lives in a deterministic test rather than in the live
probe: a probe whose central assertion depends on how the model chooses to
dispatch a tool is a flaky probe.

**A nested session does not write its own transcript.** The child reported
`Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker`. Harmless
here, since W4 needs hooks and the screen rather than the JSONL, but it will
matter for anything that wants Claude Code's own transcript for the structured
event index (A1).

---

## 8. Where the plan should change

| Plan text | Change |
|---|---|
| phase-1-execution §5 W4 — "nothing constructs the `TerminalHint`" | **Done.** `ansible_hooks::approval` constructs it; `scripts/probe-approval.sh` is the standing evidence |
| hook-coverage §5 — "`Notification` was never observed firing" | It fires, ~6 s after the prompt, and had to be guarded against demoting `AwaitingApproval` |
| hook-coverage §5 — "what does `PermissionRequest` carry, if it fires at all?" | It fires, once per prompt, with `tool_name` and `tool_input`. Decide whether to model it — see §6 |
| hook-coverage §3 — "no hook reports a pending approval" | Sharpen: no hook reports a *dismissed* approval. `PermissionRequest` does report a pending one |
| §2 reducers — `AwaitingApproval` only from `StatusSource::Terminal` | **Unchanged, and now better justified:** the falling edge is observable only on screen |
| phase-1-execution §7 — "W4 cannot detect a prompt reliably → surface pending age instead" | Not triggered |

Still open, and now with a producer to hang them on: the app-side wiring
(`apps/desktop` is still the Spike A harness, so nothing calls this yet), and
whether a prompt can be *answered* from a teammate's viewer, which is open
question #9 and not in the MVP.

### Known gap: prompts that do not ask "Do you want"

The question prefix is the anchor, so a modal that blocks on a human but phrases
its question differently is **not detected**, and the session shows `Working` with
a pending tool instead. The plan-mode confirmation is the case to check first: it
has every structural signal — marker, three numbered options, a refusal, the same
footer — but reportedly asks `Ready to code?`.

This is deliberately not pattern-matched on a hunch. The whole reason the strings
here are fixtures is that they were observed rather than guessed, and adding a
second anchor from memory would put an unrecorded string in the one place this
document insists on recordings. **The work is: drive a session into plan mode with
`scripts/probe-approval.sh`, record the screen, then widen the anchor with a
fixture behind it.** Until then the gap is a documented miss rather than a silent
one, which is the same bargain §3 makes everywhere else.

---

## 9. How to re-run

```bash
scripts/probe-approval.sh                  # 18 assertions against a real session
cargo test -p ansible-hooks                # 62 tests, incl. the recorded screens
```

The first needs interactive `claude` credentials and a machine where a permission
prompt actually appears. The second needs neither, and is where the claims that
should never regress are asserted.
