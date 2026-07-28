//! Spike A harness: a libghostty-vt terminal rendered natively inside a Tauri window.
//!
//! Composition model: the terminal is a GTK `DrawingArea` packed as a *sibling*
//! of the WebKit webview inside the window's default vertical box. It is not
//! overlaid on the webview, so there is no z-order fight, no hit-testing
//! ambiguity, and nothing to re-sync when the webview scrolls.
//!
//! Terminal bytes never enter the webview. Rendering reads libghostty terminal
//! state in Rust; keystrokes go from GTK straight to the PTY.

#![cfg_attr(not(target_os = "linux"), allow(unused))]

#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod renderer;
#[cfg(target_os = "linux")]
mod surface;

fn main() {
    #[cfg(target_os = "linux")]
    {
        if let Err(err) = run() {
            eprintln!("spike-a: {err:#}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "The Spike A harness is Linux-only. libghostty's GUI embedding API \
             accepts an AppKit NSView, so macOS should host a ghostty surface \
             directly rather than reuse this GTK path. See \
             docs/spikes/terminal-embedding.md."
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    use tauri::Manager;

    // Configurable so the spike can run a real `claude` session where
    // credentials exist, and a plain shell where they do not.
    let command = std::env::var("ANSIBLE_TERMINAL_COMMAND")
        .unwrap_or_else(|_| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    let args: Vec<String> = std::env::var("ANSIBLE_TERMINAL_ARGS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    tauri::Builder::default()
        .setup(move |app| {
            let window =
                app.get_webview_window("main").ok_or_else(|| anyhow::anyhow!("no main window"))?;
            surface::attach(&window, &command, &args)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri: {e}"))
}
