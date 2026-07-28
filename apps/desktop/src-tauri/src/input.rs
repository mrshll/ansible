//! GTK key events translated into [`TerminalInput`].
//!
//! Only the *identity* of the key is decided here. The bytes are libghostty's
//! job, because the correct encoding depends on terminal modes the application
//! negotiates at runtime (Kitty keyboard protocol, DECCKM, and so on).

use ansible_terminal::{Key, KeyEvent, Modifiers, TerminalInput};
use gtk::gdk;

pub fn modifiers(state: gdk::ModifierType) -> Modifiers {
    Modifiers {
        shift: state.contains(gdk::ModifierType::SHIFT_MASK),
        ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
        alt: state.contains(gdk::ModifierType::MOD1_MASK),
        super_: state.contains(gdk::ModifierType::SUPER_MASK),
    }
}

/// Translate a GDK key press. Returns `None` for keys with no terminal meaning,
/// such as a bare modifier.
pub fn translate(event: &gdk::EventKey) -> Option<TerminalInput> {
    let mods = modifiers(event.state());
    let keyval = event.keyval();

    if let Some(key) = named_key(keyval) {
        return Some(TerminalInput::Key(KeyEvent::press(key, mods)));
    }

    let unicode = keyval.to_unicode()?;
    if unicode.is_control() {
        return None;
    }

    // Hand the composed character through as `text` as well: libghostty needs
    // it to encode shifted and non-US-layout keys correctly.
    let mut event = KeyEvent::press(Key::Char(unicode), mods);
    if !mods.ctrl && !mods.super_ {
        event = event.with_text(unicode.to_string());
    }
    Some(TerminalInput::Key(event))
}

fn named_key(keyval: gdk::keys::Key) -> Option<Key> {
    use gdk::keys::constants as k;
    Some(match keyval {
        k::Return | k::KP_Enter => Key::Enter,
        k::Tab | k::ISO_Left_Tab => Key::Tab,
        k::BackSpace => Key::Backspace,
        k::Escape => Key::Escape,
        k::Up | k::KP_Up => Key::Up,
        k::Down | k::KP_Down => Key::Down,
        k::Left | k::KP_Left => Key::Left,
        k::Right | k::KP_Right => Key::Right,
        k::Home | k::KP_Home => Key::Home,
        k::End | k::KP_End => Key::End,
        k::Page_Up | k::KP_Page_Up => Key::PageUp,
        k::Page_Down | k::KP_Page_Down => Key::PageDown,
        k::Delete | k::KP_Delete => Key::Delete,
        k::Insert | k::KP_Insert => Key::Insert,
        k::F1 => Key::F(1),
        k::F2 => Key::F(2),
        k::F3 => Key::F(3),
        k::F4 => Key::F(4),
        k::F5 => Key::F(5),
        k::F6 => Key::F(6),
        k::F7 => Key::F(7),
        k::F8 => Key::F(8),
        k::F9 => Key::F(9),
        k::F10 => Key::F(10),
        k::F11 => Key::F(11),
        k::F12 => Key::F(12),
        _ => return None,
    })
}

/// Whether this keystroke is the paste accelerator rather than terminal input.
///
/// Ctrl-Shift-V, not Ctrl-V: in a terminal Ctrl-V is the literal-next quoting
/// key and must reach the child.
pub fn is_paste_shortcut(event: &gdk::EventKey) -> bool {
    let mods = modifiers(event.state());
    mods.ctrl && mods.shift && matches!(event.keyval().to_unicode(), Some('V' | 'v'))
}
