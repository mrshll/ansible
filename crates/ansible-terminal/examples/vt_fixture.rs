//! Deterministic terminal fixture, no GUI required.
//!
//! Drives a real PTY through libghostty-vt and prints the resulting grid back
//! out as ANSI, so the rendering path can be eyeballed on a headless machine.
//! This is the standalone-binary half of the spike: it proves the terminal
//! crate works with no Tauri, no GTK, and no display server.
//!
//!   cargo run -p ansible-terminal --example vt-fixture
//!   cargo run -p ansible-terminal --example vt-fixture -- /bin/bash

use std::time::Duration;

use ansible_terminal::{
    GhosttyTerminal, Snapshot, TerminalBackend, TerminalConfig, TerminalInput, TerminalSize,
};

const SCRIPT: &str = r#"
printf 'plain text\n'
printf '\033[31mred\033[0m \033[32mgreen\033[0m \033[34mblue\033[0m\n'
printf '\033[38;2;255;128;0mtruecolor orange\033[0m\n'
printf '\033[1mbold\033[0m \033[3mitalic\033[0m \033[4munderline\033[0m \033[7minverse\033[0m\n'
printf '\342\224\214\342\224\200\342\224\254\342\224\200\342\224\220\n'
printf '\342\224\202 \342\224\202 \342\224\202\n'
printf '\342\224\224\342\224\200\342\224\264\342\224\200\342\224\230\n'
printf 'wide: \346\274\242\345\255\227 emoji: \360\237\221\215\n'
for i in 1 2 3; do printf 'stream %s\n' $i; done
printf 'FIXTURE-DONE\n'
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "/bin/sh".to_string());
    let size = TerminalSize::new(80, 24, 8, 16);

    let mut term = GhosttyTerminal::spawn(
        TerminalConfig::command(&command, size).env("PS1", "").env("LC_ALL", "C.UTF-8"),
    )?;

    term.send(TerminalInput::Text(SCRIPT.to_string()))?;
    let done = term.wait_for_screen(Duration::from_secs(20), |snap| {
        snap.screen_text().contains("FIXTURE-DONE")
    })?;

    let snapshot = term.snapshot()?;
    print_snapshot(&snapshot);
    term.shutdown()?;

    if !done {
        eprintln!("fixture did not complete within the timeout");
        std::process::exit(1);
    }
    println!("\nfixture completed: {}x{} grid", snapshot.cols, snapshot.rows);
    Ok(())
}

/// Re-emit the snapshot as ANSI. Every attribute printed here was read back out
/// of libghostty's render state, not echoed from the input.
fn print_snapshot(snap: &Snapshot) {
    println!("--- {} cols x {} rows ---", snap.cols, snap.rows);
    for row in 0..snap.rows {
        if snap.row_text(row).is_empty() {
            continue;
        }
        let mut line = String::new();
        for col in 0..snap.cols {
            let Some(cell) = snap.cell(col, row) else { continue };
            if cell.width == ansible_terminal::CellWidth::Spacer {
                continue;
            }
            let text = snap.cell_text(cell);
            if text.is_empty() {
                line.push(' ');
                continue;
            }
            line.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m",
                cell.fg.r, cell.fg.g, cell.fg.b, cell.bg.r, cell.bg.g, cell.bg.b
            ));
            if cell.style.bold {
                line.push_str("\x1b[1m");
            }
            if cell.style.italic {
                line.push_str("\x1b[3m");
            }
            if cell.style.underline {
                line.push_str("\x1b[4m");
            }
            line.push_str(text);
            line.push_str("\x1b[0m");
        }
        println!("{}", line.trim_end());
    }
    if let Some(cursor) = snap.cursor {
        println!("cursor: col {} row {} ({:?})", cursor.col, cursor.row, cursor.shape);
    }
}
