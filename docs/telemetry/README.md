# Telemetry

Probe bundles from machines that actually have the thing installed, kept so a later
run diffs against a real baseline rather than against documentation.

## `herdr-0.7.5-20260731/`

`scripts/probe-herdr.sh` against Herdr 0.7.5, protocol 17, Linux. 10 pass, 4 fail,
8 unanswered. All four failures are fixed; the analysis is in
[docs/plan/herdr-plugin.md](../plan/herdr-plugin.md#what-the-telemetry-said).

`raw/session-snapshot.json` and `raw/agent-list.json` are the two files worth
treating as fixtures — the test module in `crates/ansible-herd/src/herdr.rs` is now
shaped from them rather than from prose.

Captures went through the plugin's redactor. `.err` files were empty and were
dropped. Re-run after any Herdr upgrade; the point of keeping this is the diff.

## `herdr-0.7.5-macos-20260731/`

Same Herdr, same protocol, macOS, and against a session with eight live agent panes
rather than none. 18 pass, 1 fail, 12 unanswered.

Most of what the Linux bundle left open is answered here, because the checks that
need a real pane finally had one: `pane.report_metadata` round-trips (E1, E2, E4),
`terminal session observe` streams `terminal.frame` records carrying base64 under
`bytes` — `teleport.rs`'s first-choice name — and `agent_status` was seen as
`working`, `idle`, and `done`. Every field the parsers probe for was reached under
its *first* choice, so no fallback fired: B2 passes and the doc-derived-parser risk
is largely retired.

The one failure is D1, already known and already handled by polling. What the run
found beyond that is in
[docs/plan/herdr-plugin.md](../plan/herdr-plugin.md#what-the-macos-run-added).

Two departures from the bundle above: the Linux run's five "missing" fields were an
artifact of a broken probe rather than of Herdr, and `raw/observe-decoded.txt` — 100
KB of one pane's real scrollback — was dropped by hand. `raw/frame-audit.txt` and
`raw/observe-shapes.ndjson` carry the finding without carrying the session.

Everything under here is exempt from `oxfmt` (see `.oxfmtrc.json`). A capture is a
verbatim response, and a reformatted one is no longer evidence.

Everything under here is exempt from `oxfmt` (see `.oxfmtrc.json`). A capture is a
verbatim response, and a reformatted one is no longer evidence.
