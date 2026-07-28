//! Input-to-glyph latency and high-volume throughput measurement.
//!
//!   cargo run --release -p ansible-terminal --example vt-latency
//!
//! Latency here is: write a keystroke to the PTY, then poll until the glyph
//! appears in a snapshot taken from libghostty's render state. It therefore
//! covers PTY round trip + VT parse + render-state update + snapshot copy —
//! everything except the pixels a GUI would then draw.

use std::time::{Duration, Instant};

use ansible_terminal::{
    GhosttyTerminal, TerminalBackend, TerminalConfig, TerminalEvent, TerminalEvents, TerminalInput,
    TerminalSize,
};

fn samples_wanted() -> usize {
    std::env::var("SAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(200)
}
fn bulk_lines() -> usize {
    std::env::var("BULK_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(50_000)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = TerminalSize::new(120, 40, 8, 16);
    let mut term = GhosttyTerminal::spawn(
        TerminalConfig::command("/bin/sh", size).env("PS1", "").env("LC_ALL", "C.UTF-8"),
    )?;

    // Drain the raw-output tee for the whole run. Without a consumer the
    // bounded queue fills and the measurement reflects dropped transcript
    // bytes rather than terminal latency.
    let drain = drain_events(term.events());

    // `cat` echoes each byte straight back, which isolates the terminal path
    // from any shell prompt or line-editing work.
    //
    // Raw mode matters: in the default cooked mode the tty line-buffers, so a
    // single keystroke would not come back until Enter, and it echoes control
    // bytes in caret notation so a clear sequence would be printed rather than
    // obeyed. Either would make this measure the tty instead of the terminal.
    term.send(TerminalInput::Text("stty raw -echo; exec cat\n".into()))?;
    term.pump_until(Duration::from_millis(800), |_| false)?;

    // Confirm the echo path is byte-for-byte before timing anything.
    term.send(TerminalInput::Text("\x1b[2J\x1b[HPROBE".into()))?;
    let echoing = term.wait_for_screen(Duration::from_secs(5), |s| s.screen_text() == "PROBE")?;
    if !echoing {
        return Err("cat is not echoing cleanly; cannot measure latency".into());
    }

    let mut samples = measure_latency(&mut term)?;
    samples.sort_unstable();

    println!("input-to-glyph latency over {} samples", samples.len());
    for (label, value) in [
        ("p50", percentile(&samples, 50)),
        ("p90", percentile(&samples, 90)),
        ("p99", percentile(&samples, 99)),
        ("max", *samples.last().unwrap()),
    ] {
        println!("  {label:>4}: {:>8.3} ms", value.as_secs_f64() * 1000.0);
    }

    let dropped = term.dropped_output_bytes();
    term.shutdown()?;
    drop(term);
    let _ = drain.join();
    if dropped > 0 {
        println!("  note: {dropped} tee bytes dropped during the latency phase");
    }

    measure_throughput(size)?;
    Ok(())
}

/// Consume raw-output events on a background thread, returning total bytes.
fn drain_events(events: TerminalEvents) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut bytes = 0usize;
        for event in events {
            if let TerminalEvent::Output(b) = event {
                bytes += b.len();
            }
        }
        bytes
    })
}

/// Type one unique character at a time and time until it is visible.
fn measure_latency(
    term: &mut GhosttyTerminal,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    // Distinct printable characters so each probe is unambiguous on screen.
    let alphabet: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
    let wanted = samples_wanted();
    let mut samples = Vec::with_capacity(wanted);

    for i in 0..wanted {
        let probe = alphabet[i % alphabet.len()];
        // Clear the screen so the previous probe cannot be mistaken for this one.
        term.send(TerminalInput::Raw(b"\x1b[2J\x1b[H".to_vec()))?;
        term.pump_until(Duration::from_millis(50), |t| {
            t.snapshot().map(|s| s.screen_text().is_empty()).unwrap_or(false)
        })?;

        let start = Instant::now();
        term.send(TerminalInput::Text(probe.to_string()))?;
        let seen = term
            .wait_for_screen(Duration::from_secs(5), |snap| snap.screen_text().contains(probe))?;
        if seen {
            samples.push(start.elapsed());
        }
    }

    if samples.is_empty() {
        return Err("no latency samples were captured".into());
    }
    Ok(samples)
}

/// Push a large amount of output through and measure sustained throughput.
fn measure_throughput(size: TerminalSize) -> Result<(), Box<dyn std::error::Error>> {
    let mut term = GhosttyTerminal::spawn(
        TerminalConfig::command("/bin/sh", size).env("PS1", "").env("LC_ALL", "C.UTF-8"),
    )?;
    let events = term.events();

    let lines = bulk_lines();
    let script = format!(
        "i=0; while [ $i -lt {lines} ]; do echo \"bulk $i {}\"; i=$((i+1)); done; echo BULKDONE\n",
        "x".repeat(60)
    );

    // A transcript consumer has to keep up; measuring while nobody drains
    // would just measure the bounded queue overflowing.
    let drain = drain_events(events);

    let start = Instant::now();
    term.send(TerminalInput::Text(script))?;
    let done = term.wait_for_screen(Duration::from_secs(180), |snap| {
        snap.screen_text().contains("BULKDONE")
    })?;
    let elapsed = start.elapsed();

    let dropped = term.dropped_output_bytes();
    term.shutdown()?;
    // Dropping the terminal releases the event sender; without that the drain
    // loop never sees a disconnect and the join below would block forever.
    drop(term);
    let bytes = drain.join().unwrap_or(0);

    println!("\nhigh-volume output");
    println!("  lines requested : {lines}");
    println!("  completed       : {done}");
    println!("  elapsed         : {:.2} s", elapsed.as_secs_f64());
    println!("  bytes tee'd     : {:.1} MiB", bytes as f64 / (1024.0 * 1024.0));
    println!(
        "  throughput      : {:.1} MiB/s",
        (bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64().max(f64::EPSILON)
    );
    println!("  dropped bytes   : {dropped}");
    if dropped > 0 {
        println!("  WARNING: the transcript tee lost bytes; capture would have a gap.");
    }

    Ok(())
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[idx]
}
