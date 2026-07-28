//! Record a session's raw PTY bytes to a file.
//!
//! This is the reference capture the golden round-trip test compares against,
//! and the tool used to derive the redaction ruleset from real output rather
//! than from guesswork.
//!
//!   cargo run -p ansible-terminal --example vt-record -- out.raw /bin/sh script.sh
//!   ANSIBLE_RECORD_SECONDS=20 cargo run -p ansible-terminal --example vt-record -- out.raw claude
//!
//! The bytes written are exactly what came off the PTY: unredacted, unparsed,
//! in arrival order. Treat the output file as sensitive.

use std::io::Write;
use std::time::{Duration, Instant};

use ansible_terminal::{
    GhosttyTerminal, TerminalBackend, TerminalConfig, TerminalEvent, TerminalInput, TerminalSize,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out_path = args.next().ok_or("usage: vt-record <out-file> <command> [args...]")?;
    let command = args.next().ok_or("usage: vt-record <out-file> <command> [args...]")?;
    let command_args: Vec<String> = args.collect();

    let seconds: u64 =
        std::env::var("ANSIBLE_RECORD_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(15);

    let mut terminal = GhosttyTerminal::spawn(
        TerminalConfig::command(&command, TerminalSize::new(120, 40, 8, 16))
            .args(command_args)
            .env("LC_ALL", "C.UTF-8"),
    )?;
    let events = terminal.events();

    // Drain on a thread so the tee never backpressures; a dropped byte here
    // would make the reference capture wrong in exactly the way that matters.
    let collector = std::thread::spawn(move || {
        let mut raw = Vec::new();
        for event in events {
            if let TerminalEvent::Output(bytes) = event {
                raw.extend_from_slice(&bytes);
            }
        }
        raw
    });

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !terminal.has_exited() {
        terminal.pump_until(Duration::from_millis(200), |_| false)?;
    }

    // A TUI that owns the alternate screen needs a nudge to exit cleanly.
    let _ = terminal.send(TerminalInput::Raw(vec![0x03]));
    terminal.pump_until(Duration::from_millis(300), |_| false)?;

    let dropped = terminal.dropped_output_bytes();
    let screen = terminal.snapshot()?.screen_text();
    terminal.shutdown()?;
    drop(terminal);

    let raw = collector.join().map_err(|_| "collector thread panicked")?;
    std::fs::File::create(&out_path)?.write_all(&raw)?;

    println!("recorded {} bytes to {out_path}", raw.len());
    println!("dropped  {dropped} bytes");
    println!("visible screen was {} lines", screen.lines().count());
    if dropped > 0 {
        eprintln!("WARNING: the tee dropped bytes; this capture is not a faithful reference");
        std::process::exit(1);
    }
    Ok(())
}
