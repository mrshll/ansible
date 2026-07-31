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
