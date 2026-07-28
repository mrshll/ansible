//! The native terminal surface and its wiring into the Tauri window.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use ansible_terminal::{
    GhosttyTerminal, Snapshot, TerminalBackend, TerminalConfig, TerminalEvent, TerminalInput,
    TerminalSize,
};
use anyhow::{Context as _, anyhow};
use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::input;
use crate::renderer::Renderer;

/// Frame tick. 120 Hz keeps input-to-glyph latency dominated by the terminal
/// rather than by the poll interval, while staying cheap when idle.
const TICK: Duration = Duration::from_millis(8);

const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PT: f64 = 13.0;

struct Surface {
    terminal: GhosttyTerminal,
    renderer: Renderer,
    snapshot: Option<Snapshot>,
    /// Set by the frame tick, cleared once a redraw has been queued.
    dirty: bool,
}

/// Pack a terminal surface as a sibling of the webview and start driving it.
pub fn attach(
    window: &tauri::WebviewWindow,
    command: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // `default_vbox` is the GTK box Tauri puts the webview in. Packing here
    // makes the terminal a sibling of the webview rather than an overlay.
    let vbox = window
        .default_vbox()
        .map_err(|e| anyhow!("window has no default vbox ({e}); Tauri layout changed"))?;

    let area = gtk::DrawingArea::new();
    area.set_size_request(-1, 480);
    area.set_can_focus(true);
    area.add_events(
        gdk::EventMask::KEY_PRESS_MASK
            | gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::FOCUS_CHANGE_MASK
            | gdk::EventMask::STRUCTURE_MASK,
    );
    vbox.pack_start(&area, true, true, 0);
    area.show();

    let mut config = TerminalConfig::command(command, TerminalSize::new(80, 24, 8, 16));
    config = config.args(args.iter().cloned());
    if let Some(cwd) = cwd {
        config = config.cwd(cwd);
    }
    let terminal =
        GhosttyTerminal::spawn(&config).with_context(|| format!("spawning `{command}`"))?;

    let surface = Rc::new(RefCell::new(Surface {
        terminal,
        renderer: Renderer::new(FONT_FAMILY, FONT_SIZE_PT),
        snapshot: None,
        dirty: true,
    }));

    connect_draw(&area, &surface);
    connect_input(&area, &surface, window);
    start_tick(&area, &surface);

    area.grab_focus();
    Ok(())
}

fn connect_draw(area: &gtk::DrawingArea, surface: &Rc<RefCell<Surface>>) {
    let surface = surface.clone();
    area.connect_draw(move |area, cr| {
        let width = f64::from(area.allocated_width());
        let height = f64::from(area.allocated_height());
        let mut s = surface.borrow_mut();

        // The widget is the source of truth for geometry: derive the grid from
        // the allocation and push it down, so a window resize becomes a
        // terminal resize and a SIGWINCH.
        let (cols, rows, metrics) = s.renderer.grid_for(cr, width, height);
        let (cell_w, cell_h) = metrics.pixel_size();
        let want = TerminalSize::new(cols, rows, cell_w, cell_h);
        if s.terminal.size() != want {
            let _ = s.terminal.resize(want);
            s.snapshot = None;
        }

        if s.snapshot.is_none() {
            s.snapshot = s.terminal.snapshot().ok();
        }
        if let Some(snapshot) = s.snapshot.take() {
            s.renderer.draw(cr, &snapshot, width, height);
            s.snapshot = Some(snapshot);
        }
        glib::Propagation::Stop
    });
}

fn connect_input(
    area: &gtk::DrawingArea,
    surface: &Rc<RefCell<Surface>>,
    window: &tauri::WebviewWindow,
) {
    // Clicking the terminal focuses it; clicking the webview does not.
    {
        let area_ref = area.clone();
        area.connect_button_press_event(move |_, _| {
            area_ref.grab_focus();
            glib::Propagation::Stop
        });
    }

    {
        let surface = surface.clone();
        let area_ref = area.clone();
        area.connect_key_press_event(move |_, event| {
            if input::is_paste_shortcut(event) {
                if let Some(text) = clipboard_text(&area_ref) {
                    let _ = surface.borrow_mut().terminal.send(TerminalInput::Paste(text));
                }
                return glib::Propagation::Stop;
            }
            if let Some(action) = input::translate(event) {
                let _ = surface.borrow_mut().terminal.send(action);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }

    // Focus reporting: libghostty emits nothing unless the application enabled
    // mode 1004, so this is safe to send unconditionally.
    for (signal_focused, focused) in [(true, true), (false, false)] {
        let surface = surface.clone();
        if signal_focused {
            area.connect_focus_in_event(move |_, _| {
                let _ = surface.borrow_mut().terminal.send(TerminalInput::Focus(focused));
                glib::Propagation::Proceed
            });
        } else {
            area.connect_focus_out_event(move |_, _| {
                let _ = surface.borrow_mut().terminal.send(TerminalInput::Focus(focused));
                glib::Propagation::Proceed
            });
        }
    }

    // Keep the OS window title in step with OSC 0/2 from the child.
    let _ = window;
}

fn start_tick(area: &gtk::DrawingArea, surface: &Rc<RefCell<Surface>>) {
    let surface = surface.clone();
    let area = area.clone();
    glib::timeout_add_local(TICK, move || {
        let mut s = surface.borrow_mut();

        if s.terminal.pump().is_err() {
            return glib::ControlFlow::Break;
        }

        // Raw PTY bytes are drained here and deliberately go nowhere yet: this
        // is the seam Spike B's transcript capture attaches to. They are not
        // part of the rendering path.
        let events: Vec<TerminalEvent> = s.terminal.events().try_iter().collect();
        for event in events {
            match event {
                TerminalEvent::Output(_) => {}
                TerminalEvent::Damage => s.dirty = true,
                TerminalEvent::Title(title) => {
                    if let Some(window) =
                        area.toplevel().and_then(|t| t.downcast::<gtk::Window>().ok())
                    {
                        window.set_title(&title);
                    }
                }
                // Same empty body as `Output` above, deliberately kept separate:
                // that arm is a seam with a consumer coming, this one is a
                // feature (an audible or visual bell) we have not built.
                #[allow(clippy::match_same_arms)]
                TerminalEvent::Bell => {}
                TerminalEvent::Exited(reason) => {
                    eprintln!("spike-a: child exited ({reason})");
                    s.dirty = true;
                }
            }
        }

        if s.dirty {
            s.dirty = false;
            s.snapshot = s.terminal.snapshot().ok();
            drop(s);
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
}

fn clipboard_text(widget: &gtk::DrawingArea) -> Option<String> {
    let display = widget.display();
    let clipboard = gtk::Clipboard::default(&display)?;
    clipboard.wait_for_text().map(|t| t.to_string())
}
