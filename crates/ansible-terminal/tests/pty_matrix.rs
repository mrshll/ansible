//! Spike A verification matrix, run against a real PTY and a real shell.
//!
//! Everything here goes through the full path — spawn, PTY, libghostty-vt
//! parse, render state, snapshot — so a pass means the embedding really works,
//! not that a mock agrees with itself.

#![cfg(unix)]

use std::time::{Duration, Instant};

use ansible_terminal::{
    ExitReason, GhosttyTerminal, Key, KeyEvent, Modifiers, TerminalBackend, TerminalConfig,
    TerminalEvent, TerminalInput, TerminalSize,
};

const TIMEOUT: Duration = Duration::from_secs(20);

fn size(cols: u16, rows: u16) -> TerminalSize {
    TerminalSize::new(cols, rows, 8, 16)
}

fn shell(cols: u16, rows: u16) -> GhosttyTerminal {
    let config = TerminalConfig::command("/bin/sh", size(cols, rows))
        .env("PS1", "")
        .env("LC_ALL", "C.UTF-8");
    GhosttyTerminal::spawn(config).expect("spawn shell")
}

/// Run one command and wait for the screen to satisfy `matches`.
///
/// The PTY echoes whatever we type, so the command line itself lands on screen
/// before the command runs. Clearing as the command's own first step erases
/// that echo. Note that `matches` still sees the echo briefly, so predicates
/// that could be satisfied by the command text itself must compare the whole
/// screen rather than use `contains` — see `expect_screen`.
fn run(term: &mut GhosttyTerminal, cmd: &str, matches: impl Fn(&str) -> bool) -> String {
    term.send(TerminalInput::Text(format!("printf '\\033[2J\\033[H'; {cmd}\n"))).expect("send");
    let ok = term.wait_for_screen(TIMEOUT, |snap| matches(&snap.screen_text())).expect("pump");
    let screen = term.snapshot().expect("snapshot").screen_text();
    assert!(ok, "timed out waiting after `{cmd}`; screen was:\n{screen}");
    screen
}

/// Wait for the screen to become exactly `expected`.
///
/// Race-free where `contains` is not: the echoed command line is still on
/// screen until the command's leading clear runs, and it contains the very text
/// most assertions look for.
fn expect_screen(term: &mut GhosttyTerminal, cmd: &str, expected: &str) {
    run(term, cmd, |s| s == expected);
}

#[test]
fn normal_text_renders() {
    let mut term = shell(80, 10);
    expect_screen(&mut term, "printf 'hello spike a\\n'", "hello spike a");
}

#[test]
fn ansi_palette_colors_render() {
    let mut term = shell(80, 10);
    expect_screen(&mut term, "printf '\\033[31mRED\\033[0m\\n'", "RED");

    let snap = term.snapshot().expect("snapshot");
    let (col, row) = find(&snap, 'R').expect("R on screen");
    let cell = snap.cell(col, row).unwrap();
    assert_ne!(cell.fg, snap.foreground, "SGR 31 should not be the default fg");
}

#[test]
fn truecolor_renders_exact_rgb() {
    let mut term = shell(80, 10);
    expect_screen(&mut term, "printf '\\033[38;2;12;240;77mT\\033[0m\\n'", "T");

    let snap = term.snapshot().expect("snapshot");
    let (col, row) = find(&snap, 'T').expect("T on screen");
    let cell = snap.cell(col, row).unwrap();
    assert_eq!(
        (cell.fg.r, cell.fg.g, cell.fg.b),
        (12, 240, 77),
        "truecolor must survive the round trip exactly"
    );
}

#[test]
fn box_drawing_characters_render() {
    let mut term = shell(80, 10);
    expect_screen(&mut term, "printf '\\342\\224\\214\\342\\224\\200\\342\\224\\220\\n'", "┌─┐");
}

#[test]
fn streaming_output_accumulates_in_order() {
    let mut term = shell(80, 12);
    let screen = run(&mut term, "for i in 1 2 3 4 5; do printf 'line%s\\n' $i; done", |s| {
        s.contains("line5")
    });
    let first = screen.find("line1").expect("line1");
    let last = screen.find("line5").expect("line5");
    assert!(first < last, "streaming output arrived out of order:\n{screen}");
}

