# Spike B — hook coverage and the grid status signal

**Status:** done, and it changes the schema.

**Question asked** (open question #2, gating Phase 1): do Claude Code hooks plus
the session JSONL yield a status signal good enough to drive the grid?

**Answer:** mostly. Four of the eight statuses come out cleanly and cheaply. The
fifth — `AwaitingApproval`, which the plan calls *"the interruption a teammate
can actually resolve, and the highest-value thing the grid can surface"* — **is
not derivable from hooks at all**, and no timeout heuristic recovers it. It has
to come from the terminal, which the app already owns.

Everything below was measured against real `claude` v2.1.220 sessions, not read
from documentation. `scripts/capture-hook-payloads.sh` reproduces the recordings;
`crates/ansible-hooks/tests/fixtures/` holds them; the tests replay them.

---

## 1. Method

A session-scoped settings overlay — exactly the mechanism §3 of the plan
describes — subscribes every hook event name that appears in the Claude Code
binary, pointing each at a receiver that appends one JSON line per invocation:

```bash
claude --print --settings overlay.json 'Use Bash to run: echo …'
```

Two runs, because the interesting difference is what happens when a tool is
*not* allowed:

| Recording | How | Recorded sequence |
|---|---|---|
| `tool-allowed.jsonl` | `--permission-mode acceptEdits` | `SessionStart → UserPromptSubmit → PreToolUse → PostToolUse → Stop → SessionEnd` |
| `tool-denied.jsonl` | a `PreToolUse` hook returning `permissionDecision: deny` | `SessionStart → UserPromptSubmit → PreToolUse → Stop → SessionEnd` |

Reproduce with `scripts/capture-hook-payloads.sh`.

---

## 2. What fires, and what each payload carries

Of the eleven event names present in the binary, **six fired**:
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`,
`SessionEnd`. Not seen: `Notification`, `PermissionRequest`, `SubagentStop`,
`PreCompact`, `PostToolUseFailure` — see [§5](#5-environment-limits) for which
absences are environmental and which are not.

Three properties hold across every payload, and each one simplifies the design:

**Every payload carries `hook_event_name`.** The event self-identifies, so the
receiver needs one endpoint rather than eleven, and cannot mislabel an event by
wiring a command to the wrong key.

**Every payload carries `session_id` and `transcript_path`.** That is enough to
route an event to a session and to locate Claude Code's own JSONL, with no
bookkeeping of our own. `transcript_path` looked like
`~/.claude/projects/<slug>/<session_id>.jsonl`.

**Payloads carry fields we do not model** — `effort`, `prompt_id`,
`background_tasks`, `session_crons`. So parsing must ignore unknown fields *and*
unknown events, or a Claude Code upgrade breaks the receiver. Both are tested.

Fields that matter, by event:

| Event | Beyond the common fields |
|---|---|
| `SessionStart` | `source` (observed `"startup"`) |
| `UserPromptSubmit` | `prompt`, `prompt_id`, `permission_mode` |
| `PreToolUse` | `tool_name`, **`tool_use_id`**, `tool_input` |
| `PostToolUse` | `tool_name`, `tool_use_id`, `tool_response`, **`duration_ms`** |
| `Stop` | `last_assistant_message`, `stop_hook_active` |
| `SessionEnd` | `reason` (observed `"other"`, even on a clean exit) |

`tool_use_id` is load-bearing: several tools can be in flight at once, so
pairing `PreToolUse` with `PostToolUse` by tool name alone would mismatch them.

`SessionEnd.reason` was `"other"` on a *successful* run, so it is **not** an
error signal. `Failed` must come from the supervisor's exit status instead.

---

## 3. The finding: `AwaitingApproval` is not in the hooks

Three measurements, in the order that makes the conclusion unavoidable.

**A denied tool fires `PreToolUse` and never `PostToolUse`.** The bracket is left
dangling, and no event reports the denial:

```
SessionStart → UserPromptSubmit → PreToolUse(Bash) → Stop → SessionEnd
                                  └─ never closed
```

**A legitimate slow tool produces the same dangling shape.** `sleep 8 && echo`
held the bracket open for 9,246 ms, with `PostToolUse.duration_ms = 9155`
confirming the tool really did run that long. A real build or test run holds it
for minutes.

**So no timeout separates them.** "PreToolUse with no PostToolUse for N seconds"
means *either* awaiting a human *or* working hard, and the app cannot tell which.
Picking any N would mislabel one of the two — and mislabelling `Working` as
`AwaitingApproval` is the worse error, because it trains people to ignore the
one status that is supposed to summon them.

`crates/ansible-hooks` encodes this rather than papering over it. There is a test
whose entire purpose is to assert the ambiguity:

```rust
// Same status, same detail, same pending age. Only the terminal differs.
assert_eq!(slow.status(), blocked.status());
assert_eq!(slow.longest_pending_ms(11_000), blocked.longest_pending_ms(11_000));
```

### What does work: the terminal

The app owns the PTY, and the approval prompt is rendered on screen. Spike A's
`Snapshot` already exposes the visible grid as text, so the host can see the
prompt directly — the one place this state is observable.

`StatusMachine` therefore takes `AwaitingApproval` only from an explicit
[`TerminalHint`], never from a timer:

```rust
machine.apply(&pre_tool_use, at_ms);            // -> Working: "running: Bash"
machine.observe_terminal(&TerminalHint::ApprovalPrompt { tool_name: None });
// -> AwaitingApproval: "awaiting approval: Bash"
```

The tool *name* still comes from `PreToolUse`, which is reliable. Hooks supply
the noun; the terminal supplies the verb. Neither alone is enough, and making
that a type-level requirement means nobody can accidentally ship the guess.

### And the transcript, after the fact

Claude Code's own JSONL *does* record the denial:

```jsonc
{"type":"user","toolDenialKind":"permission-rule",
 "message":{"content":[{"type":"tool_result","is_error":true,
                        "content":"…denial reason…"}]}}
```

That is authoritative but retrospective — it lands once the decision is made, so
it can reconcile history and drive the structured event index, but it cannot tell
the grid that someone is *waiting right now*. Useful for correctness, useless for
the interrupt.

---

## 4. Schema implications

Four consequences for §2 of the plan, in descending order of how much they change.

**1. `AwaitingApproval` needs a documented source, and it is not the hook path.**
The plan's §3 lists status as flowing "hooks → localhost receiver → status
machine". That is right for four statuses and wrong for this one. The status
machine needs a second input from the terminal, so `crates/ansible-hooks` cannot
be the sole producer of `update_session_status`. The split is now explicit in the
`StatusMachine` API.

**2. `status_detail` should stay a short unstructured string.** The plan says to
"resist making it structured until you know what the hooks actually give you."
Now we know: `tool_name` is reliable and is the only thing worth putting in the
detail. `running: Bash` and `awaiting approval: Bash` are the two shapes that
matter. Structuring further would encode a `tool_input` schema that varies per
tool and is sometimes large.

**3. `Failed` cannot come from `SessionEnd.reason`.** It was `"other"` on a clean
exit. The session supervisor's exit status is the only trustworthy source, which
means `close_session(exit_reason)` must be called by the supervisor and not
driven off the hook.

**4. `Idle` has no hook source, and may not be needed.** Nothing distinguishes
"finished a turn" from "idle for a while": `Stop` gives `AwaitingInput`, and idle
is that state plus elapsed time. Either derive it in the viewer from
`last_event_at`, or drop the variant. Carrying a status nothing can set is worse
than not having it.

Status coverage as measured:

| Status | Source | Reliable |
|---|---|---|
| `Starting` | `SessionStart` | yes |
| `Working` | `UserPromptSubmit`, `PreToolUse` | yes |
| `AwaitingInput` | `Stop` | yes |
| `Done` | `SessionEnd` | yes |
| **`AwaitingApproval`** | **terminal snapshot only** | **needs the PTY** |
| `Failed` | supervisor exit status | not a hook |
| `Detached` | heartbeat reaper | not a hook |
| `Idle` | nothing | consider dropping |

### Cost

Six hook invocations for a one-tool turn, and the status machine collapses those
into three transitions. `update_session_status` is documented as the hottest
reducer in the system; collapsing in the machine rather than in the reducer is
what keeps that cheap, and there is a test asserting transitions are strictly
fewer than events.

---

## 5. Environment limits

Distinguishing these from real findings matters, because one of them looks like a
gap and is not.

**No interactive session.** Interactive `claude` requires OAuth here, which the
`--print` path bypasses via injected credentials. So a genuine approval *prompt*
could not be produced, and `Notification` was never observed firing. Its payload
type is modelled but untested against a real payload.

**Permission mode is forced.** `--permission-mode manual` was silently
downgraded to `default`, and `bypassPermissions` is refused for root. This
container's policy forces a permissive mode, so tool calls are auto-allowed.
That is why the deny path had to be reached with a `PreToolUse` hook returning
`deny` rather than by declining a prompt.

**This does not weaken the central finding.** The deny path is what a refused
approval produces, and it is the *absence* of `PostToolUse` that matters. Two
things remain to confirm on a machine with interactive auth:

1. Does `Notification` fire when a permission prompt appears? If it does, it is a
   *cheaper* trigger than snapshotting the terminal — but the terminal is still
   needed to know when the prompt clears, so the design does not change.
2. What does `PermissionRequest` carry, if it fires at all?

Neither can turn `AwaitingApproval` into a hooks-only signal, because neither
reports the prompt being *dismissed*.

---

## 6. What this leaves for the deployed half

**Done** — the hub module is on Maincloud, the Worker and relay are built, and the
round trip is measured: [deployed-round-trip.md](deployed-round-trip.md).

Two of §4's consequences are now enforced by the deployed schema rather than only
written down. `update_session_status` takes a `StatusSource` and **rejects**
`AwaitingApproval` from the hook path and `Failed` from anything but the supervisor,
so the ambiguity this document found cannot be papered over by a future caller.
`Idle` was dropped from `SessionStatus`, per §4's fourth point.

Ready for a full environment, because it is settled here:

- `crates/ansible-hooks` parses real payloads and derives status, with recorded
  fixtures as the regression net.
- The receiver contract is fixed: one endpoint, dispatch on `hook_event_name`,
  route on `session_id`, ignore unknown events and fields.
- The reducer surface needs no change beyond the four points in §4.

The remaining hook work needs a machine with interactive auth, not
infrastructure: confirm `Notification`, and confirm the terminal-snapshot
detection of an approval prompt against a real prompt rather than a synthetic
hint.
