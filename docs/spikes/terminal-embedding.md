# Spike A: libghostty rendering and input in Tauri

## Status

**In progress; environment prerequisite discovery and host contract complete.**

The current Linux container has Rust and Node, but it has neither libghostty nor
the GTK/WebKit development packages required by a Linux Tauri harness. Network
access to crates.io and GitHub also returns HTTP 403, so this environment cannot
fetch Tauri, `libghostty-rs`, or Ghostty source. These are environment findings,
not evidence against the proposed integration.

## Contract established

`ansible-terminal` is dependency-free and deliberately unaware of Tauri and the
hub. `TerminalBackend` owns the PTY and renderer, accepts typed byte, paste, and
resize input, and emits lifecycle plus raw PTY output to the host. Raw output is
for transcript capture; libghostty remains responsible for screen rendering.

This keeps the eventual libghostty implementation native. It does not introduce
xterm.js as an interim renderer or silently turn the webview into the terminal.

## Next experiment

Run the following on a Linux workstation with GTK/WebKit development packages
and access to the relevant source repositories:

1. Pin Ghostty and `libghostty-rs` revisions rather than tracking their heads.
2. Implement `TerminalBackend` using the actual binding surface.
3. Place the libghostty GTK child surface beside, not over, the Tauri webview.
4. Launch a shell, then a real Claude Code session.
5. Verify text, truecolor, box drawing, modifiers, paste, `Ctrl-C`, focus, resize,
   `SIGWINCH`, and process exit.
6. Record input-to-glyph latency and behavior under sustained output.

The native child-surface approach is the first candidate because it avoids a
frame copy and exercises libghostty's renderer. Offscreen composition should be
considered only if GTK child-window layout cannot coexist reliably with Tauri.

## Exit criteria

The spike is complete only after the same contract works on macOS and Linux, or
after a documented libghostty limitation forces a different renderer. Missing
packages in this container do not satisfy that kill criterion.
