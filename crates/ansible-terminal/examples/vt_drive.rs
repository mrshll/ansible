//! Drive an interactive TUI through the embedded terminal from a script.
//!
//!   cargo run -p ansible-terminal --example vt-drive -- script.txt claude
//!
//! Script lines, one step each:
//!
//! ```text
//! expect <substring>      wait until the screen contains it
//! expect? <substring>     wait, but continue if it never appears
//! send <text>             type text (no newline)
//! line <text>             type text then Enter
//! key <name>              enter | tab | escape | up | down | ctrl-c | ctrl-d
//! wait <ms>               pump for a fixed interval
//! snapshot <label>        print the visible screen
//! ```
//!
//! Exists because the interesting states of an agent session — waiting for tool
//! approval, in particular — only occur interactively, and a spike needs to
//! reach them reproducibly rather than by hand.

use std::time::Duration;

use ansible_terminal::{
    GhosttyTerminal, Key, KeyEvent, Modifiers, TerminalBackend, TerminalConfig, TerminalInput,
    TerminalSize,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(90);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let script_path = args.next().ok_or("usage: vt-drive <script> <command> [args...]")?;
    let command = args.next().ok_or("usage: vt-drive <script> <command> [args...]")?;
    let command_args: Vec<String> = args.collect();

    let script = std::fs::read_to_string(&script_path)?;

    let mut term = GhosttyTerminal::spawn(
        TerminalConfig::command(&command, TerminalSize::new(120, 40, 8, 16))
            .args(command_args)
            .env("LC_ALL", "C.UTF-8"),
    )?;

    for (lineno, raw) in script.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));

        match verb {
            "expect" | "expect?" => {
                let needle = rest.to_string();
                let found =
                    term.wait_for_screen(STEP_TIMEOUT, |s| s.screen_text().contains(&needle))?;
                if found {
                    println!("[{lineno}] matched: {needle}");
                } else if verb == "expect" {
                    eprintln!("[{lineno}] TIMEOUT waiting for: {needle}");
                    eprintln!("--- screen ---\n{}", term.snapshot()?.screen_text());
                    term.shutdown()?;
                    std::process::exit(1);
                } else {
                    println!("[{lineno}] absent (optional): {needle}");
                }
            }
            "send" => term.send(TerminalInput::Text(rest.to_string()))?,
            "line" => {
                term.send(TerminalInput::Text(rest.to_string()))?;
                term.pump_until(Duration::from_millis(150), |_| false)?;
                term.send(TerminalInput::Key(KeyEvent::press(Key::Enter, Modifiers::NONE)))?;
            }
            "key" => term.send(key_input(rest)?)?,
            "wait" => {
                let ms: u64 = rest.trim().parse().unwrap_or(500);
                term.pump_until(Duration::from_millis(ms), |_| false)?;
            }
            "snapshot" => {
                println!("--- snapshot {rest} ---");
                println!("{}", term.snapshot()?.screen_text());
                println!("--- end {rest} ---");
            }
            other => return Err(format!("line {lineno}: unknown verb `{other}`").into()),
        }
    }

    term.pump_until(Duration::from_millis(500), |_| false)?;
    println!("--- final screen ---\n{}", term.snapshot()?.screen_text());
    term.shutdown()?;
    Ok(())
}

fn key_input(name: &str) -> Result<TerminalInput, Box<dyn std::error::Error>> {
    let event = match name.trim() {
        "enter" => KeyEvent::press(Key::Enter, Modifiers::NONE),
        "tab" => KeyEvent::press(Key::Tab, Modifiers::NONE),
        "escape" => KeyEvent::press(Key::Escape, Modifiers::NONE),
        "up" => KeyEvent::press(Key::Up, Modifiers::NONE),
        "down" => KeyEvent::press(Key::Down, Modifiers::NONE),
        "ctrl-c" => KeyEvent::press(Key::Char('c'), Modifiers::ctrl()),
        "ctrl-d" => KeyEvent::press(Key::Char('d'), Modifiers::ctrl()),
        other => return Err(format!("unknown key `{other}`").into()),
    };
    Ok(TerminalInput::Key(event))
}