#[test]
fn keyboard_input_reaches_the_child() {
    let mut term = shell(80, 10);
    // `cat` echoes what it receives, proving keystrokes traversed the PTY.
    term.send(TerminalInput::Text("cat\n".into())).expect("send");
    term.pump_until(Duration::from_millis(300), |_| false).expect("settle");

    for c in ['h', 'i'] {
        term.send(TerminalInput::Key(
            KeyEvent::press(Key::Char(c), Modifiers::NONE).with_text(c.to_string()),
        ))
        .expect("key");
    }
    term.send(TerminalInput::Key(KeyEvent::press(Key::Enter, Modifiers::NONE))).expect("enter");

    let ok = term
        .wait_for_screen(TIMEOUT, |snap| snap.screen_text().matches("hi").count() >= 2)
        .expect("pump");
    assert!(ok, "cat should echo the typed text back");
}

#[test]
fn ctrl_c_interrupts_the_foreground_process() {
    let mut term = shell(80, 10);
    term.send(TerminalInput::Text("sleep 60\n".into())).expect("send");
    term.pump_until(Duration::from_millis(500), |_| false).expect("settle");

    term.send(TerminalInput::Key(KeyEvent::press(Key::Char('c'), Modifiers::ctrl())))
        .expect("ctrl-c");

    // After the interrupt the shell accepts commands again.
    let screen = run(&mut term, "printf 'alive\\n'", |s| s.contains("alive"));
    assert!(screen.contains("alive"), "shell did not survive Ctrl-C:\n{screen}");
}

#[test]
fn bracketed_paste_is_framed_only_when_the_app_asks() {
    let mut term = shell(80, 10);

    // Mode 2004 is off by default, so paste must go through unframed.
    let plain = term.encode(&TerminalInput::Paste("abc".into())).expect("encode");
    assert_eq!(plain, b"abc", "unrequested bracketed paste would inject literal markers");

    // Turn mode 2004 on the way an application would, then re-encode.
    term.send(TerminalInput::Text("printf '\\033[?2004h'\n".into())).expect("send");
    let enabled = term
        .pump_until(TIMEOUT, |t| {
            t.encode(&TerminalInput::Paste("x".into())).is_ok_and(|b| b.starts_with(b"\x1b[200~"))
        })
        .expect("pump");
    assert!(enabled, "terminal never observed mode 2004");

    let framed = term.encode(&TerminalInput::Paste("abc".into())).expect("encode");
    assert_eq!(framed, b"\x1b[200~abc\x1b[201~");
}

#[test]
fn paste_content_reaches_the_child() {
    let mut term = shell(80, 10);
    term.send(TerminalInput::Text("cat\n".into())).expect("send");
    term.pump_until(Duration::from_millis(300), |_| false).expect("settle");

    term.send(TerminalInput::Paste("pasted-text".into())).expect("paste");
    term.send(TerminalInput::Key(KeyEvent::press(Key::Enter, Modifiers::NONE))).expect("enter");

    let ok = term
        .wait_for_screen(TIMEOUT, |s| s.screen_text().matches("pasted-text").count() >= 2)
        .expect("pump");
    assert!(ok, "pasted text should be echoed by cat");
}

#[test]
fn focus_events_are_suppressed_until_requested() {
    let mut term = shell(80, 10);
    assert!(
        term.encode(&TerminalInput::Focus(true)).expect("encode").is_empty(),
        "focus reporting is off by default and must emit nothing"
    );

    term.send(TerminalInput::Text("printf '\\033[?1004h'\n".into())).expect("send");
    let enabled = term
        .pump_until(TIMEOUT, |t| {
            !t.encode(&TerminalInput::Focus(true)).unwrap_or_default().is_empty()
        })
        .expect("pump");
    assert!(enabled, "terminal never observed mode 1004");
    assert_eq!(term.encode(&TerminalInput::Focus(true)).unwrap(), b"\x1b[I");
    assert_eq!(term.encode(&TerminalInput::Focus(false)).unwrap(), b"\x1b[O");
}

/// A child that reports its terminal size whenever SIGWINCH arrives.
///
/// Deliberately not a shell trap: `dash` only runs a WINCH trap at certain
/// points in its command loop and will not fire it while parked at an
/// interactive prompt, which makes a shell-based probe test the shell rather
/// than the resize path. A direct signal handler tests exactly what matters.
const WINCH_PROBE: &str = r#"
import signal, sys, os
def on_winch(*_):
    s = os.get_terminal_size()
    sys.stdout.write("SIZE-NOW %dx%d\n" % (s.columns, s.lines))
    sys.stdout.flush()
signal.signal(signal.SIGWINCH, on_winch)
sys.stdout.write("CHILD-READY\n")
sys.stdout.flush()
while True:
    signal.pause()
"#;

#[test]
fn resize_updates_the_grid_and_raises_sigwinch() {
    let config = TerminalConfig::command("python3", size(80, 24)).args(["-c", WINCH_PROBE]);
    let mut term = GhosttyTerminal::spawn(config).expect("spawn probe");

    let ready =
        term.wait_for_screen(TIMEOUT, |s| s.screen_text().contains("CHILD-READY")).expect("pump");
    assert!(ready, "probe never started");
    assert_eq!(term.size().cols, 80);

    term.resize(size(100, 30)).expect("resize");
    assert_eq!(term.size(), size(100, 30), "grid geometry must follow the resize");

    let signalled =
        term.wait_for_screen(TIMEOUT, |s| s.screen_text().contains("SIZE-NOW")).expect("pump");
    assert!(signalled, "child never received SIGWINCH");

    let snap = term.snapshot().expect("snapshot");
    assert_eq!(snap.cols, 100, "libghostty grid must follow the resize");
    assert_eq!(snap.rows, 30);
    assert!(
        snap.screen_text().contains("SIZE-NOW 100x30"),
        "child saw the wrong winsize:\n{}",
        snap.screen_text()
    );
}

#[test]
fn process_exit_is_reported() {
    let mut term = shell(80, 10);
    term.send(TerminalInput::Text("exit 7\n".into())).expect("send");

    let exited = term.pump_until(TIMEOUT, |t| t.has_exited()).expect("pump");
    assert!(exited, "shell never exited");

    let reason = term
        .events()
        .try_iter()
        .find_map(|e| match e {
            TerminalEvent::Exited(r) => Some(r),
            _ => None,
        })
        .expect("an Exited event");
    assert!(
        matches!(reason, ExitReason::Code(7) | ExitReason::Eof),
        "unexpected exit reason: {reason}"
    );
}

#[test]
fn raw_output_is_tee_d_for_transcript_capture() {
    let mut term = shell(80, 10);
    let events = term.events();
    run(&mut term, "printf 'tee-me\\n'", |s| s.contains("tee-me"));

    let raw: Vec<u8> = events
        .try_iter()
        .filter_map(|e| match e {
            TerminalEvent::Output(bytes) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect();

    assert!(!raw.is_empty(), "no raw PTY bytes were emitted");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("tee-me"), "raw tee missed the payload");
}

#[test]
fn sustained_high_volume_output_stays_consistent() {
    let mut term = shell(80, 24);
    let started = Instant::now();

    // ~20k lines. Enough to exercise scrollback churn and the bounded queue.
    let screen = run(
        &mut term,
        "i=0; while [ $i -lt 20000 ]; do echo \"bulk line $i padding padding padding\"; i=$((i+1)); done; echo BULKDONE",
        |s| s.contains("BULKDONE"),
    );
    let elapsed = started.elapsed();

    assert!(screen.contains("BULKDONE"), "bulk output never completed");
    assert!(
        elapsed < Duration::from_secs(60),
        "20k lines took {elapsed:?}, which is far slower than expected"
    );

    // The terminal must still be usable, not wedged.
    let after = run(&mut term, "printf 'still-here\\n'", |s| s.contains("still-here"));
    assert!(after.contains("still-here"));
}

#[test]
fn alternate_screen_applications_render_and_restore() {
    let mut term = shell(80, 12);
    // Enter alt screen, draw, leave. This is the shape of a TUI like `claude`.
    run(&mut term, "printf '\\033[?1049h\\033[2J\\033[HALT-SCREEN\\n'", |s| {
        s.contains("ALT-SCREEN")
    });
    let restored = run(&mut term, "printf '\\033[?1049lBACK\\n'", |s| s.contains("BACK"));
    assert!(
        !restored.contains("ALT-SCREEN"),
        "leaving the alternate screen must restore the primary one:\n{restored}"
    );
}

#[test]
fn shutdown_is_idempotent() {
    let mut term = shell(80, 10);
    term.shutdown().expect("first shutdown");
    term.shutdown().expect("second shutdown");
    assert!(term.has_exited());
}

/// First screen position holding `needle`.
fn find(snap: &ansible_terminal::Snapshot, needle: char) -> Option<(u16, u16)> {
    let want = needle.to_string();
    (0..snap.rows).find_map(|row| {
        (0..snap.cols).find_map(|col| {
            let cell = snap.cell(col, row)?;
            (snap.cell_text(cell) == want).then_some((col, row))
        })
    })
}
